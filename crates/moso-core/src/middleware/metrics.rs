//! `metrics` — count requests without exploding the backend.
//!
//! The slot is innermost, which is the whole point: it runs **after** routing,
//! so the `route` label is `/users/{id}` and not one time series per user id.
//! Labelling with the raw path is the classic cardinality explosion, and it is
//! a production incident rather than a tidiness matter.
//!
//! Moso does not depend on a metrics facade. The slot takes a
//! [`MetricsRecorder`] — one method, dyn-compatible — and an exporter crate
//! implements it in twenty lines. That keeps the choice of backend out of the
//! core's dependency tree, which is where a choice that every application has
//! an opinion about belongs.
//!
//! # The cardinality guard
//!
//! Even a pattern label can run away: a router with a wildcard fallback, an
//! Axum mount Moso cannot see, a stack installed outside routing. The layer
//! therefore caps the number of distinct route labels it will ever emit at
//! [`MetricsConfig::max_routes`], folds everything past the cap into
//! [`OTHER_ROUTE`], and says so **once**. A capped metric is a metric you can
//! still query; an uncapped one takes the Prometheus with it.
//!
//! # Metrics that are not requests
//!
//! A battery needs to count things the request path never sees: an audit entry
//! that could not be written, a cache lookup that errored. The recorder that the
//! slot already carries is the natural sink for those too, but a battery holds no
//! [`MetricsRecorder`] and must not depend on a metrics facade to obtain one.
//!
//! So the recorder is also reachable **process-wide**: [`install_process_recorder`]
//! publishes the app's recorder once at boot, and any crate then increments a
//! named series with [`counter`] / [`gauge`] without threading a handle anywhere.
//! This is a documented process-wide exception, exactly as [`requests_total`] and
//! [`in_flight`] are — a metric describes the *process*, and an exporter reads it
//! from outside any request. The same cardinality rule applies with no router to
//! enforce it: **a counter or gauge name is `&'static str` and its label set must
//! be bounded** — never fold a user id, a path or a request id into the name, for
//! the identical reason the `route` label is the pattern and not the raw path.
//! [`MetricsRecorder::counter`] and [`MetricsRecorder::gauge`] default to no-ops,
//! so an exporter opts in and an old recorder keeps compiling.

use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http::{Method, StatusCode};
use tower::Service;

use crate::middleware::{UNMATCHED_ROUTE, matched_route};
use crate::router::Route;
use crate::{BoxFuture, Request, Response};

/// The label every route past the cardinality cap is folded into.
pub const OTHER_ROUTE: &str = "<other>";

/// The request counter's name.
pub const REQUESTS_METRIC: &str = "moso_http_requests_total";

/// The request-duration histogram's name.
pub const DURATION_METRIC: &str = "moso_http_request_duration_seconds";

/// The in-flight gauge's name.
pub const IN_FLIGHT_METRIC: &str = "moso_http_in_flight";

/// One completed request, as the recorder sees it.
///
/// Borrowed rather than owned: the layer builds one of these per request and
/// hands it out for the duration of the call, so a recorder that only needs a
/// counter increment costs no allocation at all.
#[derive(Debug, Clone, Copy)]
pub struct RequestSample<'a> {
    /// The HTTP method.
    pub method: &'a Method,
    /// The matched route **pattern**, or [`UNMATCHED_ROUTE`], or
    /// [`OTHER_ROUTE`] once the cardinality cap is reached. Never a raw path.
    pub route: &'a str,
    /// The response status.
    pub status: StatusCode,
    /// How long the inner stack took.
    pub duration: Duration,
    /// Requests in flight at the moment this one finished, including itself.
    pub in_flight: i64,
}

/// Where metrics go.
///
/// One **required** method — [`record`](MetricsRecorder::record) — so an exporter
/// is a small adapter rather than an integration. Two more,
/// [`counter`](MetricsRecorder::counter) and [`gauge`](MetricsRecorder::gauge),
/// default to no-ops: they carry the non-request series a battery emits through
/// [`counter`] / [`gauge`], and an exporter that only cares about requests can
/// ignore them.
///
/// ```
/// use moso_core::middleware::metrics::{MetricsRecorder, RequestSample};
/// use std::sync::Mutex;
///
/// /// Keeps every sample, so a test can assert on them.
/// #[derive(Default)]
/// pub struct Collected(Mutex<Vec<String>>);
///
/// impl MetricsRecorder for Collected {
///     fn record(&self, sample: &RequestSample<'_>) {
///         self.0.lock().unwrap().push(format!(
///             "{} {} {}",
///             sample.method.as_str(),
///             sample.route,
///             sample.status.as_u16(),
///         ));
///     }
/// }
/// ```
///
/// A real exporter increments a counter here — `counter!("moso_http_requests_total",
/// "route" => sample.route).increment(1)` — and does no I/O: `record` runs on
/// the request's own task.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a metrics recorder",
    label = "not a `MetricsRecorder`",
    note = "help: implement `MetricsRecorder` for `{Self}`; it has one required \
            method, `record(&self, sample: &RequestSample<'_>)`"
)]
pub trait MetricsRecorder: Send + Sync + 'static {
    /// Record one completed request.
    ///
    /// Called on the request's own task, so it must not block. An exporter that
    /// needs to do I/O should push onto a channel here and do the work
    /// elsewhere.
    fn record(&self, sample: &RequestSample<'_>);

    /// Add `by` to the named counter.
    ///
    /// Called from [`counter`] when a battery increments a monotonic series that
    /// is not tied to a request. The default is a no-op, so an exporter opts in
    /// and a recorder written before this method existed keeps compiling. `name`
    /// is `&'static str` and its label set must be bounded — see the module's
    /// *Metrics that are not requests* section.
    fn counter(&self, name: &'static str, by: u64) {
        let _ = (name, by);
    }

    /// Set the named gauge to `value`.
    ///
    /// Called from [`gauge`] for a series that can go up or down — a queue depth,
    /// a pool size. The default is a no-op, for the same reason
    /// [`counter`](MetricsRecorder::counter) is.
    fn gauge(&self, name: &'static str, value: f64) {
        let _ = (name, value);
    }
}

/// How the `metrics` slot behaves.
#[derive(Clone, Default)]
pub struct MetricsConfig {
    /// Where samples go. `None` leaves the slot disabled.
    pub recorder: Option<Arc<dyn MetricsRecorder>>,
    /// The largest number of distinct `route` labels ever emitted.
    ///
    /// 2000 by default, which is far more routes than an application has and
    /// far fewer series than a backend falls over at.
    pub max_routes: usize,
}

impl core::fmt::Debug for MetricsConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MetricsConfig")
            .field("recorder", &self.recorder.is_some())
            .field("max_routes", &self.max_routes)
            .finish()
    }
}

/// The documented cap on distinct route labels.
pub const DEFAULT_MAX_ROUTES: usize = 2000;

impl MetricsConfig {
    /// Record into `recorder`.
    pub fn new(recorder: Arc<dyn MetricsRecorder>) -> Self {
        Self {
            recorder: Some(recorder),
            max_routes: DEFAULT_MAX_ROUTES,
        }
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        match &self.recorder {
            Some(_) => format!("max_routes={}", self.effective_max_routes()),
            None => "no recorder".to_owned(),
        }
    }

    /// The cap, with `0` — which `Default` produces — read as the documented
    /// default rather than as "emit nothing".
    fn effective_max_routes(&self) -> usize {
        if self.max_routes == 0 {
            DEFAULT_MAX_ROUTES
        } else {
            self.max_routes
        }
    }
}

/// Process-wide request count, readable with [`requests_total`].
static REQUESTS: AtomicU64 = AtomicU64::new(0);

/// Process-wide in-flight count, readable with [`in_flight`].
static IN_FLIGHT: AtomicI64 = AtomicI64::new(0);

/// How many requests this process has completed through the metrics slot.
pub fn requests_total() -> u64 {
    REQUESTS.load(Ordering::Relaxed)
}

/// How many requests are in the metrics slot right now.
pub fn in_flight() -> i64 {
    IN_FLIGHT.load(Ordering::Relaxed)
}

// ---------------------------------------------------------------------------
// Non-request metrics
// ---------------------------------------------------------------------------

/// The process-wide recorder that [`counter`] and [`gauge`] fan out to.
///
/// One slot per process, published by [`install_process_recorder`] and replaced
/// last-wins if two `App`s are built in one process. A `RwLock` rather than an
/// atomic because the value is an `Arc<dyn …>` and not a machine word; it is read
/// on the increment path and written only at boot, so contention is nil.
static PROCESS_RECORDER: RwLock<Option<Arc<dyn MetricsRecorder>>> = RwLock::new(None);

/// Publish `recorder` as the sink for every non-request [`counter`] / [`gauge`].
///
/// Called once at boot — [`layer`] does it automatically for the recorder in
/// [`MetricsConfig`], so an application that configured metrics has already wired
/// its batteries' counters to the same backend. A battery never calls this; it
/// only reads the sink through [`counter`] / [`gauge`].
///
/// This is a documented process-wide singleton, the same exception
/// [`requests_total`] is: two `App`s in one process share one metrics sink, which
/// is the honest reading — the series describe the process, not an `App` value.
///
/// ```
/// use moso_core::middleware::metrics::{self, MetricsRecorder, RequestSample};
/// use std::sync::{Arc, Mutex};
///
/// /// A recorder that only cares about non-request counters.
/// #[derive(Default)]
/// struct Errors(Mutex<u64>);
/// impl MetricsRecorder for Errors {
///     fn record(&self, _sample: &RequestSample<'_>) {}
///     fn counter(&self, _name: &'static str, by: u64) {
///         *self.0.lock().unwrap() += by;
///     }
/// }
///
/// let sink = Arc::new(Errors::default());
/// metrics::install_process_recorder(sink.clone());
/// metrics::counter("moso_kv_errors_total").increment(1);
/// assert_eq!(*sink.0.lock().unwrap(), 1);
/// ```
pub fn install_process_recorder(recorder: Arc<dyn MetricsRecorder>) {
    *PROCESS_RECORDER
        .write()
        .unwrap_or_else(PoisonError::into_inner) = Some(recorder);
}

/// The installed recorder, if any, cloned out from under the read lock.
fn process_recorder() -> Option<Arc<dyn MetricsRecorder>> {
    PROCESS_RECORDER
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

/// A handle to the named counter, as a battery increments it.
///
/// Cheap to build and hold nothing but the name: every increment re-reads the
/// installed recorder, so a handle taken before boot still reaches the recorder
/// installed after. `name` is `&'static str` and its label set must be bounded —
/// never build a counter name out of per-request data.
#[derive(Debug, Clone, Copy)]
pub struct Counter {
    name: &'static str,
}

impl Counter {
    /// Add `by` to this counter, through the installed recorder.
    ///
    /// A no-op — not an error — when no recorder is installed: a library that
    /// counts something must not fall over because the binary chose not to export
    /// metrics.
    pub fn increment(&self, by: u64) {
        if let Some(recorder) = process_recorder() {
            recorder.counter(self.name, by);
        }
    }
}

/// A handle to the named gauge, as a battery sets it.
///
/// The gauge counterpart of [`Counter`]; the same bounded-name rule applies.
#[derive(Debug, Clone, Copy)]
pub struct Gauge {
    name: &'static str,
}

impl Gauge {
    /// Set this gauge to `value`, through the installed recorder.
    ///
    /// A no-op when no recorder is installed, for the reason
    /// [`Counter::increment`] is.
    pub fn set(&self, value: f64) {
        if let Some(recorder) = process_recorder() {
            recorder.gauge(self.name, value);
        }
    }
}

/// A handle to the counter named `name`, ready to [`increment`](Counter::increment).
///
/// The battery-facing half of the recorder: it fans out to whatever
/// [`install_process_recorder`] published, so a crate counts a series without
/// depending on a metrics facade or holding an `Arc<dyn MetricsRecorder>`.
///
/// ```
/// use moso_core::middleware::metrics::{self, MetricsRecorder, RequestSample};
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Default)]
/// struct Seen(Mutex<Vec<(&'static str, u64)>>);
/// impl MetricsRecorder for Seen {
///     fn record(&self, _sample: &RequestSample<'_>) {}
///     fn counter(&self, name: &'static str, by: u64) {
///         self.0.lock().unwrap().push((name, by));
///     }
/// }
///
/// let sink = Arc::new(Seen::default());
/// metrics::install_process_recorder(sink.clone());
/// metrics::counter("moso_authz_audit_dropped").increment(3);
/// assert_eq!(sink.0.lock().unwrap().as_slice(), &[("moso_authz_audit_dropped", 3)]);
/// ```
#[must_use]
pub fn counter(name: &'static str) -> Counter {
    Counter { name }
}

/// A handle to the gauge named `name`, ready to [`set`](Gauge::set).
///
/// The gauge sibling of [`counter`]; see it for the fan-out and bounded-name
/// rule.
///
/// ```
/// use moso_core::middleware::metrics::{self, MetricsRecorder, RequestSample};
/// use std::sync::{Arc, Mutex};
///
/// #[derive(Default)]
/// struct Depth(Mutex<f64>);
/// impl MetricsRecorder for Depth {
///     fn record(&self, _sample: &RequestSample<'_>) {}
///     fn gauge(&self, _name: &'static str, value: f64) {
///         *self.0.lock().unwrap() = value;
///     }
/// }
///
/// let sink = Arc::new(Depth::default());
/// metrics::install_process_recorder(sink.clone());
/// metrics::gauge("moso_jobs_queue_depth").set(12.0);
/// assert_eq!(*sink.0.lock().unwrap(), 12.0);
/// ```
#[must_use]
pub fn gauge(name: &'static str) -> Gauge {
    Gauge { name }
}

/// Wrap `service` in the metrics recorder.
///
/// A configuration with no recorder passes the service through untouched: the
/// gauge and the counters are only worth their atomics when something reads
/// them.
pub fn layer(config: &MetricsConfig, silent: Arc<[String]>, service: Route) -> Route {
    let Some(recorder) = config.recorder.clone() else {
        return service;
    };
    // The configured recorder is also the process-wide sink, so a battery's
    // `counter()` reaches the same backend the request metrics do.
    install_process_recorder(Arc::clone(&recorder));
    Route::new(Metrics {
        inner: service,
        recorder,
        labels: Arc::new(RouteLabels::new(config.effective_max_routes())),
        silent,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct Metrics {
    inner: Route,
    recorder: Arc<dyn MetricsRecorder>,
    labels: Arc<RouteLabels>,
    silent: Arc<[String]>,
}

impl Service<Request> for Metrics {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let recorder = Arc::clone(&self.recorder);
        let labels = Arc::clone(&self.labels);
        let silent = Arc::clone(&self.silent);

        let method = req.method().clone();
        let path = req.uri().path().to_owned();
        // The pattern, resolved here because the extensions go with the
        // request and this is the last chance to read them.
        let route = labels.label(matched_route(req.extensions()).unwrap_or(UNMATCHED_ROUTE));

        Box::pin(async move {
            let quiet = silent
                .iter()
                .any(|prefix| !prefix.is_empty() && path.starts_with(prefix.as_str()));
            if quiet {
                return inner.call(req).await;
            }

            let started = Instant::now();
            IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
            let response = inner.call(req).await;
            let live = IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            REQUESTS.fetch_add(1, Ordering::Relaxed);

            let response = response?;
            recorder.record(&RequestSample {
                method: &method,
                route: &route,
                status: response.status(),
                duration: started.elapsed(),
                in_flight: live,
            });
            Ok(response)
        })
    }
}

/// The bounded set of route labels this stack will emit.
struct RouteLabels {
    max: usize,
    seen: Mutex<HashSet<String>>,
    warned: AtomicBool,
}

impl RouteLabels {
    /// A guard admitting at most `max` distinct labels.
    fn new(max: usize) -> Self {
        Self {
            max,
            seen: Mutex::new(HashSet::new()),
            warned: AtomicBool::new(false),
        }
    }

    /// The label to use for `route`, admitting it if there is room.
    ///
    /// The lock is held for a hash lookup and never across an await, which is
    /// why a plain `Mutex` is the right one here.
    fn label(&self, route: &str) -> String {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if seen.contains(route) {
            return route.to_owned();
        }
        if seen.len() < self.max {
            seen.insert(route.to_owned());
            return route.to_owned();
        }
        drop(seen);

        // Once, not once per request: a cardinality incident must not become a
        // logging incident too.
        if !self.warned.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                metric = REQUESTS_METRIC,
                max_routes = self.max,
                "route label cardinality cap reached; further routes are recorded as `{}`",
                OTHER_ROUTE
            );
        }
        OTHER_ROUTE.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt as _;

    #[derive(Clone, Default)]
    struct Collect {
        samples: Arc<Mutex<Vec<(String, String, u16)>>>,
    }

    impl MetricsRecorder for Collect {
        fn record(&self, sample: &RequestSample<'_>) {
            self.samples.lock().expect("lock").push((
                sample.method.to_string(),
                sample.route.to_owned(),
                sample.status.as_u16(),
            ));
        }
    }

    fn ok_route() -> Route {
        Route::new(tower::service_fn(|_req: Request| async {
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }))
    }

    #[test]
    fn the_slot_is_off_without_a_recorder() {
        let config = MetricsConfig::default();
        assert!(config.recorder.is_none());
        assert_eq!(config.summary(), "no recorder");
    }

    #[test]
    fn a_zero_cap_reads_as_the_default() {
        let config = MetricsConfig {
            recorder: Some(Arc::new(Collect::default())),
            max_routes: 0,
        };
        assert_eq!(config.effective_max_routes(), DEFAULT_MAX_ROUTES);
        assert_eq!(config.summary(), "max_routes=2000");
    }

    #[test]
    fn the_cap_folds_the_tail_into_one_label() {
        let labels = RouteLabels::new(2);
        assert_eq!(labels.label("/a"), "/a");
        assert_eq!(labels.label("/b"), "/b");
        // Already admitted, so it keeps its own label.
        assert_eq!(labels.label("/a"), "/a");
        assert_eq!(labels.label("/c"), OTHER_ROUTE);
        assert_eq!(labels.label("/d"), OTHER_ROUTE);
    }

    /// Acceptance criterion: hitting `/users/1` and `/users/2` produces one
    /// series, because the label is the pattern.
    #[tokio::test]
    async fn two_paths_of_one_route_produce_one_label() {
        let collect = Collect::default();
        let service = layer(
            &MetricsConfig::new(Arc::new(collect.clone())),
            Arc::from(Vec::new()),
            ok_route(),
        );
        let router = axum::Router::new().route_service("/users/{id}", service);

        for path in ["/users/1", "/users/2"] {
            let request = http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .expect("request");
            router
                .clone()
                .into_service::<axum::body::Body>()
                .oneshot(request)
                .await
                .expect("infallible");
        }

        let samples = collect.samples.lock().expect("lock").clone();
        assert_eq!(samples.len(), 2);
        assert_eq!(samples[0].1, "/users/{id}");
        assert_eq!(samples[1].1, "/users/{id}");
        assert_eq!(samples[0].2, 200);
    }

    #[tokio::test]
    async fn a_stack_outside_routing_folds_into_one_bounded_label() {
        let collect = Collect::default();
        let service = layer(
            &MetricsConfig::new(Arc::new(collect.clone())),
            Arc::from(Vec::new()),
            ok_route(),
        );

        for path in ["/a/1", "/a/2", "/a/3"] {
            let request = http::Request::builder()
                .uri(path)
                .body(axum::body::Body::empty())
                .expect("request");
            service.clone().oneshot(request).await.expect("infallible");
        }

        let samples = collect.samples.lock().expect("lock").clone();
        assert_eq!(samples.len(), 3);
        assert!(samples.iter().all(|sample| sample.1 == UNMATCHED_ROUTE));
    }

    #[tokio::test]
    async fn a_silenced_path_is_not_recorded() {
        let collect = Collect::default();
        let service = layer(
            &MetricsConfig::new(Arc::new(collect.clone())),
            Arc::from(vec!["/healthz".to_owned()]),
            ok_route(),
        );

        let request = http::Request::builder()
            .uri("/healthz")
            .body(axum::body::Body::empty())
            .expect("request");
        service.oneshot(request).await.expect("infallible");

        assert!(collect.samples.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn the_in_flight_gauge_returns_to_where_it_started() {
        let before = in_flight();
        let collect = Collect::default();
        layer(
            &MetricsConfig::new(Arc::new(collect)),
            Arc::from(Vec::new()),
            ok_route(),
        )
        .oneshot(Request::new(axum::body::Body::empty()))
        .await
        .expect("infallible");
        assert_eq!(in_flight(), before);
    }

    #[tokio::test]
    async fn no_recorder_means_no_layer() {
        let service = layer(&MetricsConfig::default(), Arc::from(Vec::new()), ok_route());
        let response = service
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
    }

    // ── Non-request metrics ──────────────────────────────────────────────────

    /// Captures the non-request series a battery emits, so a test can assert on
    /// them. Deliberately implements `counter` and `gauge` but leaves `record`
    /// empty — the opposite specialisation to `Collect`.
    #[derive(Default)]
    struct Meter {
        counters: Mutex<Vec<(&'static str, u64)>>,
        gauges: Mutex<Vec<(&'static str, f64)>>,
    }

    impl MetricsRecorder for Meter {
        fn record(&self, _sample: &RequestSample<'_>) {}

        fn counter(&self, name: &'static str, by: u64) {
            self.counters.lock().expect("lock").push((name, by));
        }

        fn gauge(&self, name: &'static str, value: f64) {
            self.gauges.lock().expect("lock").push((name, value));
        }
    }

    #[test]
    fn a_counter_increment_reaches_the_installed_recorder() {
        let meter = Arc::new(Meter::default());
        install_process_recorder(meter.clone());

        counter("moso_kv_errors_total").increment(2);
        counter("moso_kv_errors_total").increment(3);
        counter("moso_authz_audit_dropped").increment(1);

        assert_eq!(
            meter.counters.lock().expect("lock").as_slice(),
            &[
                ("moso_kv_errors_total", 2),
                ("moso_kv_errors_total", 3),
                ("moso_authz_audit_dropped", 1),
            ]
        );
    }

    #[test]
    fn a_gauge_set_reaches_the_installed_recorder() {
        let meter = Arc::new(Meter::default());
        install_process_recorder(meter.clone());

        gauge("moso_jobs_queue_depth").set(7.0);
        gauge("moso_jobs_queue_depth").set(4.0);

        assert_eq!(
            meter.gauges.lock().expect("lock").as_slice(),
            &[
                ("moso_jobs_queue_depth", 7.0),
                ("moso_jobs_queue_depth", 4.0)
            ]
        );
    }

    #[test]
    fn a_recorder_without_the_optional_methods_is_a_silent_noop() {
        // `Collect` implements only `record`, so the defaulted `counter`/`gauge`
        // run — and do nothing, rather than failing to compile or panicking.
        let collect = Collect::default();
        install_process_recorder(Arc::new(collect.clone()));

        counter("moso_kv_errors_total").increment(9);
        gauge("moso_jobs_queue_depth").set(1.0);

        assert!(collect.samples.lock().expect("lock").is_empty());
    }

    #[test]
    fn the_configured_recorder_becomes_the_process_sink() {
        // Building the layer wires a battery's `counter()` to the same recorder
        // the request path uses, with no separate install call.
        let meter = Arc::new(Meter::default());
        let _service = layer(
            &MetricsConfig::new(meter.clone()),
            Arc::from(Vec::new()),
            ok_route(),
        );

        counter("moso_kv_errors_total").increment(5);

        assert_eq!(
            meter.counters.lock().expect("lock").as_slice(),
            &[("moso_kv_errors_total", 5)]
        );
    }
}
