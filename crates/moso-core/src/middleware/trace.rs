//! `trace` — one span per request, and nothing else.
//!
//! This slot opens the span every later event inherits and records the two
//! facts that are only known at the end, `status` and `duration_ms`. It does
//! **not** log: that is [`catch_error`](crate::middleware::catch_error)'s job,
//! and it does it exactly once. A layer that logged on entry and on exit would
//! double every line in the system and make "one log line per request"
//! impossible to hold.
//!
//! # Why not `tower_http::trace::TraceLayer`
//!
//! `TraceLayer` is a fine layer, and it is what an application should reach for
//! when it wants request/response/body-chunk callbacks. In Moso's default stack
//! every one of those callbacks would have to be set to `()` to keep the
//! one-line rule, leaving only the span — at the cost of wrapping the response
//! body in a `ResponseBody<..>` that has to be mapped back to `axum::body::Body`
//! on the way out. Opening the span directly is the same behaviour without the
//! body wrapper. `TraceLayer` remains one `s.replace(Slot::Trace, ..)` away.
//!
//! # Fields
//!
//! ```text
//! http.request  method=POST route=/api/v1/users/{id} path=/api/v1/users/0192f…
//!               request_id=01J8XG7K3RQZ4B0N2Y6M9C5V1T status=201 duration_ms=12.4
//! ```
//!
//! `route` is the **pattern**, and it is present only when the composed stack
//! was installed inside routing — see the [module header]. `path` is the
//! concrete path and is only ever a span field, never a metric label: spans are
//! sampled, so an unbounded value there costs nothing.
//!
//! # Continuing an incoming trace
//!
//! A caller that already holds a trace sends its context in a W3C `traceparent`
//! header, and a correct system *continues* that trace rather than starting a
//! fresh one. What "continue" can mean here depends on the build:
//!
//! - **With the `otel` feature**, the request span is made a genuine **child**
//!   of the remote context. The OpenTelemetry propagator that
//!   `observability::init` (behind the `otel` feature) installs parses the
//!   header into a remote `SpanContext`, and
//!   `OpenTelemetrySpanExt::set_parent` (from `tracing-opentelemetry`)
//!   attaches it — so the exported span carries the caller's `trace_id` and
//!   points at the caller's `span_id` as its parent.
//! - **Without the `otel` feature**, plain `tracing` has no representation of a
//!   context that originated in another process, so the span cannot be
//!   reparented onto one. The trace id is still propagated for **correlation**:
//!   [`catch_error`](crate::middleware::catch_error) reads it from the same
//!   header and stamps it on the log line and the problem document. This is a
//!   real limitation, stated rather than faked — a span with a plausible but
//!   fabricated remote parent would be worse than an honestly local one.
//!
//! A missing or malformed `traceparent` leaves the span a local root under both
//! builds.
//!
//! [module header]: crate::middleware

use std::convert::Infallible;
use std::task::{Context, Poll};
use std::time::Instant;

use tracing::{Instrument as _, Level, Span};

use crate::middleware::{UNMATCHED_ROUTE, matched_route};
use crate::router::Route;
use crate::{BoxFuture, Request, Response};

/// The target every event and span from the HTTP stack carries.
pub const LOG_TARGET: &str = "moso::http";

/// The span's name. Fixed, because a span name is part of a `tracing` callsite
/// and callsites are static.
pub const SPAN_NAME: &str = "http.request";

/// How the `trace` slot behaves.
#[derive(Debug, Clone)]
pub struct TraceConfig {
    /// The level the span is opened at. `INFO` by default.
    ///
    /// Dropping it to `DEBUG` is how a service behind a mesh that already
    /// traces keeps the span available for local debugging without paying for
    /// it in production.
    pub level: Level,
    /// Whether to record the concrete request path.
    ///
    /// On. It is the field people actually search by, and a span field is not
    /// a metric label.
    pub record_path: bool,
    /// Whether to record the `User-Agent`.
    ///
    /// Off: it is long, it is high-cardinality, and it is rarely what a
    /// production question turns on.
    pub record_user_agent: bool,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            level: Level::INFO,
            record_path: true,
            record_user_agent: false,
        }
    }
}

impl TraceConfig {
    /// Open the span at `level`.
    pub fn level(&mut self, level: Level) -> &mut Self {
        self.level = level;
        self
    }

    /// Record the `User-Agent` as a span field.
    pub fn with_user_agent(&mut self) -> &mut Self {
        self.record_user_agent = true;
        self
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        let mut summary = format!("level={}", self.level.as_str().to_ascii_lowercase());
        if self.record_user_agent {
            summary.push_str(" user_agent=on");
        }
        summary
    }
}

/// Wrap `service` in the request span.
pub fn layer(config: &TraceConfig, service: Route) -> Route {
    Route::new(TraceMiddleware {
        inner: service,
        config: config.clone(),
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct TraceMiddleware {
    inner: Route,
    config: TraceConfig,
}

/// One span shape, five levels.
///
/// `tracing` builds a static callsite per macro invocation, so the level cannot
/// be a runtime value: it has to be five invocations of the same field list.
/// The macro is what keeps them from drifting.
macro_rules! request_span {
    ($span:ident, $target:expr) => {
        tracing::$span!(
            target: $target,
            SPAN_NAME,
            method = tracing::field::Empty,
            route = tracing::field::Empty,
            path = tracing::field::Empty,
            user_agent = tracing::field::Empty,
            request_id = tracing::field::Empty,
            status = tracing::field::Empty,
            duration_ms = tracing::field::Empty,
        )
    };
}

impl TraceMiddleware {
    /// The span for one request, with everything known up front recorded.
    fn make_span(&self, req: &Request) -> Span {
        let span = match self.config.level {
            Level::ERROR => request_span!(error_span, LOG_TARGET),
            Level::WARN => request_span!(warn_span, LOG_TARGET),
            Level::INFO => request_span!(info_span, LOG_TARGET),
            Level::DEBUG => request_span!(debug_span, LOG_TARGET),
            Level::TRACE => request_span!(trace_span, LOG_TARGET),
        };

        // A disabled span records nothing, and every `record` below is a no-op
        // on one, so the cheap path stays cheap without a branch here.
        span.record("method", req.method().as_str());
        span.record(
            "route",
            matched_route(req.extensions()).unwrap_or(UNMATCHED_ROUTE),
        );
        if self.config.record_path {
            span.record("path", req.uri().path());
        }
        if self.config.record_user_agent
            && let Some(agent) = req
                .headers()
                .get(http::header::USER_AGENT)
                .and_then(|value| value.to_str().ok())
        {
            span.record("user_agent", agent);
        }
        if let Some(id) = req
            .headers()
            .get(crate::REQUEST_ID_HEADER)
            .and_then(|value| value.to_str().ok())
        {
            span.record("request_id", id);
        }
        // Reparent onto an incoming remote trace, when the build can represent
        // one. See the module's "Continuing an incoming trace" section.
        #[cfg(feature = "otel")]
        set_remote_parent(&span, req);
        span
    }
}

/// Make `span` a child of the trace context carried in `req`'s `traceparent`.
///
/// Only under the `otel` feature: reparenting onto a *remote* context needs the
/// OpenTelemetry propagator `observability::init`
/// installs, and plain `tracing` has no notion of a cross-process parent. An
/// absent or malformed header leaves `span` a local root, which is the same
/// outcome the non-`otel` build always has.
#[cfg(feature = "otel")]
fn set_remote_parent(span: &Span, req: &Request) {
    use std::collections::HashMap;

    use opentelemetry::global;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;

    let mut carrier: HashMap<String, String> = HashMap::new();
    for name in ["traceparent", "tracestate"] {
        if let Some(value) = req
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
        {
            carrier.insert(name.to_owned(), value.to_owned());
        }
    }
    if !carrier.contains_key("traceparent") {
        return;
    }
    let parent = global::get_text_map_propagator(|propagator| propagator.extract(&carrier));
    // A failure here means the header did not parse into a usable context; the
    // span simply stays a local root, so the error is not worth surfacing.
    let _ = span.set_parent(parent);
}

impl tower::Service<Request> for TraceMiddleware {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        tower::Service::poll_ready(&mut self.inner, cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);

        let span = self.make_span(&req);
        // One handle to enter the inner future with, one to record the outcome
        // on afterwards. A `Span` is a cheap refcounted handle.
        let outcome = span.clone();

        Box::pin(async move {
            let started = Instant::now();
            let response = tower::Service::call(&mut inner, req)
                .instrument(span)
                .await?;
            outcome.record("status", response.status().as_u16());
            outcome.record("duration_ms", started.elapsed().as_secs_f64() * 1000.0);
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt as _;

    #[test]
    fn the_default_is_an_info_span_with_the_path() {
        let config = TraceConfig::default();
        assert_eq!(config.level, Level::INFO);
        assert!(config.record_path);
        assert!(!config.record_user_agent);
        assert_eq!(config.summary(), "level=info");
    }

    #[test]
    fn the_summary_reports_a_lowered_level() {
        let mut config = TraceConfig::default();
        config.level(Level::DEBUG).with_user_agent();
        assert_eq!(config.summary(), "level=debug user_agent=on");
    }

    /// A subscriber that records the spans it is asked to open, so the test can
    /// assert that exactly one is opened and that it carries the fields the
    /// documentation promises.
    #[derive(Clone, Default)]
    struct SpanRecorder {
        spans: Arc<Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for SpanRecorder {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attributes: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            let mut rendered = attributes.metadata().name().to_owned();
            let mut visitor = Collect(&mut rendered);
            attributes.record(&mut visitor);
            self.spans.lock().expect("lock").push(rendered);
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
            let mut spans = self.spans.lock().expect("lock");
            if let Some(last) = spans.last_mut() {
                let mut visitor = Collect(last);
                values.record(&mut visitor);
            }
        }

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// Appends `key=value` for every recorded field.
    struct Collect<'a>(&'a mut String);

    impl tracing::field::Visit for Collect<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push_str(&format!(" {}={}", field.name(), value));
        }
    }

    /// The `otel` half of "an incoming trace context is continued, not
    /// replaced": a well-formed `traceparent` makes the request span a child of
    /// the remote context, so it carries the caller's `trace_id`.
    #[cfg(feature = "otel")]
    #[test]
    fn a_traceparent_makes_the_request_span_a_child_of_the_remote_trace() {
        use opentelemetry::global;
        use opentelemetry::trace::{TraceContextExt as _, TracerProvider as _};
        use opentelemetry_sdk::propagation::TraceContextPropagator;
        use opentelemetry_sdk::trace::SdkTracerProvider;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        use tracing_subscriber::prelude::*;

        global::set_text_map_propagator(TraceContextPropagator::new());
        // No exporter: the test asserts parentage, which is set before export.
        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));
        let _guard = tracing::subscriber::set_default(subscriber);

        let span = tracing::info_span!("http.request");
        let request = http::Request::builder()
            .uri("/x")
            .header(
                "traceparent",
                "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
            )
            .body(axum::body::Body::empty())
            .expect("request");

        set_remote_parent(&span, &request);

        let trace_id = span.context().span().span_context().trace_id().to_string();
        assert_eq!(trace_id, "4bf92f3577b34da6a3ce929d0e0e4736", "{trace_id}");
    }

    /// The mirror of the above: a malformed header leaves the span a local root
    /// rather than crashing or inventing a parent.
    #[cfg(feature = "otel")]
    #[test]
    fn a_malformed_traceparent_leaves_the_span_a_local_root() {
        use opentelemetry::global;
        use opentelemetry_sdk::propagation::TraceContextPropagator;

        global::set_text_map_propagator(TraceContextPropagator::new());
        let span = tracing::info_span!("http.request");
        let request = http::Request::builder()
            .uri("/x")
            .header("traceparent", "garbage")
            .body(axum::body::Body::empty())
            .expect("request");
        // The point is that this does not panic.
        set_remote_parent(&span, &request);
    }

    #[tokio::test]
    async fn one_span_is_opened_and_carries_the_outcome() {
        let recorder = SpanRecorder::default();
        let guard = tracing::subscriber::set_default(recorder.clone());

        let inner = Route::new(tower::service_fn(|_req: Request| async {
            let mut response = Response::new(axum::body::Body::empty());
            *response.status_mut() = http::StatusCode::CREATED;
            Ok::<_, Infallible>(response)
        }));

        let request = http::Request::builder()
            .method(http::Method::POST)
            .uri("/users/42")
            .header(crate::REQUEST_ID_HEADER, "01J8XG7K3RQZ4B0N2Y6M9C5V1T")
            .body(axum::body::Body::empty())
            .expect("request");

        layer(&TraceConfig::default(), inner)
            .oneshot(request)
            .await
            .expect("infallible");
        drop(guard);

        let spans = recorder.spans.lock().expect("lock").clone();
        assert_eq!(spans.len(), 1, "{spans:?}");
        let span = &spans[0];
        assert!(span.starts_with("http.request"), "{span}");
        assert!(span.contains("method=POST"), "{span}");
        assert!(span.contains("path=/users/42"), "{span}");
        assert!(
            span.contains("request_id=01J8XG7K3RQZ4B0N2Y6M9C5V1T"),
            "{span}"
        );
        assert!(span.contains("status=201"), "{span}");
        assert!(span.contains("duration_ms="), "{span}");
        // No matched path was in the extensions, so the label is the bounded
        // placeholder rather than the raw path.
        assert!(span.contains(&format!("route={UNMATCHED_ROUTE}")), "{span}");
    }
}
