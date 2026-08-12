//! Graceful shutdown: stop accepting, drain, then leave.
//!
//! ```text
//! SIGTERM
//!   │
//!   ├─▶ /readyz answers 503 within milliseconds — the load balancer removes us
//!   │   while we are still serving what we already accepted
//!   │
//!   ├─▶ the listener stops accepting
//!   │
//!   ├─▶ in-flight requests drain, up to `server.shutdown_grace` (25 s)
//!   │       long-lived handlers select on `Inject<Signal>` and close
//!   │
//!   ├─▶ on_shutdown hooks run in reverse registration order
//!   ├─▶ lifespan guards drop, innermost first
//!   └─▶ tracing exporters flush, then exit 0
//! ```
//!
//! The 25 s grace is under the 30 s an orchestrator typically allows before
//! `SIGKILL`, deliberately: a grace longer than the kill timeout means the
//! process is killed mid-drain, which is the thing the grace existed to prevent.
//!
//! # Long-lived handlers
//!
//! An SSE or WebSocket handler outlives a request and must cooperate:
//!
//! ```
//! use moso::prelude::*;
//! use moso::Signal;
//! use moso::response::NoContent;
//!
//! /// Do slow work, but give up the moment shutdown starts.
//! #[endpoint]
//! async fn export(Inject(signal): Inject<Signal>) -> Result<NoContent> {
//!     loop {
//!         tokio::select! {
//!             () = signal.recv() => break,
//!             () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
//!         }
//!     }
//!     Ok(NoContent)
//! }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let signal = Signal::new();
//! assert!(!signal.is_shutting_down());
//!
//! signal.trigger();
//! signal.recv().await;                 // returns immediately once triggered
//! assert!(signal.is_shutting_down());
//! # }
//! ```
//!
//! A route still open when the grace expires is named in a warning at `WARN`,
//! with its matched path. That log line is how a leaked stream gets found;
//! without it, the symptom is "deploys take 25 seconds" and nobody knows why.
//!
//! # Two counters, not one
//!
//! [`Drain`] keeps an atomic count *and* a registry of the names still open. The
//! count answers "are we done yet" on a hot loop without taking a lock; the
//! registry answers "what is still open" exactly once, at the end of the grace,
//! which is the only moment anybody needs it.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A handle that resolves when shutdown begins.
///
/// Registered as a provider, so any handler, dependency or job can take
/// `Inject<Signal>`. Cloning is a refcount bump.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
/// use moso::Signal;
///
/// /// Work that stops when the process is asked to.
/// #[endpoint]
/// async fn export(Inject(signal): Inject<Signal>) -> Result<NoContent> {
///     tokio::select! {
///         () = signal.recv() => {}
///         () = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
///     }
///     Ok(NoContent)
/// }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() {
/// let signal = Signal::new();
/// assert!(!signal.is_shutting_down());
///
/// signal.trigger();
/// signal.recv().await;   // already triggered, so this returns immediately
/// assert!(signal.is_shutting_down());
/// # }
/// ```
///
/// Every waiter is woken, and a `recv()` after the trigger returns at once — so a
/// handler that starts during the drain does not wait out the grace period.
#[derive(Clone)]
pub struct Signal {
    inner: Arc<SignalInner>,
}

struct SignalInner {
    notify: tokio::sync::Notify,
    shutting_down: AtomicBool,
    /// When `trigger` first ran, so every budget measured from the signal —
    /// the connection drain, then the guard drain — shares one deadline
    /// instead of each starting its own.
    triggered_at: OnceLock<Instant>,
}

impl Signal {
    /// A signal that has not fired.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(SignalInner {
                notify: tokio::sync::Notify::new(),
                shutting_down: AtomicBool::new(false),
                triggered_at: OnceLock::new(),
            }),
        }
    }

    /// Wait for shutdown to begin.
    ///
    /// Returns immediately if it already has, so a task that starts late is not
    /// left waiting for an event that has passed — the classic missed-wakeup
    /// bug in hand-rolled shutdown handling.
    ///
    /// The second flag check is not redundant. [`Notify::notify_waiters`] only
    /// wakes waiters that are *already registered*, so a `trigger` landing
    /// between the first check and registration would be missed. The sequence
    /// is: check, register (`enable`), check again, then park. `trigger` sets
    /// the flag before notifying, so at least one of the two checks sees it.
    ///
    /// [`Notify::notify_waiters`]: tokio::sync::Notify::notify_waiters
    pub async fn recv(&self) {
        if self.is_shutting_down() {
            return;
        }
        let notified = self.inner.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.is_shutting_down() {
            return;
        }
        notified.await;
    }

    /// Whether shutdown has begun.
    ///
    /// For a loop that wants to check between iterations rather than await.
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
    }

    /// Begin shutdown, waking every waiter.
    ///
    /// Idempotent. Called by the signal handler, by `TestApp` on drop, and by
    /// an application that decides to stop itself.
    pub fn trigger(&self) {
        // The timestamp is set before the flag, so anybody who observes the
        // flag can also observe when it was set.
        let _ = self.inner.triggered_at.set(Instant::now());
        self.inner.shutting_down.store(true, Ordering::Release);
        self.inner.notify.notify_waiters();
    }

    /// How long ago shutdown began, or `None` if it has not.
    ///
    /// Everything after the signal shares one budget — the connection drain,
    /// then the guard drain — so each stage subtracts what the last one spent
    /// instead of starting its own `shutdown_grace`. Two stages each taking the
    /// full grace would double the deploy window the grace exists to bound.
    pub fn since_trigger(&self) -> Option<Duration> {
        self.inner.triggered_at.get().map(Instant::elapsed)
    }

    /// What is left of `grace`, measured from the signal.
    ///
    /// `grace` in full before the signal fires, and never negative after it.
    pub fn remaining(&self, grace: Duration) -> Duration {
        match self.since_trigger() {
            Some(elapsed) => grace.saturating_sub(elapsed),
            None => grace,
        }
    }
}

impl Default for Signal {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Signal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Signal")
            .field("shutting_down", &self.is_shutting_down())
            .finish()
    }
}

/// Keeps the drain waiting while it is alive.
///
/// Held by an in-flight request or by a background task that must finish before
/// the process exits. The drain waits for every guard to drop, or for the grace
/// period to expire — whichever comes first.
pub struct ShutdownGuard {
    drain: Arc<DrainInner>,
    id: u64,
    name: &'static str,
}

impl ShutdownGuard {
    /// The name reported if this guard is still held when the grace expires.
    pub fn name(&self) -> &'static str {
        self.name
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        self.drain.close(self.id);
    }
}

impl core::fmt::Debug for ShutdownGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ShutdownGuard")
            .field("name", &self.name)
            .finish()
    }
}

/// Hands out [`ShutdownGuard`]s and waits for them all to drop.
#[derive(Debug, Clone, Default)]
pub struct Drain {
    inner: Arc<DrainInner>,
}

/// The shared state behind every clone of one [`Drain`].
#[derive(Debug, Default)]
struct DrainInner {
    outstanding: AtomicUsize,
    next_id: AtomicU64,
    open: Mutex<BTreeMap<u64, &'static str>>,
}

impl DrainInner {
    /// Register a name and return the id that will deregister it.
    fn open(&self, name: &'static str) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.outstanding.fetch_add(1, Ordering::AcqRel);
        if let Ok(mut open) = self.open.lock() {
            open.insert(id, name);
        }
        id
    }

    /// Deregister an id. Poisoning is ignored: a poisoned registry is a
    /// cosmetic loss (one missing name in one warning), never a reason to
    /// panic a second time on the shutdown path.
    fn close(&self, id: u64) {
        if let Ok(mut open) = self.open.lock() {
            open.remove(&id);
        }
        self.outstanding.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drain {
    /// A drain with nothing outstanding.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take a guard, keeping the drain waiting until it drops.
    pub fn guard(&self, name: &'static str) -> ShutdownGuard {
        let id = self.inner.open(name);
        ShutdownGuard {
            drain: Arc::clone(&self.inner),
            id,
            name,
        }
    }

    /// How many guards are outstanding.
    ///
    /// A load of one atomic. Unlike a refcount on the drain itself, this is
    /// unaffected by how many clones of the `Drain` handle exist — and there is
    /// one per request context.
    pub fn outstanding(&self) -> usize {
        self.inner.outstanding.load(Ordering::Acquire)
    }

    /// The names still held, in the order they were taken.
    ///
    /// Read once, at the end of the grace period, to name what kept the process
    /// alive. Duplicates are kept: three open streams on the same route are
    /// three entries, and the count is the diagnostic.
    pub fn open_names(&self) -> Vec<&'static str> {
        match self.inner.open.lock() {
            Ok(open) => open.values().copied().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Wait for every guard to drop, or for `grace` to expire.
    ///
    /// Returns `true` when the drain completed and `false` when it timed out.
    /// A `false` is followed by a warning naming what was still open, which is
    /// the diagnostic that makes a slow shutdown fixable.
    pub async fn wait(&self, grace: Duration) -> bool {
        /// How often the drain re-checks. Short enough that a fast drain is not
        /// perceptibly delayed, long enough that a 25 s grace is 2500 loads of
        /// one atomic rather than a spin.
        const POLL: Duration = Duration::from_millis(10);

        if self.outstanding() == 0 {
            return true;
        }
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return self.outstanding() == 0;
            }
            tokio::time::sleep(POLL.min(deadline - now)).await;
            if self.outstanding() == 0 {
                return true;
            }
        }
    }
}

/// Wait for `SIGINT` or `SIGTERM`.
///
/// `SIGTERM` is what an orchestrator sends and `SIGINT` is what Ctrl-C sends;
/// both mean the same thing here. On a platform without Unix signals only
/// Ctrl-C is observed.
///
/// A failure to install the `SIGTERM` handler is logged and degraded to
/// Ctrl-C only, rather than aborting: a process that cannot observe `SIGTERM`
/// should still start, and the log line says what was lost.
pub async fn listen_for_signals() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not install a SIGTERM handler; only Ctrl-C will stop this process"
                );
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_signal_has_not_fired() {
        assert!(!Signal::new().is_shutting_down());
    }

    #[test]
    fn guards_are_counted() {
        let drain = Drain::new();
        assert_eq!(drain.outstanding(), 0);
        let guard = drain.guard("request");
        assert_eq!(drain.outstanding(), 1);
        assert_eq!(guard.name(), "request");
        drop(guard);
        assert_eq!(drain.outstanding(), 0);
    }

    #[test]
    fn triggering_is_idempotent() {
        let signal = Signal::new();
        signal.trigger();
        let first = signal.since_trigger().expect("recorded");
        signal.trigger();
        assert!(signal.is_shutting_down());
        // The second trigger does not restart the clock, so the grace period
        // cannot be extended by signalling twice.
        assert!(signal.since_trigger().expect("recorded") >= first);
    }

    #[test]
    fn the_grace_budget_is_shared_by_every_stage() {
        let signal = Signal::new();
        assert!(signal.since_trigger().is_none());
        assert_eq!(
            signal.remaining(Duration::from_secs(25)),
            Duration::from_secs(25)
        );

        signal.trigger();
        assert!(signal.since_trigger().is_some());
        assert!(signal.remaining(Duration::from_secs(25)) <= Duration::from_secs(25));
        // Never negative, however long the first stage took.
        assert_eq!(signal.remaining(Duration::ZERO), Duration::ZERO);
    }

    #[tokio::test]
    async fn recv_returns_immediately_once_triggered() {
        let signal = Signal::new();
        signal.trigger();
        // No timeout wrapper: if this ever blocks, the test hangs and says so.
        signal.recv().await;
    }

    #[tokio::test]
    async fn recv_wakes_every_waiter() {
        let signal = Signal::new();
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let signal = signal.clone();
                tokio::spawn(async move { signal.recv().await })
            })
            .collect();

        // Give the waiters a chance to register before the notification.
        tokio::task::yield_now().await;
        signal.trigger();

        for waiter in waiters {
            waiter.await.expect("waiter did not panic");
        }
    }

    #[tokio::test]
    async fn a_waiter_registered_before_the_trigger_is_not_missed() {
        // The missed-wakeup shape: the future is created, then the signal
        // fires, then the future is first polled.
        let signal = Signal::new();
        let pending = signal.recv();
        signal.trigger();
        pending.await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_drain_completes_at_once() {
        let drain = Drain::new();
        assert!(drain.wait(Duration::from_secs(25)).await);
    }

    #[tokio::test(start_paused = true)]
    async fn a_held_guard_times_the_drain_out() {
        let drain = Drain::new();
        let _guard = drain.guard("GET /events");
        assert!(!drain.wait(Duration::from_millis(50)).await);
        assert_eq!(drain.open_names(), vec!["GET /events"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_guard_dropped_during_the_grace_completes_the_drain() {
        let drain = Drain::new();
        let guard = drain.guard("GET /events");
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(guard);
        });

        assert!(drain.wait(Duration::from_secs(5)).await);
        handle.await.expect("the dropper did not panic");
        assert!(drain.open_names().is_empty());
    }

    #[test]
    fn cloning_a_drain_does_not_inflate_the_count() {
        let drain = Drain::new();
        let clone = drain.clone();
        assert_eq!(drain.outstanding(), 0);

        let guard = clone.guard("request");
        // Both handles see the same one guard.
        assert_eq!(drain.outstanding(), 1);
        assert_eq!(clone.outstanding(), 1);
        drop(guard);
        assert_eq!(drain.outstanding(), 0);
    }

    #[test]
    fn open_names_lists_every_holder() {
        let drain = Drain::new();
        let first = drain.guard("GET /events");
        let second = drain.guard("GET /ws");
        assert_eq!(drain.open_names(), vec!["GET /events", "GET /ws"]);
        drop(first);
        assert_eq!(drain.open_names(), vec!["GET /ws"]);
        drop(second);
        assert!(drain.open_names().is_empty());
    }
}
