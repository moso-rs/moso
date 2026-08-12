//! `catch_panic` — one bad request must not kill the connection.
//!
//! A panic is a bug. This layer exists so that a bug costs one request rather
//! than one connection and every request multiplexed onto it — **not** so that
//! `unwrap()` becomes an acceptable control-flow choice. The documentation says
//! so in those words, because a framework that silently swallows panics teaches
//! people to write them.
//!
//! What it does:
//!
//! - converts the panic into a 500 problem carrying the request id,
//! - logs at `ERROR` with the payload and the request context,
//! - increments [`PANICS_METRIC`] — an alertable signal,
//! - in the `dev` profile, renders the message into the response body, because
//!   hunting for it in a terminal is a wasted afternoon.
//!
//! # The backtrace
//!
//! It is **not** captured here. By the time `catch_unwind` returns, the stack
//! between the panic and this layer has already unwound, so a backtrace taken
//! at the catch site describes the wrong frames. The useful backtrace is the
//! one the panic *hook* prints at the panic site, which still happens: this
//! layer does not replace the hook, it only stops the unwind from reaching the
//! connection. `RUST_BACKTRACE=1` therefore works exactly as it always does.
//!
//! # It is the outermost slot, and that costs one thing
//!
//! Being outside `request_id` is what lets it catch a panic in `request_id`
//! itself — but it also means the correlation id does not exist yet when the
//! request goes in. The layer therefore hands the inner stack a one-shot cell
//! and reads the id back out of it on the way past. A panic *before*
//! `request_id` ran leaves the cell empty and the problem document simply has
//! no `request_id`, which is honest: there was not one.

use std::any::Any;
use std::convert::Infallible;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use futures_util::FutureExt as _;
use http::{Method, StatusCode, Uri};
use tower::Service;

use crate::error::ErrorKind;
use crate::error::problem::{Problem, ProblemOptions};
use crate::middleware::request_id::RequestIdSlot;
use crate::router::Route;
use crate::{BoxFuture, IntoResponse, Request, Response};

/// How the `catch_panic` slot behaves.
#[derive(Debug, Clone)]
pub struct CatchPanicConfig {
    /// Whether to render the panic message and backtrace into the body.
    ///
    /// On in `dev`, off everywhere else. A panic message routinely contains an
    /// index, a key, or a slice of the data that caused it.
    pub render_details: bool,
    /// Whether to count panics.
    pub count: bool,
}

impl Default for CatchPanicConfig {
    fn default() -> Self {
        Self {
            render_details: false,
            count: true,
        }
    }
}

impl CatchPanicConfig {
    /// The configuration for a profile.
    pub fn for_profile(profile: crate::config::Profile) -> Self {
        Self {
            render_details: matches!(profile, crate::config::Profile::Dev),
            count: true,
        }
    }

    /// Render panic details into the response body.
    pub fn render_details(mut self) -> Self {
        self.render_details = true;
        self
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        format!("render_details={}", self.render_details)
    }
}

/// The counter incremented when a handler panics.
///
/// Labelled by matched route. A non-zero rate here is always a bug and should
/// always page someone.
pub const PANICS_METRIC: &str = "moso_panics_total";

/// The log target every event this layer emits carries.
const LOG_TARGET: &str = "moso::http";

/// Process-wide panic count, readable with [`panics_total`].
static PANICS: AtomicU64 = AtomicU64::new(0);

/// How many handler panics this process has caught.
///
/// The in-process backing for [`PANICS_METRIC`]. A metrics exporter reads it;
/// a test asserts on it. Monotonic, and never reset.
pub fn panics_total() -> u64 {
    PANICS.load(Ordering::Relaxed)
}

/// Render a panic payload as a string.
///
/// Handles the two payload types that occur in practice — `&'static str` and
/// `String` — and falls back to a fixed message for anything else, rather than
/// pretending to know what an arbitrary `Box<dyn Any>` says.
pub fn panic_message(payload: &(dyn Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&'static str>() {
        return (*message).to_owned();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "a panic payload that is neither `&str` nor `String`".to_owned()
}

/// Wrap `service` in the panic catcher.
pub fn layer(config: &CatchPanicConfig, service: Route) -> Route {
    Route::new(CatchPanic {
        inner: service,
        config: Arc::new(config.clone()),
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct CatchPanic {
    inner: Route,
    config: Arc<CatchPanicConfig>,
}

impl Service<Request> for CatchPanic {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        // `self.inner` is the instance that was polled ready, so it is the one
        // that must be called; the clone takes its place for the next call.
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let config = Arc::clone(&self.config);

        let method = req.method().clone();
        let uri = req.uri().clone();
        let slot = RequestIdSlot::default();
        req.extensions_mut().insert(slot.clone());

        Box::pin(async move {
            // Two catch sites, because a `Service::call` may panic before it
            // ever produces a future.
            let started = std::panic::catch_unwind(AssertUnwindSafe(|| inner.call(req)));
            let outcome = match started {
                Ok(future) => AssertUnwindSafe(future).catch_unwind().await,
                Err(payload) => Err(payload),
            };

            match outcome {
                Ok(result) => result,
                // `&*payload`, not `&payload`: `&Box<dyn Any + Send>` also
                // unsize-coerces to `&dyn Any` with the *box* as the concrete
                // type, and every downcast would then miss.
                Err(payload) => Ok(render(&*payload, &config, &method, &uri, slot.get())),
            }
        })
    }
}

/// Log the panic, count it, and turn it into a 500 problem document.
fn render(
    payload: &(dyn Any + Send),
    config: &CatchPanicConfig,
    method: &Method,
    uri: &Uri,
    request_id: Option<String>,
) -> Response {
    let message = panic_message(payload);

    if config.count {
        PANICS.fetch_add(1, Ordering::Relaxed);
    }

    tracing::error!(
        target: LOG_TARGET,
        metric = PANICS_METRIC,
        panic = %message,
        method = %method,
        path = %uri.path(),
        request_id = request_id.as_deref(),
        "handler panicked",
    );

    // `render_details` is exactly `expose_internal_errors` for this one
    // document, so the disclosure decision goes through the same code as every
    // other error rather than being re-derived here.
    let options = ProblemOptions {
        expose_internal_errors: config.render_details,
        ..ProblemOptions::default()
    };
    let mut problem = Problem::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        ErrorKind::Internal.type_uri(),
        ErrorKind::Internal.title(),
    )
    .with_instance(uri.path().to_owned());
    if options.expose_internal_errors {
        problem.detail = Some(format!("panicked: {message}"));
    }
    if let Some(request_id) = request_id {
        problem = problem.with_request_id(request_id);
    }
    problem.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Profile;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    async fn body_string(response: Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8")
    }

    fn panicking_route(message: &'static str) -> Route {
        Route::new(tower::service_fn(move |_req: Request| async move {
            panic!("{message}");
            #[allow(
                unreachable_code,
                reason = "the panic is the point of the fixture; the tail exists only to \
                          give the closure the return type `service_fn` requires"
            )]
            Ok::<Response, Infallible>(Response::new(axum::body::Body::empty()))
        }))
    }

    #[test]
    fn defaults_hide_details_and_count() {
        let config = CatchPanicConfig::default();
        assert!(!config.render_details);
        assert!(config.count);
    }

    #[test]
    fn only_dev_renders_details() {
        assert!(CatchPanicConfig::for_profile(Profile::Dev).render_details);
        assert!(!CatchPanicConfig::for_profile(Profile::Test).render_details);
        assert!(!CatchPanicConfig::for_profile(Profile::Production).render_details);
    }

    #[test]
    fn the_summary_names_the_only_visible_setting() {
        assert_eq!(
            CatchPanicConfig::default().summary(),
            "render_details=false"
        );
    }

    #[test]
    fn panic_messages_cover_the_two_payloads_that_happen() {
        let str_payload: Box<dyn Any + Send> = Box::new("boom");
        assert_eq!(panic_message(&*str_payload), "boom");

        let string_payload: Box<dyn Any + Send> = Box::new("boom 42".to_owned());
        assert_eq!(panic_message(&*string_payload), "boom 42");

        let other: Box<dyn Any + Send> = Box::new(7_u8);
        assert!(panic_message(&*other).contains("neither"));
    }

    /// Acceptance criterion: a panic in a handler yields a 500 problem and the
    /// service survives to answer the next request.
    #[tokio::test]
    async fn a_panic_becomes_a_500_problem_and_the_service_survives() {
        let before = panics_total();
        let service = layer(&CatchPanicConfig::default(), panicking_route("kaboom"));

        for _ in 0..2 {
            let response = service
                .clone()
                .oneshot(Request::new(axum::body::Body::empty()))
                .await
                .expect("the layer is infallible");

            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(
                response
                    .headers()
                    .get(http::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/problem+json")
            );
            let body = body_string(response).await;
            // The payload is a detail of a 5xx, so it is suppressed.
            assert!(!body.contains("kaboom"), "{body}");
            let json: serde_json::Value = serde_json::from_str(&body).expect("json");
            assert_eq!(json["status"], 500);
        }

        // `>=` rather than `==`: the counter is process-wide and the test
        // binary runs its tests in parallel.
        assert!(panics_total() >= before + 2);
    }

    #[tokio::test]
    async fn dev_renders_the_panic_message() {
        let service = layer(
            &CatchPanicConfig::for_profile(Profile::Dev),
            panicking_route("index out of bounds"),
        );
        let response = service
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        let body = body_string(response).await;
        assert!(body.contains("index out of bounds"), "{body}");
    }

    #[tokio::test]
    async fn a_healthy_response_passes_straight_through() {
        let inner = Route::new(tower::service_fn(|_req: Request| async {
            let mut response = Response::new(axum::body::Body::empty());
            *response.status_mut() = StatusCode::CREATED;
            Ok::<_, Infallible>(response)
        }));
        let response = layer(&CatchPanicConfig::default(), inner)
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn the_problem_carries_the_id_the_inner_stack_assigned() {
        // Stand in for the `request_id` layer: fill the cell `catch_panic`
        // handed inwards, then panic.
        let inner = Route::new(tower::service_fn(|req: Request| async move {
            if let Some(slot) = req.extensions().get::<RequestIdSlot>() {
                slot.set("01J8XG7K3RQZ4B0N2Y6M9C5V1T");
            }
            panic!("after the id was assigned");
            #[allow(
                unreachable_code,
                reason = "the panic is the point of the fixture; the tail exists only to \
                          give the closure the return type `service_fn` requires"
            )]
            Ok::<Response, Infallible>(Response::new(axum::body::Body::empty()))
        }));

        let response = layer(&CatchPanicConfig::default(), inner)
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        let json: serde_json::Value =
            serde_json::from_str(&body_string(response).await).expect("json");
        assert_eq!(json["request_id"], "01J8XG7K3RQZ4B0N2Y6M9C5V1T");
    }
}
