//! `catch_error` — the one place an error becomes a response, and the one place
//! it is logged.
//!
//! # Exactly once
//!
//! No `tracing::error!` anywhere else in the framework, and the documentation
//! tells applications to do the same. An error logged at its construction site,
//! again where it is wrapped, and again at the boundary produces three lines
//! that look like three incidents. [`Error`](crate::Error) is a value; this
//! layer is the event.
//!
//! Exactly once also means *at least* once: this layer emits one line per
//! request whatever the outcome, so a 201 and a 500 are the same shape of
//! record and an access log is a filter rather than a second layer.
//!
//! # Levels
//!
//! | Status | Level | Why |
//! | --- | --- | --- |
//! | 5xx | `ERROR` | with the source chain |
//! | 401, 403, 409, 410, 423, 429 | `WARN` | worth noticing in aggregate |
//! | 404, 422 and every other 4xx | `DEBUG` | routine; at `INFO` they drown everything else |
//! | 2xx, 3xx | `INFO` | the access line |
//!
//! # Rendering
//!
//! This layer installs the [`ErrorContext`] before calling inwards, so an
//! `Error` rendered *anywhere* inside it — by handler glue, by an extractor, by
//! a guard — already carries the request id, the trace id, the path and the
//! configured disclosure policy. There is deliberately no second rendering pass
//! on the way out: re-rendering would discard headers an inner layer had
//! already set, and the first rendering is complete precisely because the
//! context was in scope.
//!
//! # Redaction
//!
//! Structural, never a regex over the body. The log line carries the error's
//! `title: detail: source-chain` and never its field errors — a validation
//! message can quote the value it rejected, and a value is exactly what must
//! not be logged. Headers reach the line only when
//! [`CatchErrorConfig::log_headers`] is set, and then only after every name in
//! [`REDACTED_HEADERS`](crate::extract::headers::REDACTED_HEADERS) has been
//! replaced with a fixed marker.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use http::{HeaderMap, Method, StatusCode};
use tower::Service;
use tracing::Level;

use crate::error::problem::{ErrorContext, ErrorRef, ProblemOptions};
use crate::extract::headers::is_redacted;
use crate::middleware::trace::LOG_TARGET;
use crate::middleware::{UNMATCHED_ROUTE, matched_route};
use crate::router::Route;
use crate::{BoxFuture, Request, Response};

/// How the `catch_error` slot renders and logs.
#[derive(Debug, Clone)]
pub struct CatchErrorConfig {
    /// What may be disclosed, and in what shape.
    pub problem: ProblemOptions,
    /// Whether to include the request's headers in the 5xx log line.
    ///
    /// Redaction still applies. Off by default: a header dump is large, and
    /// most 5xx investigations start from the request id instead.
    pub log_headers: bool,
    /// Whether to count errors by route and status.
    ///
    /// Feeds the `moso_requests_failed_total` counter, which is the signal an
    /// alert should be built on.
    pub count: bool,
}

impl Default for CatchErrorConfig {
    fn default() -> Self {
        Self {
            problem: ProblemOptions::default(),
            log_headers: false,
            count: true,
        }
    }
}

impl CatchErrorConfig {
    /// Disclose 5xx details to the client.
    ///
    /// `http.expose_internal_errors`. Appropriate for an internal service
    /// behind a trusted boundary, and a data-leak waiting to happen anywhere
    /// else — which is why it is one explicit call and not a profile default.
    pub fn expose_internal_errors(mut self) -> Self {
        self.problem.expose_internal_errors = true;
        self
    }

    /// Include redacted request headers in 5xx logs.
    pub fn log_headers(mut self) -> Self {
        self.log_headers = true;
        self
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        format!(
            "expose_internal={} profile={}{}",
            self.problem.expose_internal_errors,
            self.problem.profile.as_str(),
            if self.log_headers {
                " log_headers=true"
            } else {
                ""
            }
        )
    }
}

/// The metric name the layer increments for a failed request.
pub const FAILED_REQUESTS_METRIC: &str = "moso_requests_failed_total";

/// What a redacted header renders as. Fixed, so a log search for it finds
/// every occurrence.
pub const REDACTED_MARKER: &str = "[redacted]";

/// Process-wide failed-request count, readable with [`failed_requests_total`].
static FAILED: AtomicU64 = AtomicU64::new(0);

/// How many requests this process has answered with a 4xx or a 5xx.
///
/// The in-process backing for [`FAILED_REQUESTS_METRIC`].
pub fn failed_requests_total() -> u64 {
    FAILED.load(Ordering::Relaxed)
}

/// Wrap `service` in the error boundary.
///
/// `silent` is a list of path prefixes kept out of the access log — the health
/// probes and the docs UI, which are polled by infrastructure nobody is
/// debugging and would otherwise be the bulk of a quiet service's log volume.
pub fn layer(config: &CatchErrorConfig, silent: Arc<[String]>, service: Route) -> Route {
    Route::new(CatchError {
        inner: service,
        config: Arc::new(config.clone()),
        silent,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct CatchError {
    inner: Route,
    config: Arc<CatchErrorConfig>,
    silent: Arc<[String]>,
}

impl Service<Request> for CatchError {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let config = Arc::clone(&self.config);
        let silent = Arc::clone(&self.silent);

        let (parts, body) = req.into_parts();
        let method = parts.method.clone();
        let uri = parts.uri.clone();
        let route: Arc<str> = matched_route(&parts.extensions)
            .unwrap_or(UNMATCHED_ROUTE)
            .into();
        let request_id = header_string(&parts.headers, crate::REQUEST_ID_HEADER);
        let trace_id = trace_id_from(&parts.headers);
        let headers = config.log_headers.then(|| redacted_headers(&parts.headers));

        // `with_parts` fills in the instance path and the HTML preference; the
        // two ids are the part only this layer knows.
        let mut context = ErrorContext::new(config.problem.clone()).with_parts(&parts);
        context.request_id = request_id.clone();
        context.trace_id = trace_id;

        let req = Request::from_parts(parts, body);

        Box::pin(async move {
            let started = Instant::now();
            // Everything rendered inside this scope — handler glue, extractor
            // rejections, guard refusals — picks the context up, which is what
            // makes a second rendering pass unnecessary.
            let response =
                crate::error::problem::with_error_context(context, inner.call(req)).await?;
            let elapsed = started.elapsed();

            let status = response.status();
            if config.count && (status.is_client_error() || status.is_server_error()) {
                FAILED.fetch_add(1, Ordering::Relaxed);
            }

            if !is_silent(&silent, uri.path()) {
                let error = response.extensions().get::<ErrorRef>();
                let level =
                    error.map_or_else(|| level_for(status), |error| error.kind().log_level());
                let detail = error.map(|error| {
                    if error.is_server_error() {
                        error.chain()
                    } else {
                        error.to_string()
                    }
                });
                // A header dump is large and is only ever wanted for the class
                // of failure nobody can reproduce.
                let headers = headers.filter(|_| status.is_server_error());
                emit(
                    level,
                    &Line {
                        status: status.as_u16(),
                        method: &method,
                        route: &route,
                        path: uri.path(),
                        duration_ms: elapsed.as_secs_f64() * 1000.0,
                        request_id: request_id.as_deref(),
                        error: detail.as_deref(),
                        headers: headers.as_deref(),
                    },
                );
            }

            Ok(response)
        })
    }
}

/// The one log line, before it is handed to a level.
struct Line<'a> {
    status: u16,
    method: &'a Method,
    route: &'a str,
    path: &'a str,
    duration_ms: f64,
    request_id: Option<&'a str>,
    error: Option<&'a str>,
    headers: Option<&'a str>,
}

/// One field list, five levels.
///
/// A `tracing` callsite is static, so the level cannot be a runtime value. The
/// macro is what stops the five copies from drifting apart.
macro_rules! request_line {
    ($level:ident, $line:expr) => {
        tracing::$level!(
            target: LOG_TARGET,
            status = $line.status,
            method = %$line.method,
            route = $line.route,
            path = $line.path,
            duration_ms = $line.duration_ms,
            request_id = $line.request_id,
            error = $line.error,
            headers = $line.headers,
            "request",
        )
    };
}

/// Emit `line` at `level`.
fn emit(level: Level, line: &Line<'_>) {
    match level {
        Level::ERROR => request_line!(error, line),
        Level::WARN => request_line!(warn, line),
        Level::INFO => request_line!(info, line),
        Level::DEBUG => request_line!(debug, line),
        Level::TRACE => request_line!(trace, line),
    }
}

/// The level a response with no [`Error`](crate::Error) behind it is logged at.
///
/// The same split [`ErrorKind::log_level`](crate::ErrorKind::log_level) applies,
/// derived from the status rather than the taxonomy, so a problem document the
/// router itself produced is logged like one an `Error` produced.
pub fn level_for(status: StatusCode) -> Level {
    if status.is_server_error() {
        return Level::ERROR;
    }
    match status.as_u16() {
        401 | 403 | 409 | 410 | 423 | 429 => Level::WARN,
        400..=499 => Level::DEBUG,
        _ => Level::INFO,
    }
}

/// Whether `path` is one the access log ignores.
fn is_silent(prefixes: &[String], path: &str) -> bool {
    prefixes
        .iter()
        .any(|prefix| !prefix.is_empty() && path.starts_with(prefix.as_str()))
}

/// A header's value as an owned string, when it is text at all.
fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The W3C trace id from a `traceparent`, if the header is well formed.
///
/// `00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01` → the middle
/// 32-character field. A malformed header yields `None` rather than a guess.
pub fn trace_id_from(headers: &HeaderMap) -> Option<String> {
    let value = headers.get("traceparent")?.to_str().ok()?;
    let mut fields = value.split('-');
    let _version = fields.next()?;
    let trace_id = fields.next()?;
    (trace_id.len() == 32 && trace_id.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| trace_id.to_owned())
}

/// The request's headers, rendered for a log line with the sensitive ones
/// replaced.
///
/// Structural redaction: the decision is made from the header *name* against a
/// fixed list, never by pattern-matching the value. A regex over a value is
/// both slower and wrong — it misses the secret that does not look like one.
pub fn redacted_headers(headers: &HeaderMap) -> String {
    let mut out = String::with_capacity(headers.len() * 32);
    for (name, value) in headers {
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(name.as_str());
        out.push('=');
        if is_redacted(name.as_str()) || value.is_sensitive() {
            out.push_str(REDACTED_MARKER);
        } else {
            match value.to_str() {
                Ok(text) => out.push_str(text),
                Err(_) => out.push_str("[binary]"),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use crate::error::Error;
    use crate::{IntoResponse, Result};
    use std::sync::Mutex;
    use tower::ServiceExt as _;

    #[test]
    fn defaults_disclose_nothing_and_count() {
        let config = CatchErrorConfig::default();
        assert!(!config.problem.expose_internal_errors);
        assert!(config.count);
        assert!(!config.log_headers);
    }

    #[test]
    fn the_summary_names_the_disclosure_policy() {
        assert_eq!(
            CatchErrorConfig::default().summary(),
            "expose_internal=false profile=production"
        );
        assert!(
            CatchErrorConfig::default()
                .expose_internal_errors()
                .log_headers()
                .summary()
                .contains("log_headers=true")
        );
    }

    #[test]
    fn the_levels_follow_the_documented_table() {
        assert_eq!(level_for(StatusCode::OK), Level::INFO);
        assert_eq!(level_for(StatusCode::FOUND), Level::INFO);
        assert_eq!(level_for(StatusCode::NOT_FOUND), Level::DEBUG);
        assert_eq!(level_for(StatusCode::UNPROCESSABLE_ENTITY), Level::DEBUG);
        assert_eq!(level_for(StatusCode::BAD_REQUEST), Level::DEBUG);
        assert_eq!(level_for(StatusCode::UNAUTHORIZED), Level::WARN);
        assert_eq!(level_for(StatusCode::FORBIDDEN), Level::WARN);
        assert_eq!(level_for(StatusCode::CONFLICT), Level::WARN);
        assert_eq!(level_for(StatusCode::TOO_MANY_REQUESTS), Level::WARN);
        assert_eq!(level_for(StatusCode::INTERNAL_SERVER_ERROR), Level::ERROR);
        assert_eq!(level_for(StatusCode::SERVICE_UNAVAILABLE), Level::ERROR);
    }

    #[test]
    fn a_trace_parent_yields_its_trace_id() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "traceparent",
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
                .parse()
                .expect("value"),
        );
        assert_eq!(
            trace_id_from(&headers).as_deref(),
            Some("4bf92f3577b34da6a3ce929d0e0e4736")
        );

        headers.insert("traceparent", "garbage".parse().expect("value"));
        assert!(trace_id_from(&headers).is_none());
        assert!(trace_id_from(&HeaderMap::new()).is_none());
    }

    #[test]
    fn header_redaction_is_by_name_not_by_pattern() {
        let mut headers = HeaderMap::new();
        headers.insert(
            http::header::AUTHORIZATION,
            "Bearer hunter2".parse().unwrap(),
        );
        headers.insert(http::header::COOKIE, "session=abc".parse().unwrap());
        headers.insert(http::header::ACCEPT, "application/json".parse().unwrap());

        let rendered = redacted_headers(&headers);
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(!rendered.contains("session=abc"), "{rendered}");
        assert!(rendered.contains("accept=application/json"), "{rendered}");
        assert_eq!(rendered.matches(REDACTED_MARKER).count(), 2, "{rendered}");
    }

    // ── the layer ────────────────────────────────────────────────────────

    /// Captures every event, so a test can assert that there is exactly one.
    #[derive(Clone, Default)]
    struct Capture {
        events: Arc<Mutex<Vec<(Level, String)>>>,
    }

    impl tracing::Subscriber for Capture {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            let mut rendered = String::new();
            event.record(&mut Collect(&mut rendered));
            self.events
                .lock()
                .expect("lock")
                .push((*event.metadata().level(), rendered));
        }

        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    struct Collect<'a>(&'a mut String);

    impl tracing::field::Visit for Collect<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn core::fmt::Debug) {
            self.0.push_str(&format!(" {}={:?}", field.name(), value));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.push_str(&format!(" {}={}", field.name(), value));
        }
    }

    fn route_returning(result: Result<Response>) -> Route {
        let result = Arc::new(Mutex::new(Some(result)));
        Route::new(tower::service_fn(move |_req: Request| {
            let result = Arc::clone(&result);
            async move {
                let taken = result.lock().expect("lock").take();
                Ok::<_, Infallible>(match taken {
                    Some(Ok(response)) => response,
                    Some(Err(error)) => error.into_response(),
                    None => Response::new(axum::body::Body::empty()),
                })
            }
        }))
    }

    async fn run(config: CatchErrorConfig, silent: &[&str], inner: Route) -> (Capture, Response) {
        let capture = Capture::default();
        let guard = tracing::subscriber::set_default(capture.clone());

        let silent: Arc<[String]> = Arc::from(
            silent
                .iter()
                .map(|path| (*path).to_owned())
                .collect::<Vec<_>>(),
        );
        let request = http::Request::builder()
            .uri("/api/users")
            .header(crate::REQUEST_ID_HEADER, "01J8XG7K3RQZ4B0N2Y6M9C5V1T")
            .header(http::header::AUTHORIZATION, "Bearer hunter2")
            .body(axum::body::Body::empty())
            .expect("request");

        let response = layer(&config, silent, inner)
            .oneshot(request)
            .await
            .expect("infallible");
        drop(guard);
        (capture, response)
    }

    /// The acceptance criterion: exactly one line per request.
    #[tokio::test]
    async fn a_successful_request_logs_exactly_one_info_line() {
        let (capture, response) = run(
            CatchErrorConfig::default(),
            &[],
            route_returning(Ok(Response::new(axum::body::Body::empty()))),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
        let events = capture.events.lock().expect("lock").clone();
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0].0, Level::INFO);
        assert!(events[0].1.contains("status=200"), "{:?}", events[0]);
        assert!(events[0].1.contains("path=/api/users"), "{:?}", events[0]);
        assert!(
            events[0]
                .1
                .contains("request_id=01J8XG7K3RQZ4B0N2Y6M9C5V1T"),
            "{:?}",
            events[0]
        );
    }

    #[tokio::test]
    async fn a_failure_logs_exactly_one_line_at_the_documented_level() {
        for (error, level, status) in [
            (Error::not_found("user"), Level::DEBUG, 404),
            (Error::forbidden("nope"), Level::WARN, 403),
            (
                Error::internal(std::io::Error::other("boom")),
                Level::ERROR,
                500,
            ),
        ] {
            let (capture, response) = run(
                CatchErrorConfig::default(),
                &[],
                route_returning(Err(error)),
            )
            .await;

            assert_eq!(response.status().as_u16(), status);
            let events = capture.events.lock().expect("lock").clone();
            assert_eq!(events.len(), 1, "{events:?}");
            assert_eq!(events[0].0, level, "{events:?}");
        }
    }

    #[tokio::test]
    async fn a_5xx_line_carries_the_source_chain() {
        let error = Error::unavailable("database is down")
            .with_source(std::io::Error::other("tcp connect error"));
        let (capture, _) = run(
            CatchErrorConfig::default(),
            &[],
            route_returning(Err(error)),
        )
        .await;
        let events = capture.events.lock().expect("lock").clone();
        assert!(events[0].1.contains("tcp connect error"), "{:?}", events[0]);
    }

    /// Structural redaction: a secret in a *field* error never reaches a line,
    /// because field errors are not logged at all.
    #[tokio::test]
    async fn a_secret_in_a_field_error_never_reaches_the_log() {
        const CANARY: &str = "SECRET_PASSWORD_hunter2";
        let error = Error::validation(moso_schema::ValidationErrors::one(
            "/password",
            "custom:weak",
            CANARY,
        ));
        let (capture, _) = run(
            CatchErrorConfig::default(),
            &[],
            route_returning(Err(error)),
        )
        .await;
        let events = capture.events.lock().expect("lock").clone();
        assert_eq!(events.len(), 1);
        assert!(!events[0].1.contains(CANARY), "{:?}", events[0]);
    }

    #[tokio::test]
    async fn logged_headers_are_redacted_and_only_on_a_5xx() {
        let config = CatchErrorConfig::default().log_headers();
        let (capture, _) = run(
            config.clone(),
            &[],
            route_returning(Err(Error::internal_msg("boom"))),
        )
        .await;
        let events = capture.events.lock().expect("lock").clone();
        assert!(events[0].1.contains("headers="), "{:?}", events[0]);
        assert!(!events[0].1.contains("hunter2"), "{:?}", events[0]);

        let (capture, _) = run(
            config,
            &[],
            route_returning(Ok(Response::new(axum::body::Body::empty()))),
        )
        .await;
        let events = capture.events.lock().expect("lock").clone();
        assert!(!events[0].1.contains("accept="), "{:?}", events[0]);
    }

    #[tokio::test]
    async fn a_silenced_path_logs_nothing() {
        let (capture, _) = run(
            CatchErrorConfig::default(),
            &["/api"],
            route_returning(Ok(Response::new(axum::body::Body::empty()))),
        )
        .await;
        assert!(capture.events.lock().expect("lock").is_empty());
    }

    #[tokio::test]
    async fn failures_are_counted() {
        let before = failed_requests_total();
        run(
            CatchErrorConfig::default(),
            &[],
            route_returning(Err(Error::not_found("user"))),
        )
        .await;
        // `>` rather than `== before + 1`: the counter is process-wide and the
        // test binary runs its tests in parallel.
        assert!(failed_requests_total() > before);
    }

    /// The rendering half: an error raised inside the layer picks up the
    /// request id and the path without anything else having to pass them.
    #[tokio::test]
    async fn the_installed_context_reaches_the_problem_document() {
        use http_body_util::BodyExt as _;

        let (_, response) = run(
            CatchErrorConfig::default(),
            &[],
            route_returning(Err(Error::not_found("user"))),
        )
        .await;

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).expect("json");
        assert_eq!(json["request_id"], "01J8XG7K3RQZ4B0N2Y6M9C5V1T");
        assert_eq!(json["instance"], "/api/users");
        assert_eq!(json["status"], 404);
    }

    #[tokio::test]
    async fn a_5xx_detail_is_still_suppressed_by_default() {
        use http_body_util::BodyExt as _;

        const CANARY: &str = "SECRET_TABLE_users";
        let (_, response) = run(
            CatchErrorConfig::default(),
            &[],
            route_returning(Err(Error::internal(std::io::Error::other(CANARY)))),
        )
        .await;

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("utf-8");
        assert!(!body.contains(CANARY), "{body}");
    }

    #[tokio::test]
    async fn exposing_internal_errors_reaches_the_body() {
        use http_body_util::BodyExt as _;

        const CANARY: &str = "SECRET_TABLE_users";
        let mut config = CatchErrorConfig {
            problem: ProblemOptions::for_profile(Profile::Dev),
            ..CatchErrorConfig::default()
        };
        config.problem.expose_internal_errors = true;

        let (_, response) = run(
            config,
            &[],
            route_returning(Err(Error::internal(std::io::Error::other(CANARY)))),
        )
        .await;

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let body = String::from_utf8(bytes.to_vec()).expect("utf-8");
        assert!(body.contains(CANARY), "{body}");
    }
}
