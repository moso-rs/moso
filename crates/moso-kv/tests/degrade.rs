//! Acceptance criterion 5: **a backend outage with `Degrade` namespaces keeps
//! the application serving; with `Fail` namespaces it produces a 503.**
//!
//! # Why this file has a fake in it and the others do not
//!
//! Everywhere else in this crate, a test runs against a real store, because a
//! mocked data layer proves nothing. Here the thing under test *is* the
//! failure policy, and the only way to exercise it is to make a healthy
//! backend fail on demand. [`Outage`] wraps a real [`MemoryStore`] and switches
//! it off; every operation that is not switched off goes to the real store, so
//! what is faked is exactly the outage and nothing else.
//!
//! The alternative — `docker stop`-ing a container mid-test — makes the suite
//! depend on Docker, on a specific container name, and on a timing window, and
//! it can only fail one backend at a time. This is the same assertion, made
//! deterministically.
//!
//! # What is asserted
//!
//! 1. A `Degrade` namespace turns an outage into a cache miss, and the value is
//!    recomputed rather than 503-ing.
//! 2. A `Fail` namespace turns an outage into an error whose HTTP rendering is
//!    a 503 with a `Retry-After`.
//! 3. A programmer error — a decode failure, an unsupported operation — is
//!    **never** degraded away, whatever the namespace says.
//! 4. The circuit breaker opens after the configured run of failures, refuses
//!    without a round trip while it is open, and closes again on recovery.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use bytes::Bytes;
use moso_core::{BoxFuture, HealthStatus};
use moso_kv::backend::MemoryStore;
use moso_kv::breaker::{BreakerConfig, BreakerState};
use moso_kv::{Capabilities, Error, Key, Kv, KvStore, Result, ScanCursor, SetOpts, minutes};

moso_kv::namespace! {
    /// A cache. Losing it costs a database read.
    pub Cached: u64 => Option<String>, ttl = minutes(5);

    /// A session. Losing it logs somebody out, so it fails loudly.
    pub Session: u64 => String, ttl = minutes(60), on_failure = fail;
}

// ---------------------------------------------------------------------------
// The fault injector
// ---------------------------------------------------------------------------

/// A real [`MemoryStore`] with a switch on the front of it.
///
/// While `down` is set every operation returns a transient
/// [`Error::Backend`] — the same shape a Redis with a closed connection
/// produces — without touching the store underneath. Turning it off restores
/// the real store, values and all, which is what a recovered Redis looks like.
struct Outage {
    inner: MemoryStore,
    down: AtomicBool,
    reached: AtomicUsize,
}

impl Outage {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(),
            down: AtomicBool::new(false),
            reached: AtomicUsize::new(0),
        })
    }

    /// Start failing.
    fn go_down(&self) {
        self.down.store(true, Ordering::SeqCst);
    }

    /// Stop failing.
    fn come_back(&self) {
        self.down.store(false, Ordering::SeqCst);
    }

    /// How many operations reached the real store.
    fn reached(&self) -> usize {
        self.reached.load(Ordering::SeqCst)
    }

    /// `Err` while the outage is on; otherwise counts the call through.
    fn gate(&self, operation: &'static str) -> Result<()> {
        if self.down.load(Ordering::SeqCst) {
            return Err(Error::backend(
                "memory",
                operation,
                std::io::Error::other("connection refused"),
            ));
        }
        self.reached.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

impl KvStore for Outage {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn capabilities(&self) -> Capabilities {
        self.inner.capabilities()
    }

    fn health(&self) -> BoxFuture<'_, HealthStatus> {
        Box::pin(async move {
            if self.down.load(Ordering::SeqCst) {
                HealthStatus::Down(String::from("connection refused"))
            } else {
                self.inner.health().await
            }
        })
    }

    fn get<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            self.gate("get")?;
            self.inner.get(key).await
        })
    }

    fn set<'a>(&'a self, key: &'a Key, value: Bytes, opts: SetOpts) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.gate("set")?;
            self.inner.set(key, value, opts).await
        })
    }

    fn delete<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.gate("delete")?;
            self.inner.delete(key).await
        })
    }

    fn exists<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.gate("exists")?;
            self.inner.exists(key).await
        })
    }

    fn expire<'a>(&'a self, key: &'a Key, ttl: Duration) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.gate("expire")?;
            self.inner.expire(key, ttl).await
        })
    }

    fn ttl<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move {
            self.gate("ttl")?;
            self.inner.ttl(key).await
        })
    }

    fn incr<'a>(
        &'a self,
        key: &'a Key,
        by: i64,
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            self.gate("incr")?;
            self.inner.incr(key, by, ttl).await
        })
    }

    fn scan<'a>(
        &'a self,
        prefix: &'a Key,
        cursor: ScanCursor,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Key>, ScanCursor)>> {
        Box::pin(async move {
            self.gate("scan")?;
            self.inner.scan(prefix, cursor, limit).await
        })
    }
}

/// A handle over `store`, with the breaker configured for the test.
fn handle(store: Arc<dyn KvStore>, breaker: BreakerConfig) -> Kv {
    Kv::builder("shop")
        .shared_store(store)
        .breaker(breaker)
        .build()
        .expect("built")
}

// ---------------------------------------------------------------------------
// Degrade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_degrading_namespace_turns_an_outage_into_a_miss() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::never(),
    );

    kv.set::<Cached>(&1, &Some(String::from("warm")))
        .await
        .expect("set");
    assert_eq!(
        kv.get::<Cached>(&1).await.expect("get"),
        Some(Some(String::from("warm")))
    );

    store.go_down();

    // Every read is a miss, and nothing fails.
    assert_eq!(kv.get::<Cached>(&1).await.expect("degraded"), None);
    assert!(!kv.exists::<Cached>(&1).await.expect("degraded"));
    assert_eq!(kv.ttl::<Cached>(&1).await.expect("degraded"), None);
    assert!(!kv.delete::<Cached>(&1).await.expect("degraded"));

    // A write is a no-op rather than an error.
    kv.set::<Cached>(&2, &Some(String::from("x")))
        .await
        .expect("degraded");

    let stats = kv.stats();
    assert!(stats.degraded >= 5, "{stats:?}");
    assert_eq!(stats.errors, 0, "nothing propagated");

    // The value is still there when the store comes back: degrading did not
    // delete anything.
    store.come_back();
    assert_eq!(
        kv.get::<Cached>(&1).await.expect("get"),
        Some(Some(String::from("warm")))
    );
}

#[tokio::test]
async fn a_degrading_read_through_recomputes_rather_than_failing() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::never(),
    );
    let calls = AtomicUsize::new(0);

    store.go_down();

    for _ in 0..3 {
        let value = kv
            .get_or_insert_with::<Cached, _, _>(&1, || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Ok(Some(String::from("from the database")))
            })
            .await
            .expect("the request is served");
        assert_eq!(value, Some(String::from("from the database")));
    }

    // Every call recomputed, because nothing could be cached — which is the
    // point: the application is slower and it is still serving.
    assert_eq!(calls.load(Ordering::SeqCst), 3);
}

// ---------------------------------------------------------------------------
// Fail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failing_namespace_produces_a_503_with_a_retry_hint() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::never(),
    );

    kv.set::<Session>(&1, &String::from("token"))
        .await
        .expect("set");

    store.go_down();

    let error = kv
        .get::<Session>(&1)
        .await
        .expect_err("sessions fail loudly");
    assert!(error.retryable(), "{error}");
    assert!(!error.is_programmer_error(), "{error}");

    let http: moso_core::Error = error.into();
    assert_eq!(http.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(http.is_server_error());

    assert!(kv.set::<Session>(&2, &String::from("x")).await.is_err());
    assert!(kv.delete::<Session>(&1).await.is_err());

    let stats = kv.stats();
    assert!(stats.errors >= 3, "{stats:?}");
    assert_eq!(stats.degraded, 0, "a `fail` namespace never degrades");
}

#[tokio::test]
async fn the_two_modes_coexist_in_one_process() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::never(),
    );
    store.go_down();

    // Same store, same outage, two answers — which is the whole point of a
    // per-namespace failure mode.
    assert_eq!(kv.get::<Cached>(&1).await.expect("degraded"), None);
    assert!(kv.get::<Session>(&1).await.is_err());
}

// ---------------------------------------------------------------------------
// A programmer error is never degraded away
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unsupported_operation_is_not_degraded() {
    /// A store with nothing optional, so `scan` is a programmer error.
    struct Bare(MemoryStore);

    impl KvStore for Bare {
        fn name(&self) -> &'static str {
            "memory"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::none()
        }
        fn health(&self) -> BoxFuture<'_, HealthStatus> {
            self.0.health()
        }
        fn get<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
            self.0.get(key)
        }
        fn set<'a>(
            &'a self,
            key: &'a Key,
            value: Bytes,
            opts: SetOpts,
        ) -> BoxFuture<'a, Result<bool>> {
            self.0.set(key, value, opts)
        }
        fn delete<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
            self.0.delete(key)
        }
        fn exists<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
            self.0.exists(key)
        }
        fn expire<'a>(&'a self, key: &'a Key, ttl: Duration) -> BoxFuture<'a, Result<bool>> {
            self.0.expire(key, ttl)
        }
        fn ttl<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
            self.0.ttl(key)
        }
        fn incr<'a>(
            &'a self,
            key: &'a Key,
            by: i64,
            ttl: Option<Duration>,
        ) -> BoxFuture<'a, Result<i64>> {
            self.0.incr(key, by, ttl)
        }
    }

    let kv = handle(
        Arc::new(Bare(MemoryStore::new())) as Arc<dyn KvStore>,
        BreakerConfig::never(),
    );

    // `Cached` degrades — and still, this propagates, because it is a bug and
    // not an outage.
    let error = kv
        .clear_namespace::<Cached>()
        .await
        .expect_err("scan is unsupported here");
    assert!(error.is_programmer_error(), "{error}");
    assert!(!error.retryable(), "{error}");

    let http: moso_core::Error = error.into();
    assert_eq!(http.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

    assert_eq!(kv.stats().degraded, 0);
    assert!(kv.stats().errors >= 1);
}

#[tokio::test]
async fn bytes_written_by_another_version_are_a_miss_and_never_a_500() {
    // The rolling-deploy case: the old pods wrote a framing this build does not
    // read. Both modes agree here, and both answer "miss", because 500-ing
    // every request until the old pods drain is strictly worse.
    for is_session in [false, true] {
        let kv = Kv::in_memory("shop").expect("built");

        let key = if is_session {
            kv.key::<Session>(&1).expect("short")
        } else {
            kv.key::<Cached>(&1).expect("short")
        };
        kv.store()
            .set(&key, Bytes::from_static(b"not a frame"), SetOpts::new())
            .await
            .expect("set");

        if is_session {
            assert_eq!(kv.get::<Session>(&1).await.expect("a miss"), None);
        } else {
            assert_eq!(kv.get::<Cached>(&1).await.expect("a miss"), None);
        }
        assert_eq!(kv.stats().decode_failures, 1);
        assert_eq!(kv.stats().errors, 0);
    }
}

// ---------------------------------------------------------------------------
// The circuit breaker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_breaker_opens_and_stops_reaching_the_store() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::default()
            .failure_threshold(3)
            .cooldown(Duration::from_secs(30))
            .jitter_percent(0),
    );

    store.go_down();
    let before = store.reached();

    for _ in 0..3 {
        let _ = kv.get::<Cached>(&1).await;
    }
    assert_eq!(kv.breaker().state(), BreakerState::Open);

    // While it is open, nothing is attempted at all.
    for _ in 0..20 {
        assert_eq!(kv.get::<Cached>(&1).await.expect("degraded"), None);
    }
    assert_eq!(
        store.reached(),
        before,
        "the breaker let calls through while it was open"
    );

    // And a `fail` namespace still fails, with a retry hint from the breaker.
    let error = kv.get::<Session>(&1).await.expect_err("open");
    assert!(matches!(error, Error::CircuitOpen { .. }), "{error}");
    assert!(error.retry_after().is_some());

    let http: moso_core::Error = error.into();
    assert_eq!(http.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    assert!(http.headers().expect("headers").contains_key("retry-after"));
}

#[tokio::test]
async fn the_breaker_closes_again_when_the_store_comes_back() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::default()
            .failure_threshold(2)
            .cooldown(Duration::from_millis(80))
            .jitter_percent(0),
    );

    store.go_down();
    for _ in 0..2 {
        let _ = kv.get::<Cached>(&1).await;
    }
    assert_eq!(kv.breaker().state(), BreakerState::Open);

    store.come_back();

    // Before the cooldown, still refused.
    assert_eq!(kv.breaker().state(), BreakerState::Open);

    // After it, one probe goes through and closes the breaker.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(kv.get::<Cached>(&1).await.expect("probe"), None);
    assert_eq!(kv.breaker().state(), BreakerState::Closed);

    // ... and normal service resumes.
    kv.set::<Cached>(&1, &Some(String::from("back")))
        .await
        .expect("set");
    assert_eq!(
        kv.get::<Cached>(&1).await.expect("get"),
        Some(Some(String::from("back")))
    );
}

#[tokio::test]
async fn a_health_check_reports_the_outage_even_while_requests_are_served() {
    let store = Outage::new();
    let kv = handle(
        Arc::clone(&store) as Arc<dyn KvStore>,
        BreakerConfig::never(),
    );
    let check = kv.health_check();

    assert_eq!(check.probe().await, HealthStatus::Up);

    store.go_down();
    match check.probe().await {
        HealthStatus::Down(reason) => assert!(reason.contains("refused"), "{reason}"),
        other => panic!("{other:?}"),
    }

    // The instance is still serving — that is why the check is not critical by
    // default — and the report still says what is wrong.
    assert!(!moso_core::HealthCheck::critical(&check));
    assert_eq!(kv.get::<Cached>(&1).await.expect("degraded"), None);
}
