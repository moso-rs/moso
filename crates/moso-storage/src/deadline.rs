//! Two deadlines, because "how long may this take" has two different answers.
//!
//! [`StorageConfig::timeout`](crate::StorageConfig::timeout) used to be a
//! number nothing read. Making it real needed a decision that a single
//! `tokio::time::timeout` around every call gets wrong in both directions: 30
//! seconds is far too long for a `head` that should answer in 20 ms, and far
//! too short for a gibibyte moving steadily at 40 MB/s. A whole-operation
//! deadline over a streaming transfer kills healthy downloads; no deadline at
//! all leaves a socket that went quiet holding a connection, a task and a
//! buffer until the process restarts.
//!
//! So there are two, and which one applies is decided by the *shape* of the
//! call rather than by its name:
//!
//! | Shape | Bound by | Restarts on progress | Operations |
//! | --- | --- | --- | --- |
//! | answers once | [`Deadlines::operation`] | no | `head`, `delete`, `delete_many`, `list`, `copy`, `signed_url`, `presigned_upload`, `multipart_start`, `probe`, and each multipart part |
//! | moves bytes | [`Deadlines::idle`] | **yes** | `put`, `get`, `get_range` |
//!
//! The failures are separate values, because they mean different things to
//! whoever reads the log: [`Error::Timeout`](crate::Error::Timeout) is "the
//! store never answered", [`Error::Stalled`](crate::Error::Stalled) is "the
//! store answered and then went quiet halfway through". Both are retryable and
//! both become a 504.
//!
//! # How the idle deadline is measured
//!
//! ```text
//! put:  body ──▶ [ mark progress per chunk ] ──▶ backend
//!                          │
//!                          ╰── watchdog: abandon the whole `put` future when
//!                              no chunk has been pulled for `idle`
//!
//! get:  backend ──▶ [ per-chunk timeout ] ──▶ caller
//!                       └── the *setup* call is bounded by `operation`;
//!                           the body it returns is bounded by `idle`
//! ```
//!
//! A `put` is watched rather than wrapped chunk-by-chunk because the stall can
//! be at either end: a client that stopped sending stops the source, and a
//! backend that stopped reading stops the sink. Both look identical from here —
//! no chunk was pulled — and both are the failure worth naming.
//!
//! Nothing here collects. [`TimedStorage`] hands the same [`ByteStream`]
//! through with a timer attached to it, so the 20 MiB peak-RSS acceptance
//! criterion in `tests/acceptance.rs` is unaffected by wrapping a backend.

use std::future::Future;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moso_core::BoxFuture;
use moso_schema::Url;

use crate::{
    ByteStream, Listing, MultipartUpload, ObjectMeta, PresignedPost, PutOpts, Result, ServedObject,
    Storage, StorageCapabilities, StorageKey, UploadPolicy,
};

// ---------------------------------------------------------------------------
// the policy
// ---------------------------------------------------------------------------

/// How long a storage operation may take.
///
/// `None` for either half means "unbounded", which is what a hand-built
/// backend has until something gives it a policy.
///
/// ```
/// use moso_storage::Deadlines;
/// use std::time::Duration;
///
/// let deadlines = Deadlines::uniform(Duration::from_secs(30));
/// assert_eq!(deadlines.operation(), Some(Duration::from_secs(30)));
/// assert_eq!(deadlines.idle(), Some(Duration::from_secs(30)));
/// assert_eq!(Deadlines::NONE.operation(), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Deadlines {
    /// The whole-operation deadline, for a call that answers once.
    operation: Option<Duration>,
    /// The stall deadline, for a call that moves bytes.
    idle: Option<Duration>,
}

impl Deadlines {
    /// No deadline at all. What a hand-built backend enforces.
    ///
    /// ```
    /// use moso_storage::Deadlines;
    ///
    /// assert_eq!(Deadlines::NONE.idle(), None);
    /// ```
    pub const NONE: Self = Self {
        operation: None,
        idle: None,
    };

    /// The same number for both, which is what one configured `timeout` means.
    ///
    /// ```
    /// use moso_storage::Deadlines;
    /// use std::time::Duration;
    ///
    /// let deadlines = Deadlines::uniform(Duration::from_secs(10));
    /// assert_eq!(deadlines.operation(), deadlines.idle());
    /// ```
    #[must_use]
    pub const fn uniform(timeout: Duration) -> Self {
        Self {
            operation: Some(timeout),
            idle: Some(timeout),
        }
    }

    /// A separate whole-operation deadline and stall deadline.
    ///
    /// Worth splitting when the store is far away: a `head` across an ocean
    /// still answers in under a second, while a transfer over the same link
    /// deserves a much longer quiet period before it is called dead.
    ///
    /// ```
    /// use moso_storage::Deadlines;
    /// use std::time::Duration;
    ///
    /// let deadlines = Deadlines::new(Duration::from_secs(5), Duration::from_secs(60));
    /// assert_eq!(deadlines.operation(), Some(Duration::from_secs(5)));
    /// assert_eq!(deadlines.idle(), Some(Duration::from_secs(60)));
    /// ```
    #[must_use]
    pub const fn new(operation: Duration, idle: Duration) -> Self {
        Self {
            operation: Some(operation),
            idle: Some(idle),
        }
    }

    /// How long a call that answers once may take.
    ///
    /// ```
    /// # use moso_storage::Deadlines;
    /// assert_eq!(Deadlines::NONE.operation(), None);
    /// ```
    #[must_use]
    pub const fn operation(self) -> Option<Duration> {
        self.operation
    }

    /// How long a transfer may move no bytes before it is abandoned.
    ///
    /// ```
    /// # use moso_storage::Deadlines;
    /// # use std::time::Duration;
    /// assert_eq!(Deadlines::uniform(Duration::from_secs(1)).idle(), Some(Duration::from_secs(1)));
    /// ```
    #[must_use]
    pub const fn idle(self) -> Option<Duration> {
        self.idle
    }

    /// Run a call that answers once under the whole-operation deadline.
    pub(crate) async fn unary<T, F>(
        self,
        backend: &'static str,
        operation: &'static str,
        future: F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let Some(limit) = self.operation else {
            return future.await;
        };
        match tokio::time::timeout(limit, future).await {
            Ok(result) => result,
            Err(_elapsed) => Err(crate::Error::timed_out(backend, operation, limit)),
        }
    }

    /// Run a transfer under the stall deadline, restarting it on every chunk.
    ///
    /// The future is polled inside a timeout that is recomputed from the last
    /// recorded progress, so the *total* time is unbounded while bytes keep
    /// moving and bounded by `idle` once they stop.
    async fn progressing<T, F>(
        self,
        backend: &'static str,
        operation: &'static str,
        progress: &Progress,
        future: F,
    ) -> Result<T>
    where
        F: Future<Output = Result<T>>,
    {
        let Some(limit) = self.idle else {
            return future.await;
        };

        let mut future = Box::pin(future);
        loop {
            // `checked_sub` is the stall test: `None` and zero both mean the
            // last chunk is at least `limit` old.
            let remaining = match limit.checked_sub(progress.idle_for()) {
                Some(remaining) if !remaining.is_zero() => remaining,
                _ => return Err(crate::Error::stalled(backend, operation, limit)),
            };
            if let Ok(result) = tokio::time::timeout(remaining, &mut future).await {
                return result;
            }
        }
    }

    /// Attach the stall deadline to a stream the backend produced.
    fn guard(
        self,
        backend: &'static str,
        operation: &'static str,
        stream: ByteStream,
    ) -> ByteStream {
        let Some(limit) = self.idle else {
            return stream;
        };

        // `None` as the state ends the stream, so a stall is terminal rather
        // than a chunk a caller can poll past.
        Box::pin(futures_util::stream::unfold(
            Some(stream),
            move |state| async move {
                use futures_util::StreamExt as _;

                let mut stream = state?;
                match tokio::time::timeout(limit, stream.next()).await {
                    Ok(Some(Ok(chunk))) => Some((Ok(chunk), Some(stream))),
                    Ok(Some(Err(error))) => Some((Err(error), None)),
                    Ok(None) => None,
                    Err(_elapsed) => {
                        Some((Err(crate::Error::stalled(backend, operation, limit)), None))
                    }
                }
            },
        ))
    }
}

impl Default for Deadlines {
    /// [`Deadlines::NONE`]: a value that appeared from nowhere must not start
    /// abandoning transfers.
    fn default() -> Self {
        Self::NONE
    }
}

// ---------------------------------------------------------------------------
// progress
// ---------------------------------------------------------------------------

/// When a transfer last moved a byte.
///
/// Nanoseconds since a fixed `Instant` in an atomic rather than a
/// `Mutex<Instant>`: the writer is on the stream's poll path and runs once per
/// chunk, and a lock there would be a contention point for no reason.
#[derive(Debug)]
pub(crate) struct Progress {
    /// What the recorded offsets are measured from.
    start: Instant,
    /// Nanoseconds after `start` at which the last chunk was seen.
    last: AtomicU64,
}

impl Progress {
    /// A tracker whose clock starts now.
    fn new() -> Self {
        Self {
            start: Instant::now(),
            last: AtomicU64::new(0),
        }
    }

    /// Record that a chunk moved.
    fn mark(&self) {
        self.last
            .store(Self::nanos(self.start.elapsed()), Ordering::Relaxed);
    }

    /// How long since the last chunk moved.
    fn idle_for(&self) -> Duration {
        let now = Self::nanos(self.start.elapsed());
        Duration::from_nanos(now.saturating_sub(self.last.load(Ordering::Relaxed)))
    }

    /// A duration as nanoseconds, saturating rather than wrapping.
    ///
    /// `u64` nanoseconds runs out after 584 years, which no request reaches.
    fn nanos(duration: Duration) -> u64 {
        u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX)
    }
}

/// Mark progress on every chunk a stream yields.
///
/// The stream is handed through, not collected: this is one `inspect` and no
/// buffer, which is what keeps a gibibyte inside the peak-RSS budget.
fn instrument(stream: ByteStream, progress: Arc<Progress>) -> ByteStream {
    use futures_util::StreamExt as _;

    Box::pin(stream.inspect(move |_chunk| progress.mark()))
}

// ---------------------------------------------------------------------------
// the wrapper
// ---------------------------------------------------------------------------

/// Any [`Storage`], with deadlines enforced.
///
/// One implementation rather than five: every shipped backend gets exactly the
/// same rule, and a backend written outside this crate gets it by being wrapped
/// the same way. [`StorageConfig::build`](crate::StorageConfig::build) always
/// returns one of these, so a configured `timeout` is enforced without an
/// application doing anything.
///
/// It is transparent: [`name`](Storage::name) and
/// [`capabilities`](Storage::capabilities) are the inner backend's, so a log
/// line still says `s3` and a capability check still answers for the real
/// store.
///
/// ```
/// use moso_storage::{Deadlines, Storage, TimedStorage, backend::MemoryStorage};
/// use std::time::Duration;
///
/// let storage = TimedStorage::new(MemoryStorage::new(), Deadlines::uniform(Duration::from_secs(5)));
/// assert_eq!(storage.name(), "memory");
/// assert_eq!(storage.deadlines().operation(), Some(Duration::from_secs(5)));
/// ```
pub struct TimedStorage<S: Storage> {
    /// The backend doing the work.
    inner: S,
    /// What it is allowed to take.
    deadlines: Deadlines,
}

impl<S: Storage> TimedStorage<S> {
    /// Enforce `deadlines` on `inner`.
    ///
    /// ```
    /// use moso_storage::{Deadlines, TimedStorage, backend::MemoryStorage};
    /// use std::time::Duration;
    ///
    /// let _ = TimedStorage::new(MemoryStorage::new(), Deadlines::uniform(Duration::from_secs(30)));
    /// ```
    #[must_use]
    pub const fn new(inner: S, deadlines: Deadlines) -> Self {
        Self { inner, deadlines }
    }

    /// The backend underneath, for the operations this crate does not model.
    ///
    /// The escape hatch: `LocalStorage::routes` needs the concrete backend, and
    /// wrapping it must not be the reason an application cannot mount it.
    ///
    /// ```
    /// use moso_storage::{Deadlines, TimedStorage, backend::MemoryStorage};
    ///
    /// let storage = TimedStorage::new(MemoryStorage::new(), Deadlines::NONE);
    /// assert!(storage.inner().is_empty());
    /// ```
    #[must_use]
    pub const fn inner(&self) -> &S {
        &self.inner
    }

    /// Take the backend back out.
    ///
    /// ```
    /// use moso_storage::{Deadlines, TimedStorage, backend::MemoryStorage};
    ///
    /// let storage = TimedStorage::new(MemoryStorage::new(), Deadlines::NONE);
    /// assert!(storage.into_inner().is_empty());
    /// ```
    #[must_use]
    pub fn into_inner(self) -> S {
        self.inner
    }

    /// What this wrapper enforces.
    ///
    /// ```
    /// use moso_storage::{Deadlines, TimedStorage, backend::MemoryStorage};
    ///
    /// let storage = TimedStorage::new(MemoryStorage::new(), Deadlines::NONE);
    /// assert_eq!(storage.deadlines(), Deadlines::NONE);
    /// ```
    #[must_use]
    pub const fn deadlines(&self) -> Deadlines {
        self.deadlines
    }
}

impl<S: Storage> core::fmt::Debug for TimedStorage<S> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TimedStorage")
            .field("backend", &self.inner.name())
            .field("deadlines", &self.deadlines)
            .finish()
    }
}

impl<S: Storage> Storage for TimedStorage<S> {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn capabilities(&self) -> StorageCapabilities {
        self.inner.capabilities()
    }

    fn put<'a>(
        &'a self,
        key: &'a StorageKey,
        body: ByteStream,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(async move {
            // The stall can be at either end — a client that stopped sending or
            // a backend that stopped reading — and both show up as "no chunk was
            // pulled", which is what the watchdog waits for.
            let progress = Arc::new(Progress::new());
            let body = instrument(body, Arc::clone(&progress));
            self.deadlines
                .progressing(
                    self.inner.name(),
                    "put",
                    &progress,
                    self.inner.put(key, body, opts),
                )
                .await
        })
    }

    fn get<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let stream = self
                .deadlines
                .unary(self.inner.name(), "get", self.inner.get(key))
                .await?;
            Ok(self.deadlines.guard(self.inner.name(), "get", stream))
        })
    }

    fn get_range<'a>(
        &'a self,
        key: &'a StorageKey,
        range: Range<u64>,
    ) -> BoxFuture<'a, Result<ByteStream>> {
        Box::pin(async move {
            let stream = self
                .deadlines
                .unary(
                    self.inner.name(),
                    "get_range",
                    self.inner.get_range(key, range),
                )
                .await?;
            Ok(self.deadlines.guard(self.inner.name(), "get_range", stream))
        })
    }

    fn head<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
        Box::pin(
            self.deadlines
                .unary(self.inner.name(), "head", self.inner.head(key)),
        )
    }

    fn delete<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
        Box::pin(
            self.deadlines
                .unary(self.inner.name(), "delete", self.inner.delete(key)),
        )
    }

    fn list<'a>(
        &'a self,
        prefix: &'a str,
        cursor: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Listing>> {
        Box::pin(
            self.deadlines
                .unary(self.inner.name(), "list", self.inner.list(prefix, cursor)),
        )
    }

    fn copy<'a>(
        &'a self,
        from: &'a StorageKey,
        to: &'a StorageKey,
    ) -> BoxFuture<'a, Result<ObjectMeta>> {
        Box::pin(
            self.deadlines
                .unary(self.inner.name(), "copy", self.inner.copy(from, to)),
        )
    }

    fn delete_many<'a>(&'a self, keys: &'a [StorageKey]) -> BoxFuture<'a, Result<u64>> {
        // The whole batch, not one deadline per key: the point of the method is
        // that a backend with a bulk API sends one request, and splitting the
        // deadline would mean timing a request nobody made.
        Box::pin(self.deadlines.unary(
            self.inner.name(),
            "delete_many",
            self.inner.delete_many(keys),
        ))
    }

    fn signed_url<'a>(&'a self, key: &'a StorageKey, ttl: Duration) -> BoxFuture<'a, Result<Url>> {
        Box::pin(self.deadlines.unary(
            self.inner.name(),
            "signed_url",
            self.inner.signed_url(key, ttl),
        ))
    }

    fn presigned_upload<'a>(
        &'a self,
        key: &'a StorageKey,
        policy: UploadPolicy,
    ) -> BoxFuture<'a, Result<PresignedPost>> {
        Box::pin(self.deadlines.unary(
            self.inner.name(),
            "presigned_upload",
            self.inner.presigned_upload(key, policy),
        ))
    }

    fn multipart_start<'a>(
        &'a self,
        key: &'a StorageKey,
        opts: PutOpts,
    ) -> BoxFuture<'a, Result<MultipartUpload>> {
        Box::pin(async move {
            let upload = self
                .deadlines
                .unary(
                    self.inner.name(),
                    "multipart_start",
                    self.inner.multipart_start(key, opts),
                )
                .await?;
            // The session outlives this call, so the policy travels with it.
            Ok(upload.with_deadlines(self.deadlines))
        })
    }

    fn serve<'a>(&'a self, key: &'a StorageKey) -> BoxFuture<'a, Result<ServedObject>> {
        Box::pin(async move {
            let object = self
                .deadlines
                .unary(self.inner.name(), "serve", self.inner.serve(key))
                .await?;
            let (backend, deadlines) = (self.inner.name(), self.deadlines);
            Ok(object.map_body(|body| deadlines.guard(backend, "serve", body)))
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(
            self.deadlines
                .unary(self.inner.name(), "probe", self.inner.probe()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use futures_util::StreamExt as _;

    // ── fixtures ──────────────────────────────────────────────────────────

    /// A backend that reads what it is given and remembers nothing.
    ///
    /// Not a mock of a store: it exists to prove what the *wrapper* does with a
    /// stream, which needs a sink that can take a gibibyte without the machine
    /// noticing. Every method it does not need answers honestly.
    #[derive(Debug, Default)]
    struct Drain {
        /// How many bytes `put` consumed.
        written: std::sync::atomic::AtomicU64,
        /// How long `head` sleeps before answering.
        head_delay: Duration,
    }

    impl Storage for Drain {
        fn name(&self) -> &'static str {
            "drain"
        }

        fn capabilities(&self) -> StorageCapabilities {
            StorageCapabilities::minimal()
        }

        fn put<'a>(
            &'a self,
            key: &'a StorageKey,
            mut body: ByteStream,
            opts: PutOpts,
        ) -> BoxFuture<'a, Result<ObjectMeta>> {
            Box::pin(async move {
                let mut size = 0_u64;
                while let Some(chunk) = body.next().await {
                    size += chunk?.len() as u64;
                }
                self.written.store(size, Ordering::Relaxed);
                Ok(crate::object::meta_from(key, size, &opts, None, None))
            })
        }

        fn get<'a>(&'a self, _: &'a StorageKey) -> BoxFuture<'a, Result<ByteStream>> {
            // One chunk, then silence for longer than any test's patience.
            Box::pin(async move {
                Ok(
                    Box::pin(futures_util::stream::unfold(false, |sent| async move {
                        if !sent {
                            return Some((Ok(bytes::Bytes::from_static(b"first")), true));
                        }
                        tokio::time::sleep(Duration::from_secs(3600)).await;
                        None
                    })) as ByteStream,
                )
            })
        }

        fn get_range<'a>(
            &'a self,
            key: &'a StorageKey,
            _: Range<u64>,
        ) -> BoxFuture<'a, Result<ByteStream>> {
            self.get(key)
        }

        fn head<'a>(&'a self, _: &'a StorageKey) -> BoxFuture<'a, Result<Option<ObjectMeta>>> {
            Box::pin(async move {
                tokio::time::sleep(self.head_delay).await;
                Ok(None)
            })
        }

        fn delete<'a>(&'a self, _: &'a StorageKey) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }

        fn list<'a>(&'a self, _: &'a str, _: Option<&'a str>) -> BoxFuture<'a, Result<Listing>> {
            Box::pin(async {
                Ok(Listing {
                    objects: Vec::new(),
                    prefixes: Vec::new(),
                    cursor: None,
                })
            })
        }

        fn copy<'a>(
            &'a self,
            from: &'a StorageKey,
            _: &'a StorageKey,
        ) -> BoxFuture<'a, Result<ObjectMeta>> {
            Box::pin(async move { Err(crate::Error::not_found(from.as_str())) })
        }
    }

    fn key() -> StorageKey {
        StorageKey::new("bulk/data.bin").expect("valid")
    }

    /// A stream of `chunks` chunks of `size` bytes, `gap` apart.
    fn paced(chunks: usize, size: usize, gap: Duration) -> ByteStream {
        Box::pin(futures_util::stream::unfold(
            0_usize,
            move |sent| async move {
                if sent == chunks {
                    return None;
                }
                if !gap.is_zero() {
                    tokio::time::sleep(gap).await;
                }
                // A fresh allocation per chunk: reusing one would hide a layer that
                // kept a reference to what it was handed.
                Some((Ok(bytes::Bytes::from(vec![0xab_u8; size])), sent + 1))
            },
        ))
    }

    // ── the whole-operation deadline ──────────────────────────────────────

    /// A `head` that never answers is the failure the deadline exists for, and
    /// the error has to name the operation rather than saying "storage".
    #[tokio::test]
    async fn a_unary_call_that_does_not_answer_is_abandoned() {
        let storage = TimedStorage::new(
            Drain {
                head_delay: Duration::from_secs(3600),
                ..Drain::default()
            },
            Deadlines::uniform(Duration::from_millis(50)),
        );

        let error = storage.head(&key()).await.expect_err("the deadline fires");
        assert!(
            matches!(
                error,
                crate::Error::Timeout {
                    operation: "head",
                    ..
                }
            ),
            "{error}",
        );
        assert!(error.retryable());
    }

    /// Every unary operation is covered, or the one that is not is the one that
    /// hangs in production.
    #[tokio::test]
    async fn every_unary_operation_is_covered() {
        let storage = TimedStorage::new(
            Drain {
                head_delay: Duration::from_secs(3600),
                ..Drain::default()
            },
            Deadlines::uniform(Duration::from_millis(20)),
        );

        // `head` is the slow one; the rest answer instantly and must *not*
        // fail, which is the other half of the check.
        assert!(storage.head(&key()).await.is_err());
        assert!(!storage.delete(&key()).await.expect("answers"));
        assert!(
            storage
                .list("", None)
                .await
                .expect("answers")
                .objects
                .is_empty()
        );
        assert!(storage.probe().await.is_ok());
        assert_eq!(storage.delete_many(&[key()]).await.expect("answers"), 0);
    }

    /// With no policy the wrapper is a pass-through, which is what makes
    /// `Deadlines::NONE` an honest default rather than a hidden 0-second one.
    #[tokio::test]
    async fn no_deadline_means_no_deadline() {
        let storage = TimedStorage::new(Drain::default(), Deadlines::NONE);
        assert!(storage.head(&key()).await.expect("answers").is_none());
    }

    // ── the stall deadline ────────────────────────────────────────────────

    /// The property the whole module exists for: a transfer that keeps moving
    /// finishes, however long it takes, and a 50 ms whole-operation deadline
    /// does not touch it.
    #[tokio::test]
    async fn a_transfer_that_keeps_moving_outlives_the_whole_operation_deadline() {
        let storage = TimedStorage::new(
            Drain::default(),
            Deadlines::new(Duration::from_millis(50), Duration::from_millis(500)),
        );

        // Twenty chunks 10 ms apart is 200 ms of steady progress — four times
        // the whole-operation deadline, and well inside the stall deadline.
        let meta = storage
            .put(
                &key(),
                paced(20, 1024, Duration::from_millis(10)),
                PutOpts::new("application/octet-stream").trust_content_type(),
            )
            .await
            .expect("steady progress is not a timeout");
        assert_eq!(meta.size, 20 * 1024);
    }

    /// A gibibyte crosses the wrapper without being collected. If `TimedStorage`
    /// buffered, this would need a gibibyte of memory; it needs one chunk.
    #[tokio::test]
    async fn a_gibibyte_crosses_the_wrapper_a_chunk_at_a_time() {
        /// 256 KiB, which is roughly what a real socket delivers.
        const CHUNK: usize = 256 * 1024;
        /// A gibibyte, the size the acceptance criterion names.
        const TOTAL: u64 = 1024 * 1024 * 1024;

        let storage = TimedStorage::new(
            Drain::default(),
            Deadlines::new(Duration::from_millis(10), Duration::from_secs(30)),
        );

        let chunks = (TOTAL / CHUNK as u64) as usize;
        let meta = storage
            .put(
                &key(),
                paced(chunks, CHUNK, Duration::ZERO),
                PutOpts::new("application/octet-stream").trust_content_type(),
            )
            .await
            .expect("a gibibyte of steady progress is not a timeout");
        assert_eq!(meta.size, TOTAL);
    }

    /// A body that stops arriving must not hold the connection open forever,
    /// and the failure must say "stalled" rather than "timed out" so a log
    /// reader knows the transfer had started.
    #[tokio::test]
    async fn a_body_that_stops_arriving_is_abandoned() {
        let storage = TimedStorage::new(
            Drain::default(),
            Deadlines::new(Duration::from_secs(3600), Duration::from_millis(50)),
        );

        // One chunk, then silence.
        let body: ByteStream = Box::pin(futures_util::stream::unfold(false, |sent| async move {
            if !sent {
                return Some((Ok(bytes::Bytes::from_static(b"first")), true));
            }
            tokio::time::sleep(Duration::from_secs(3600)).await;
            None
        }));

        let error = storage
            .put(
                &key(),
                body,
                PutOpts::new("application/octet-stream").trust_content_type(),
            )
            .await
            .expect_err("the stall deadline fires");
        assert!(
            matches!(
                error,
                crate::Error::Stalled {
                    operation: "put",
                    ..
                }
            ),
            "{error}",
        );
    }

    /// The download half: the first chunk arrives, the second never does, and
    /// the stream ends with an error rather than hanging.
    #[tokio::test]
    async fn a_download_that_goes_quiet_ends_with_an_error() {
        let storage = TimedStorage::new(
            Drain::default(),
            Deadlines::new(Duration::from_secs(3600), Duration::from_millis(50)),
        );

        let mut stream = storage.get(&key()).await.expect("the read starts");
        assert_eq!(
            stream.next().await.expect("a chunk").expect("no error"),
            bytes::Bytes::from_static(b"first"),
        );

        let error = stream
            .next()
            .await
            .expect("a second item")
            .expect_err("the stall deadline fires");
        assert!(
            matches!(
                error,
                crate::Error::Stalled {
                    operation: "get",
                    ..
                }
            ),
            "{error}",
        );
        assert!(stream.next().await.is_none(), "a stall is terminal");
    }

    /// `serve` reads metadata and then a body, so both deadlines apply to it —
    /// and the body it hands back is the guarded one.
    #[tokio::test]
    async fn a_served_body_carries_the_stall_deadline() {
        let storage = TimedStorage::new(
            crate::backend::MemoryStorage::new(),
            Deadlines::new(Duration::from_millis(50), Duration::from_millis(50)),
        );
        storage
            .put(
                &key(),
                crate::stream_from_bytes(bytes::Bytes::from_static(b"payload")),
                PutOpts::new("application/octet-stream").trust_content_type(),
            )
            .await
            .expect("stores");

        let object = storage.serve(&key()).await.expect("serves");
        assert_eq!(object.meta().size, 7);
        assert_eq!(object.status(), http::StatusCode::OK);
    }

    // ── transparency ──────────────────────────────────────────────────────

    /// A wrapped backend must still look like itself, or a capability check and
    /// every log line start lying.
    #[tokio::test]
    async fn the_wrapper_is_transparent() {
        let storage = TimedStorage::new(
            crate::backend::MemoryStorage::new(),
            Deadlines::uniform(Duration::from_secs(1)),
        );

        assert_eq!(storage.name(), "memory");
        assert!(!storage.capabilities().signed_urls);
        assert!(matches!(
            storage
                .signed_url(&key(), Duration::from_secs(60))
                .await
                .expect_err("memory cannot sign"),
            crate::Error::Unsupported { .. },
        ));
        assert!(storage.inner().is_empty());
    }
}
