//! Ledger #8 and #13, the KV halves: a backend operation runs inside a cheap
//! tracing span, and a backend failure moves `moso_kv_errors_total`.
//!
//! Both are asserted through the public surface: a hand-written
//! [`tracing::Subscriber`] that records the spans that are opened, and a
//! [`MetricsRecorder`](moso_core::middleware::metrics::MetricsRecorder) installed
//! as the process sink that records the counters that are moved. Neither needs a
//! database, so this file runs on the macOS CI leg with `DATABASE_URL` and
//! `REDIS_URL` unset, like every other in-process KV test.

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use moso_core::middleware::metrics::{self, MetricsRecorder, RequestSample};
use moso_core::{BoxFuture, HealthStatus};
use moso_kv::breaker::BreakerConfig;
use moso_kv::kv::{KV_ERRORS_METRIC, KV_OPERATIONS_METRIC};
use moso_kv::{Capabilities, Error, Key, Kv, KvStore, Result, SetOpts};

moso_kv::namespace! {
    /// A tiny cached string, degrading on failure like any cache.
    pub Thing: u64 => String, ttl = moso_kv::minutes(5);
}

// ---------------------------------------------------------------------------
// A subscriber that records which spans were opened
// ---------------------------------------------------------------------------

/// One opened span: its metadata name and its fields, rendered `key=value;`.
#[derive(Clone, Debug)]
struct OpenedSpan {
    name: &'static str,
    fields: String,
}

/// A `tracing::Subscriber` that keeps every span it is asked to open, so a test
/// can assert what `Kv` emitted without a real collector.
#[derive(Clone, Default)]
struct CaptureSpans {
    spans: Arc<Mutex<Vec<OpenedSpan>>>,
    next_id: Arc<AtomicU64>,
}

/// Renders a span's fields into `key=value;`, quoting through `Debug` so a
/// `&str` value keeps its quotes and cannot be confused with a bare field name.
struct FieldVisitor(String);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        use fmt::Write as _;
        let _ = write!(self.0, "{}={value:?};", field.name());
    }
}

impl tracing::Subscriber for CaptureSpans {
    fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        let mut visitor = FieldVisitor(String::new());
        span.record(&mut visitor);
        self.spans.lock().expect("not poisoned").push(OpenedSpan {
            name: span.metadata().name(),
            fields: visitor.0,
        });
        // Ids must be non-zero and need not be unique for this test's purposes.
        tracing::span::Id::from_u64(self.next_id.fetch_add(1, Ordering::Relaxed) + 1)
    }

    fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
    fn event(&self, _event: &tracing::Event<'_>) {}
    fn enter(&self, _span: &tracing::span::Id) {}
    fn exit(&self, _span: &tracing::span::Id) {}
}

#[test]
fn a_get_runs_inside_a_span_naming_the_operation_and_the_backend() {
    let capture = CaptureSpans::default();
    let spans = Arc::clone(&capture.spans);

    // `with_default` sets the subscriber for this thread, and the current-thread
    // runtime drives the whole future here, so the `debug_span!` in `guarded`
    // fires on this thread and is recorded.
    tracing::subscriber::with_default(capture, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let kv = Kv::in_memory("shop").expect("built");
            assert_eq!(kv.get::<Thing>(&1).await.expect("get"), None);
        });
    });

    let opened = spans.lock().expect("not poisoned");
    assert!(
        opened.iter().any(|span| span.name == "kv.op"
            && span.fields.contains("op=\"get\"")
            && span.fields.contains("backend=\"memory\"")),
        "expected a `kv.op` span for get on the memory backend, got: {:?}",
        opened
            .iter()
            .map(|span| (span.name, span.fields.as_str()))
            .collect::<Vec<_>>(),
    );
}

// ---------------------------------------------------------------------------
// A recorder that records which counters moved
// ---------------------------------------------------------------------------

/// A [`MetricsRecorder`] that sums each named counter, so a test can read the
/// process-wide series a battery increments.
#[derive(Debug, Default)]
struct SeenCounters(Arc<Mutex<HashMap<&'static str, u64>>>);

impl MetricsRecorder for SeenCounters {
    fn record(&self, _sample: &RequestSample<'_>) {}

    fn counter(&self, name: &'static str, by: u64) {
        *self
            .0
            .lock()
            .expect("not poisoned")
            .entry(name)
            .or_default() += by;
    }
}

/// A backend whose `get` always fails, to induce the one thing a real
/// in-process store will not do on demand: a transient backend error. Every
/// other required method answers trivially — the test only reaches `get`.
#[derive(Debug)]
struct FlakyStore;

impl KvStore for FlakyStore {
    fn name(&self) -> &'static str {
        "flaky"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities::none()
    }
    fn health(&self) -> BoxFuture<'_, HealthStatus> {
        Box::pin(async { HealthStatus::Up })
    }
    fn get<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async {
            Err(Error::backend(
                "flaky",
                "get",
                std::io::Error::other("connection reset"),
            ))
        })
    }
    fn set<'a>(&'a self, _k: &'a Key, _v: Bytes, _o: SetOpts) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn delete<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn exists<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn expire<'a>(&'a self, _key: &'a Key, _ttl: Duration) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn ttl<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async { Ok(None) })
    }
    fn incr<'a>(
        &'a self,
        _key: &'a Key,
        by: i64,
        _ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { Ok(by) })
    }
}

#[tokio::test]
async fn a_backend_error_increments_the_errors_counter() {
    let seen = Arc::new(Mutex::new(HashMap::new()));
    metrics::install_process_recorder(Arc::new(SeenCounters(Arc::clone(&seen))));

    // A never-tripping breaker so the failing `get` actually reaches the store,
    // and the default `Degrade` namespace so the failure becomes a miss — the
    // counter moves on the degrade path exactly as the failure policy documents.
    let kv = Kv::builder("shop")
        .store(FlakyStore)
        .breaker(BreakerConfig::never())
        .build()
        .expect("built");

    assert_eq!(
        kv.get::<Thing>(&1).await.expect("degrades to a miss"),
        None,
        "a Degrade namespace turns a backend failure into a miss",
    );

    let counts = seen.lock().expect("not poisoned");
    assert!(
        counts.get(KV_ERRORS_METRIC).copied().unwrap_or(0) >= 1,
        "the backend error moved `{KV_ERRORS_METRIC}`: {counts:?}",
    );
    assert!(
        counts.get(KV_OPERATIONS_METRIC).copied().unwrap_or(0) >= 1,
        "the operation moved `{KV_OPERATIONS_METRIC}`: {counts:?}",
    );
}
