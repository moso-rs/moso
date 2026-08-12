//! `timeout` — a request that outlives its budget becomes a 504, not a leak.
//!
//! Placed **inside** `catch_error` so the expiry renders as a problem document
//! with a request id, rather than as a dropped connection the client has to
//! guess about.
//!
//! # Long-lived responses
//!
//! SSE and WebSocket handlers exceed any sane request timeout by design. They
//! are exempted by matched route rather than by disabling the slot, so the rest
//! of the application keeps its budget.
//!
//! # Why not `tower_http::timeout::TimeoutLayer`
//!
//! It answers with the configured status and an **empty body**, and it has no
//! notion of an exemption. Moso's contract is a problem document carrying the
//! request id and the budget that was exceeded, and per-route exemptions, so
//! the expiry goes through [`Error::timeout`](crate::Error::timeout) like every
//! other failure. The per-route `Router::timeout` layer answers identically.

use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use tower::Service;

use crate::error::Error;
use crate::middleware::matched_route;
use crate::router::Route;
use crate::{BoxFuture, IntoResponse, Request, Response};

/// How the `timeout` slot behaves.
#[derive(Debug, Clone)]
pub struct TimeoutConfig {
    /// The budget for a request. `http.timeout`, default 30 s.
    pub timeout: Duration,
    /// Route patterns exempt from the timeout.
    ///
    /// Matched against the route pattern (`/events/{id}`), never the raw path,
    /// so an exemption cannot be widened by a crafted URL.
    pub exempt: Vec<String>,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            exempt: Vec::new(),
        }
    }
}

impl TimeoutConfig {
    /// A budget of `timeout`.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            ..Self::default()
        }
    }

    /// Exempt a route pattern.
    pub fn exempt(mut self, pattern: impl Into<String>) -> Self {
        self.exempt.push(pattern.into());
        self
    }

    /// Whether a matched route is exempt.
    pub fn is_exempt(&self, matched_path: &str) -> bool {
        self.exempt.iter().any(|pattern| pattern == matched_path)
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        let mut summary = humantime::format_duration(self.timeout).to_string();
        if !self.exempt.is_empty() {
            summary.push_str(&format!(" exempt={}", self.exempt.len()));
        }
        summary
    }
}

/// Wrap `service` in the request budget.
pub fn layer(config: &TimeoutConfig, service: Route) -> Route {
    Route::new(TimeoutMiddleware {
        inner: service,
        timeout: config.timeout,
        exempt: Arc::from(config.exempt.clone()),
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct TimeoutMiddleware {
    inner: Route,
    timeout: Duration,
    exempt: Arc<[String]>,
}

impl Service<Request> for TimeoutMiddleware {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let timeout = self.timeout;

        // Only the matched *pattern* can grant an exemption. With the stack
        // installed outside routing there is no pattern, so nothing is exempt —
        // which is the safe direction to fail.
        let exempt = matched_route(req.extensions())
            .is_some_and(|route| self.exempt.iter().any(|pattern| pattern == route));

        Box::pin(async move {
            if exempt {
                return inner.call(req).await;
            }
            match tokio::time::timeout(timeout, inner.call(req)).await {
                Ok(result) => result,
                Err(_) => Ok(Error::timeout(timeout).into_response()),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use tower::ServiceExt as _;

    #[test]
    fn the_default_is_thirty_seconds() {
        assert_eq!(TimeoutConfig::default().timeout, Duration::from_secs(30));
    }

    #[test]
    fn exemptions_match_the_pattern_exactly() {
        let config = TimeoutConfig::default().exempt("/events/{id}");
        assert!(config.is_exempt("/events/{id}"));
        assert!(!config.is_exempt("/events/42"));
    }

    #[test]
    fn the_summary_is_humane() {
        assert_eq!(TimeoutConfig::default().summary(), "30s");
        assert_eq!(
            TimeoutConfig::new(Duration::from_millis(1500)).summary(),
            "1s 500ms"
        );
        assert!(
            TimeoutConfig::default()
                .exempt("/events/{id}")
                .summary()
                .contains("exempt=1")
        );
    }

    fn slow_route(delay: Duration) -> Route {
        Route::new(tower::service_fn(move |_req: Request| async move {
            tokio::time::sleep(delay).await;
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }))
    }

    #[tokio::test(start_paused = true)]
    async fn an_overrunning_request_becomes_a_504_problem() {
        let config = TimeoutConfig::new(Duration::from_millis(50));
        let response = layer(&config, slow_route(Duration::from_secs(10)))
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        // …and it is an `Error`, so `catch_error` can log it like any other.
        assert!(
            response
                .extensions()
                .get::<crate::error::problem::ErrorRef>()
                .is_some()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_request_inside_the_budget_is_untouched() {
        let config = TimeoutConfig::new(Duration::from_secs(10));
        let response = layer(&config, slow_route(Duration::from_millis(1)))
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::OK);
    }

    /// Send `path` through an Axum router that mounts `service` at `pattern`,
    /// which is the only way to obtain a real `MatchedPath`: Axum's constructor
    /// is crate-private, and faking one would prove nothing.
    async fn through_router(pattern: &str, path: &str, service: Route) -> Response {
        let router = axum::Router::new().route_service(pattern, service);
        let request = http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("request");
        router
            .into_service::<axum::body::Body>()
            .oneshot(request)
            .await
            .expect("infallible")
    }

    #[tokio::test(start_paused = true)]
    async fn an_exempt_pattern_is_not_timed() {
        let config = TimeoutConfig::new(Duration::from_millis(10)).exempt("/events/{id}");
        let response = through_router(
            "/events/{id}",
            "/events/42",
            layer(&config, slow_route(Duration::from_secs(5))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test(start_paused = true)]
    async fn a_route_that_is_not_exempt_is_still_timed() {
        let config = TimeoutConfig::new(Duration::from_millis(10)).exempt("/events/{id}");
        let response = through_router(
            "/users/{id}",
            "/users/42",
            layer(&config, slow_route(Duration::from_secs(5))),
        )
        .await;
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn a_raw_path_cannot_buy_an_exemption() {
        // The exemption names a pattern; with no match behind the request there
        // is no pattern at all, so nothing is exempt.
        let config = TimeoutConfig::new(Duration::from_millis(10)).exempt("/events/{id}");
        let request = http::Request::builder()
            .uri("/events/%7Bid%7D")
            .body(axum::body::Body::empty())
            .expect("request");

        let response = layer(&config, slow_route(Duration::from_secs(5)))
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }
}
