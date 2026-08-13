//! Distributed locks: [`Kv::lock`], [`LockGuard`], and an honest warning.
//!
//! # Read this before using one for correctness
//!
//! **This is not safe as a correctness mechanism across a Redis failover.** It
//! is "Redlock-lite": a single-instance lock with a fencing token, an
//! auto-renewed lease, and release on drop. If the Redis primary fails over to
//! a replica that had not yet received the `SET`, two processes can hold the
//! same lock at the same time. No amount of clever retrying fixes that; it is a
//! property of an asynchronously-replicated store.
//!
//! Use it for what it is good at:
//!
//! * **stopping duplicate work** — one importer, one nightly report, one cache
//!   warm — where doing it twice is wasteful rather than wrong;
//! * **leader election** where a split brain is tolerated by the thing being
//!   led.
//!
//! When two holders would be a *bug*, use PostgreSQL advisory locks
//! (`pg_advisory_lock`) through `moso-orm`'s `Db`, or make the operation
//! idempotent and skip the lock. Being honest about this matters more than the
//! feature does.
//!
//! # The fencing token
//!
//! Every acquisition gets a strictly increasing `i64` from a counter beside the
//! lock. Pass it to whatever the lock protects and have that thing reject a
//! token lower than the highest it has seen. That is the only construction that
//! survives a paused process: a holder that stalls past its lease and wakes up
//! still believing it holds the lock will present a stale token, and be
//! refused.
//!
//! ```
//! use moso_kv::{Kv, Result};
//! use std::time::Duration;
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
//! let kv = Kv::in_memory("shop")?;
//!
//! let guard = kv.lock("import:acme", Duration::from_secs(30)).await?;
//! assert!(guard.token() > 0);
//!
//! // Somebody else cannot take it while it is held.
//! assert!(kv.try_lock("import:acme", Duration::from_secs(30)).await?.is_none());
//!
//! drop(guard);
//! # Ok(())
//! # }
//! ```

use std::time::Duration;

use bytes::Bytes;

use crate::error::{Error, Result};
use crate::key::{Key, KeyBuf};
use crate::kv::Kv;
use crate::store::SetOpts;

/// The namespace segment locks live under.
///
/// ```
/// use moso_kv::lock::LOCK_PREFIX;
///
/// assert_eq!(LOCK_PREFIX, "lock");
/// ```
pub const LOCK_PREFIX: &str = "lock";

/// The namespace segment the fencing counters live under.
///
/// ```
/// use moso_kv::lock::FENCE_PREFIX;
///
/// assert_eq!(FENCE_PREFIX, "lock_fence");
/// ```
pub const FENCE_PREFIX: &str = "lock_fence";

/// The layout version of the lock keys.
const LOCK_VERSION: u16 = 1;

/// The smallest gap between two renewals, whatever the lease.
const MIN_RENEW_INTERVAL: Duration = Duration::from_millis(50);

// ---------------------------------------------------------------------------
// LockOptions
// ---------------------------------------------------------------------------

/// How to take a lock.
///
/// ```
/// use moso_kv::LockOptions;
/// use std::time::Duration;
///
/// let opts = LockOptions::new(Duration::from_secs(30))
///     .wait(Duration::from_secs(5))
///     .no_renew();
///
/// assert_eq!(opts.lease, Duration::from_secs(30));
/// assert_eq!(opts.wait, Duration::from_secs(5));
/// assert!(!opts.auto_renew);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct LockOptions {
    /// How long the lock survives without renewal.
    ///
    /// This is the *only* bound on how long a crashed holder blocks everybody
    /// else, so it should be a small multiple of the expected work, not a
    /// generous margin.
    pub lease: Duration,

    /// How long to keep trying before giving up. [`Duration::ZERO`] means one
    /// attempt.
    pub wait: Duration,

    /// How long to sleep between attempts.
    pub retry_interval: Duration,

    /// Whether to renew the lease in the background while the guard is held.
    pub auto_renew: bool,
}

impl LockOptions {
    /// The defaults for a lease of `lease`: one attempt, auto-renewal on.
    ///
    /// ```
    /// use moso_kv::LockOptions;
    /// use std::time::Duration;
    ///
    /// let opts = LockOptions::new(Duration::from_secs(30));
    /// assert_eq!(opts.wait, Duration::ZERO);
    /// assert!(opts.auto_renew);
    /// ```
    #[must_use]
    pub const fn new(lease: Duration) -> Self {
        Self {
            lease,
            wait: Duration::ZERO,
            retry_interval: Duration::from_millis(50),
            auto_renew: true,
        }
    }

    /// Keep trying for up to `wait`.
    ///
    /// ```
    /// use moso_kv::LockOptions;
    /// use std::time::Duration;
    ///
    /// let opts = LockOptions::new(Duration::from_secs(1)).wait(Duration::from_secs(3));
    /// assert_eq!(opts.wait, Duration::from_secs(3));
    /// ```
    #[must_use]
    pub const fn wait(mut self, wait: Duration) -> Self {
        self.wait = wait;
        self
    }

    /// Sleep `interval` between attempts.
    ///
    /// ```
    /// use moso_kv::LockOptions;
    /// use std::time::Duration;
    ///
    /// let opts = LockOptions::new(Duration::from_secs(1))
    ///     .retry_interval(Duration::from_millis(10));
    /// assert_eq!(opts.retry_interval, Duration::from_millis(10));
    /// ```
    #[must_use]
    pub const fn retry_interval(mut self, interval: Duration) -> Self {
        self.retry_interval = interval;
        self
    }

    /// Do not renew the lease.
    ///
    /// For work whose duration is genuinely bounded by the lease, where an
    /// overrun should lose the lock rather than extend it indefinitely.
    ///
    /// ```
    /// use moso_kv::LockOptions;
    /// use std::time::Duration;
    ///
    /// assert!(!LockOptions::new(Duration::from_secs(1)).no_renew().auto_renew);
    /// ```
    #[must_use]
    pub const fn no_renew(mut self) -> Self {
        self.auto_renew = false;
        self
    }

    /// How often the renewer runs: a third of the lease, floored.
    ///
    /// A third gives two chances to renew before the lease lapses, which
    /// survives one lost round trip.
    ///
    /// ```
    /// use moso_kv::LockOptions;
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     LockOptions::new(Duration::from_secs(30)).renew_interval(),
    ///     Duration::from_secs(10),
    /// );
    /// // Never faster than 50 ms, however short the lease.
    /// assert_eq!(
    ///     LockOptions::new(Duration::from_millis(30)).renew_interval(),
    ///     Duration::from_millis(50),
    /// );
    /// ```
    #[must_use]
    pub fn renew_interval(&self) -> Duration {
        (self.lease / 3).max(MIN_RENEW_INTERVAL)
    }
}

// ---------------------------------------------------------------------------
// LockGuard
// ---------------------------------------------------------------------------

/// A held lock. Releases on drop.
///
/// # The release is asynchronous, and that is visible
///
/// `Drop` is not `async`, so dropping a guard *spawns* the release rather than
/// awaiting it. The lock is free a moment later, not at the closing brace.
/// Three consequences worth knowing:
///
/// * [`lock`](Kv::lock) waits, so the next acquisition still succeeds — it
///   just may take a retry interval.
/// * [`try_lock`](Kv::try_lock) does not wait, so it can see a lock that a
///   guard dropped microseconds ago as still held.
/// * [`release`](Self::release) is the spelling for "I need it back now", and
///   it is also the one that reports a lost lease.
///
/// ```
/// use moso_kv::{Kv, Result};
/// use std::time::Duration;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
/// let kv = Kv::in_memory("shop")?;
/// {
///     let guard = kv.lock("nightly", Duration::from_secs(10)).await?;
///     assert_eq!(guard.name(), "nightly");
/// }
///
/// // `lock` waits for the release the drop spawned.
/// let regained = kv.lock("nightly", Duration::from_secs(10)).await?;
/// assert_eq!(regained.name(), "nightly");
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct LockGuard {
    kv: Kv,
    name: String,
    key: Key,
    token: i64,
    lease: Duration,
    renewer: Option<tokio::task::JoinHandle<()>>,
    released: bool,
}

impl LockGuard {
    /// The name this lock was taken under.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import", Duration::from_secs(5)).await?;
    /// assert_eq!(guard.name(), "import");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The fencing token: strictly greater than every token handed out before
    /// it for this lock.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    ///
    /// let first = kv.lock("job", Duration::from_secs(5)).await?;
    /// let first_token = first.token();
    /// // `release` rather than `drop`: dropping is correct but asynchronous,
    /// // and the next acquisition would race it.
    /// first.release().await?;
    ///
    /// let second = kv.lock("job", Duration::from_secs(5)).await?;
    /// assert!(second.token() > first_token);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn token(&self) -> i64 {
        self.token
    }

    /// The lease this lock was taken with.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import", Duration::from_secs(5)).await?;
    /// assert_eq!(guard.lease(), Duration::from_secs(5));
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn lease(&self) -> Duration {
        self.lease
    }

    /// The key the lock occupies, for a test or a debugging session.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import", Duration::from_secs(5)).await?;
    /// assert_eq!(guard.key().as_str(), "moso:v1:shop:lock:1:import");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// Whether this guard still holds the lock.
    ///
    /// `false` after the lease lapsed and somebody else took it — which
    /// auto-renewal makes unlikely and cannot make impossible, because a
    /// process that is not scheduled cannot renew.
    ///
    /// # Errors
    ///
    /// A transient backend failure, subject to the `Degrade`/`Fail` policy of
    /// the internal lock namespace, which is `Fail`.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import", Duration::from_secs(5)).await?;
    /// assert!(guard.is_held().await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn is_held(&self) -> Result<bool> {
        let held = self.kv.store().get(&self.key).await?;
        Ok(held.as_deref() == Some(token_bytes(self.token).as_ref()))
    }

    /// Extend the lease now, rather than waiting for the renewer.
    ///
    /// # Errors
    ///
    /// [`Error::LockLost`] when the lock is no longer ours, and a transient
    /// backend failure otherwise.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import", Duration::from_secs(5)).await?;
    /// guard.renew().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn renew(&self) -> Result<()> {
        let renewed = self
            .kv
            .store()
            .compare_and_expire(&self.key, &token_bytes(self.token), self.lease)
            .await?;
        if renewed {
            Ok(())
        } else {
            Err(Error::LockLost {
                name: self.name.clone(),
            })
        }
    }

    /// Release now, rather than on drop, and say whether it was still ours.
    ///
    /// Prefer this at the end of a fallible operation: it reports a lost lease,
    /// where [`Drop`] can only log one.
    ///
    /// # Errors
    ///
    /// A transient backend failure.
    ///
    /// ```
    /// # use moso_kv::{Kv, Result};
    /// # use std::time::Duration;
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import", Duration::from_secs(5)).await?;
    /// assert!(guard.release().await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn release(mut self) -> Result<bool> {
        self.stop_renewer();
        self.released = true;
        self.kv
            .store()
            .compare_and_delete(&self.key, &token_bytes(self.token))
            .await
    }

    /// Stop the background renewer, if there is one.
    fn stop_renewer(&mut self) {
        if let Some(handle) = self.renewer.take() {
            handle.abort();
        }
    }
}

impl Drop for LockGuard {
    /// Releases the lock.
    ///
    /// The release is a compare-and-delete against this guard's own token, so
    /// a guard whose lease already lapsed cannot remove the *next* holder's
    /// lock. It is spawned rather than awaited, because `Drop` is not `async`;
    /// outside a Tokio runtime there is nowhere to spawn it and the lease is
    /// what releases the lock instead.
    fn drop(&mut self) {
        self.stop_renewer();
        if self.released {
            return;
        }

        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            tracing::debug!(
                lock = %self.name,
                "no Tokio runtime at drop; the lock will release when its lease expires"
            );
            return;
        };

        let kv = self.kv.clone();
        let key = self.key.clone();
        let token = self.token;
        let name = self.name.clone();
        handle.spawn(async move {
            if let Err(error) = kv
                .store()
                .compare_and_delete(&key, &token_bytes(token))
                .await
            {
                tracing::warn!(
                    lock = %name,
                    error = %error,
                    "releasing a lock failed; it will release when its lease expires"
                );
            }
        });
    }
}

/// The token, as the bytes stored under the lock key.
fn token_bytes(token: i64) -> Bytes {
    Bytes::from(token.to_string().into_bytes())
}

// ---------------------------------------------------------------------------
// Kv::lock
// ---------------------------------------------------------------------------

impl Kv {
    /// Take a lock, waiting for it if somebody else has it.
    ///
    /// Waits up to the lease for the current holder to finish, which is the
    /// only wait that can succeed: a holder that has not finished within its
    /// own lease has lost the lock anyway.
    ///
    /// # Errors
    ///
    /// [`Error::LockHeld`] when the wait runs out, and a backend failure
    /// otherwise. Locks never degrade — silently proceeding without a lock is
    /// the failure the lock exists to prevent.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv.lock("import:acme", Duration::from_secs(30)).await?;
    /// assert_eq!(guard.name(), "import:acme");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn lock(&self, name: &str, lease: Duration) -> Result<LockGuard> {
        self.lock_with(name, LockOptions::new(lease).wait(lease))
            .await
    }

    /// Take a lock, or return `None` immediately.
    ///
    /// # Errors
    ///
    /// A backend failure. A lock that is held is `Ok(None)`, not an error.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let first = kv.try_lock("nightly", Duration::from_secs(10)).await?;
    /// assert!(first.is_some());
    /// assert!(kv.try_lock("nightly", Duration::from_secs(10)).await?.is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn try_lock(&self, name: &str, lease: Duration) -> Result<Option<LockGuard>> {
        match self.lock_with(name, LockOptions::new(lease)).await {
            Ok(guard) => Ok(Some(guard)),
            Err(Error::LockHeld { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// Take a lock with explicit options.
    ///
    /// # Errors
    ///
    /// [`Error::LockHeld`] when [`LockOptions::wait`] runs out, and a backend
    /// failure otherwise.
    ///
    /// ```
    /// use moso_kv::{Kv, LockOptions, Result};
    /// use std::time::Duration;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let guard = kv
    ///     .lock_with(
    ///         "batch",
    ///         LockOptions::new(Duration::from_secs(60)).no_renew(),
    ///     )
    ///     .await?;
    /// assert_eq!(guard.lease(), Duration::from_secs(60));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn lock_with(&self, name: &str, options: LockOptions) -> Result<LockGuard> {
        use tracing::Instrument as _;

        // One cheap span over the whole acquisition — the fencing `incr`, the
        // `set NX` and any retries — so a lock shows up in a request trace as a
        // single `op="lock"` unit rather than as scattered raw store calls.
        self.lock_with_inner(name, options)
            .instrument(self.op_span("lock"))
            .await
    }

    async fn lock_with_inner(&self, name: &str, options: LockOptions) -> Result<LockGuard> {
        let key = self.lock_key(name)?;
        let fence = self.fence_key(name)?;

        // The fencing token comes first and unconditionally: it must increase
        // even for an attempt that fails, so that two contenders can never be
        // handed the same number.
        let token = self.store().incr(&fence, 1, None).await?;
        let token_value = token_bytes(token);

        let opts = SetOpts::new().if_absent().ttl(options.lease);
        let deadline = std::time::Instant::now() + options.wait;

        loop {
            if self.store().set(&key, token_value.clone(), opts).await? {
                let renewer = if options.auto_renew {
                    Some(spawn_renewer(
                        self.clone(),
                        key.clone(),
                        token,
                        options.lease,
                        options.renew_interval(),
                        name.to_owned(),
                    ))
                } else {
                    None
                };

                return Ok(LockGuard {
                    kv: self.clone(),
                    name: name.to_owned(),
                    key,
                    token,
                    lease: options.lease,
                    renewer,
                    released: false,
                });
            }

            if std::time::Instant::now() >= deadline {
                let retry_after = self
                    .store()
                    .ttl(&key)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(options.retry_interval);
                return Err(Error::LockHeld {
                    name: name.to_owned(),
                    retry_after,
                });
            }
            tokio::time::sleep(options.retry_interval).await;
        }
    }

    /// The key a lock named `name` occupies.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when the resulting key is too long.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.lock_key("import").expect("short").as_str(), "moso:v1:shop:lock:1:import");
    /// ```
    pub fn lock_key(&self, name: &str) -> Result<Key> {
        let mut buf = KeyBuf::new(self.app(), LOCK_PREFIX, LOCK_VERSION)?;
        buf.segment_str(name);
        Ok(buf.finish()?)
    }

    /// The key the fencing counter for `name` occupies.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when the resulting key is too long.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(
    ///     kv.fence_key("import").expect("short").as_str(),
    ///     "moso:v1:shop:lock_fence:1:import",
    /// );
    /// ```
    pub fn fence_key(&self, name: &str) -> Result<Key> {
        let mut buf = KeyBuf::new(self.app(), FENCE_PREFIX, LOCK_VERSION)?;
        buf.segment_str(name);
        Ok(buf.finish()?)
    }
}

/// The background task that keeps a lease alive.
fn spawn_renewer(
    kv: Kv,
    key: Key,
    token: i64,
    lease: Duration,
    interval: Duration,
    name: String,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let value = token_bytes(token);
        loop {
            tokio::time::sleep(interval).await;
            match kv.store().compare_and_expire(&key, &value, lease).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        lock = %name,
                        token,
                        "the lease lapsed and the lock is no longer held; stopping renewal"
                    );
                    return;
                }
                Err(error) => {
                    // Keep trying: one failed renewal still leaves two more
                    // chances before the lease runs out.
                    tracing::warn!(lock = %name, error = %error, "renewing a lock lease failed");
                }
            }
        }
    })
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use super::*;

    fn kv() -> Kv {
        Kv::in_memory("shop").expect("built")
    }

    #[tokio::test]
    async fn a_lock_excludes_and_then_releases_on_drop() {
        let kv = kv();
        {
            let guard = kv
                .lock_with("job", LockOptions::new(Duration::from_secs(30)))
                .await
                .expect("acquired");
            assert!(guard.is_held().await.expect("held"));
            assert!(
                kv.try_lock("job", Duration::from_secs(30))
                    .await
                    .expect("try")
                    .is_none()
            );
        }

        // Drop spawns the release, so give the runtime a turn.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if kv
                .try_lock("job", Duration::from_secs(30))
                .await
                .expect("try")
                .is_some()
            {
                return;
            }
        }
        panic!("the lock never released");
    }

    #[tokio::test]
    async fn an_explicit_release_is_immediate_and_reports() {
        let kv = kv();
        let guard = kv
            .lock_with("job", LockOptions::new(Duration::from_secs(30)))
            .await
            .expect("acquired");
        assert!(guard.release().await.expect("released"));
        assert!(
            kv.try_lock("job", Duration::from_secs(30))
                .await
                .expect("try")
                .is_some()
        );
    }

    #[tokio::test]
    async fn the_fencing_token_strictly_increases() {
        let kv = kv();
        let mut previous = 0;
        for _ in 0..5 {
            let guard = kv
                .lock_with("job", LockOptions::new(Duration::from_secs(30)))
                .await
                .expect("acquired");
            assert!(guard.token() > previous, "{} <= {previous}", guard.token());
            previous = guard.token();
            guard.release().await.expect("released");
        }
    }

    #[tokio::test]
    async fn a_token_is_burned_even_by_a_failed_attempt() {
        let kv = kv();
        let held = kv
            .lock_with("job", LockOptions::new(Duration::from_secs(30)))
            .await
            .expect("acquired");

        assert!(
            kv.try_lock("job", Duration::from_secs(30))
                .await
                .expect("try")
                .is_none()
        );

        let after = held.token();
        held.release().await.expect("released");
        let next = kv
            .lock_with("job", LockOptions::new(Duration::from_secs(30)))
            .await
            .expect("acquired");
        assert!(
            next.token() > after + 1,
            "the failed attempt burned a token"
        );
    }

    #[tokio::test]
    async fn a_lock_carries_the_lease_as_its_ttl_so_a_dead_holder_frees_it() {
        let kv = kv();
        let guard = kv
            .lock_with(
                "job",
                LockOptions::new(Duration::from_millis(60)).no_renew(),
            )
            .await
            .expect("acquired");

        let ttl = kv
            .store()
            .ttl(guard.key())
            .await
            .expect("ttl")
            .expect("a ttl");
        assert!(ttl <= Duration::from_millis(60));

        // Forget the guard, which is what a crashed process amounts to.
        std::mem::forget(guard);
        tokio::time::sleep(Duration::from_millis(90)).await;
        assert!(
            kv.try_lock("job", Duration::from_secs(1))
                .await
                .expect("try")
                .is_some(),
            "the lease did not free the lock"
        );
    }

    #[tokio::test]
    async fn auto_renewal_keeps_a_lock_past_its_lease() {
        let kv = kv();
        let guard = kv
            .lock_with("job", LockOptions::new(Duration::from_millis(150)))
            .await
            .expect("acquired");

        // Three leases' worth: without renewal this would have lapsed twice.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(guard.is_held().await.expect("held"));
        assert!(
            kv.try_lock("job", Duration::from_millis(150))
                .await
                .expect("try")
                .is_none()
        );
    }

    #[tokio::test]
    async fn without_renewal_a_lock_lapses() {
        let kv = kv();
        let guard = kv
            .lock_with(
                "job",
                LockOptions::new(Duration::from_millis(60)).no_renew(),
            )
            .await
            .expect("acquired");
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!guard.is_held().await.expect("held"));
        assert!(guard.renew().await.is_err(), "a lapsed lease cannot renew");
    }

    #[tokio::test]
    async fn a_lapsed_holder_cannot_release_the_next_holders_lock() {
        let kv = kv();
        let stale = kv
            .lock_with(
                "job",
                LockOptions::new(Duration::from_millis(50)).no_renew(),
            )
            .await
            .expect("acquired");
        tokio::time::sleep(Duration::from_millis(80)).await;

        let fresh = kv
            .lock_with("job", LockOptions::new(Duration::from_secs(30)))
            .await
            .expect("acquired");

        // The stale guard's release is a no-op ...
        assert!(!stale.release().await.expect("released"));
        // ... and the fresh holder still has it.
        assert!(fresh.is_held().await.expect("held"));
    }

    #[tokio::test]
    async fn waiting_gives_up_with_a_retry_hint() {
        let kv = kv();
        let _guard = kv
            .lock_with("job", LockOptions::new(Duration::from_secs(30)))
            .await
            .expect("acquired");

        let error = kv
            .lock_with(
                "job",
                LockOptions::new(Duration::from_secs(30))
                    .wait(Duration::from_millis(30))
                    .retry_interval(Duration::from_millis(10)),
            )
            .await
            .expect_err("held elsewhere");

        match error {
            Error::LockHeld { name, retry_after } => {
                assert_eq!(name, "job");
                assert!(retry_after > Duration::ZERO);
            }
            other => panic!("{other}"),
        }
    }

    #[tokio::test]
    async fn waiting_succeeds_once_the_holder_lets_go() {
        let kv = kv();
        let guard = kv
            .lock_with("job", LockOptions::new(Duration::from_secs(30)))
            .await
            .expect("acquired");

        let releaser = {
            let kv = kv.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(40)).await;
                guard.release().await.expect("released");
                drop(kv);
            })
        };

        let waited = kv
            .lock_with(
                "job",
                LockOptions::new(Duration::from_secs(30))
                    .wait(Duration::from_secs(2))
                    .retry_interval(Duration::from_millis(10)),
            )
            .await
            .expect("acquired after waiting");
        assert!(waited.is_held().await.expect("held"));
        releaser.await.expect("joined");
    }

    #[tokio::test]
    async fn a_panic_still_releases_the_lock() {
        let kv = kv();
        let handle = {
            let kv = kv.clone();
            tokio::spawn(async move {
                let _guard = kv
                    .lock_with("job", LockOptions::new(Duration::from_secs(30)))
                    .await
                    .expect("acquired");
                panic!("work failed");
            })
        };
        assert!(handle.await.is_err(), "the task panicked");

        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(5)).await;
            if kv
                .try_lock("job", Duration::from_secs(30))
                .await
                .expect("try")
                .is_some()
            {
                return;
            }
        }
        panic!("a panicking holder did not release its lock");
    }

    #[tokio::test]
    async fn lock_names_are_escaped_like_every_other_key_part() {
        let kv = kv();
        assert_eq!(
            kv.lock_key("a:b").expect("short").as_str(),
            "moso:v1:shop:lock:1:a\\cb"
        );
        assert_eq!(
            kv.fence_key("a:b").expect("short").as_str(),
            "moso:v1:shop:lock_fence:1:a\\cb"
        );
    }

    #[test]
    fn the_renew_interval_is_a_third_of_the_lease_with_a_floor() {
        assert_eq!(
            LockOptions::new(Duration::from_secs(30)).renew_interval(),
            Duration::from_secs(10)
        );
        assert_eq!(
            LockOptions::new(Duration::from_millis(9)).renew_interval(),
            MIN_RENEW_INTERVAL
        );
    }

    #[test]
    fn the_options_builder_says_what_it_means() {
        let opts = LockOptions::new(Duration::from_secs(1))
            .wait(Duration::from_secs(2))
            .retry_interval(Duration::from_millis(7))
            .no_renew();
        assert_eq!(opts.lease, Duration::from_secs(1));
        assert_eq!(opts.wait, Duration::from_secs(2));
        assert_eq!(opts.retry_interval, Duration::from_millis(7));
        assert!(!opts.auto_renew);
    }
}
