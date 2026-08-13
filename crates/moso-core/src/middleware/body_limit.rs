//! `body_limit` — reject an oversized body before it is read.
//!
//! The layer is a coarse outer guard: it checks `Content-Length` and caps the
//! stream. The precise, per-extractor enforcement lives in
//! [`read_limited`](crate::extract::read_limited), which is where the 413 with
//! the right limit in it comes from.
//!
//! Both exist because they defend different things. The layer stops a body from
//! being buffered by *any* route including an Axum mount; the extractor knows
//! which limit applies to this operation and can report it.
//!
//! # Why not `tower_http::limit::RequestBodyLimitLayer`
//!
//! It rewrites the *request* body type to `Limited<B>`, so the service it wraps
//! must accept `Request<Limited<Body>>`. Moso's [`Route`] accepts
//! `Request<axum::body::Body>` and nothing else — that is what makes the stack
//! reorderable at runtime — so the layer cannot wrap one. The cap here is the
//! same `http_body_util::Limited`, re-erased into an `axum::body::Body` so the
//! request type is unchanged.
//!
//! # What each half answers with
//!
//! A body that *declares* itself too large in `Content-Length` is refused here,
//! before a byte is read, with a 413 naming the limit. A body that lies, or
//! that is chunked and has no length to lie in, is cut off mid-stream by the
//! cap; the extractor counting as it reads produces the precise 413 for that
//! case.
//!
//! So that the two halves cannot name different numbers, the layer records its
//! cap in the request extensions as [`BodyCap`], and
//! [`read_limited`](crate::extract::read_limited) enforces the *tighter* of
//! that and the operation's own limit. Without it an application whose stack
//! cap is below `http.body_max` would be told about a limit that is not the one
//! that stopped it.

use std::convert::Infallible;
use std::task::{Context, Poll};

use http_body_util::Limited;
use tower::Service;

use crate::error::Error;
use crate::router::Route;
use crate::{BoxFuture, IntoResponse, Request, Response};

/// How the `body_limit` slot behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyLimitConfig {
    /// The maximum body size in bytes. `http.body_max`, default 2 MiB.
    pub max_bytes: usize,
    /// Whether to reject on `Content-Length` alone.
    ///
    /// On by default. It is the cheap check; the streaming cap still applies to
    /// a chunked body, which has no `Content-Length` to lie in.
    pub trust_content_length: bool,
}

impl Default for BodyLimitConfig {
    fn default() -> Self {
        Self {
            max_bytes: crate::ctx::Limits::DEFAULT.body_max,
            trust_content_length: true,
        }
    }
}

impl BodyLimitConfig {
    /// A limit of `max_bytes`.
    pub fn new(max_bytes: usize) -> Self {
        Self {
            max_bytes,
            ..Self::default()
        }
    }

    /// A one-line summary for `moso middleware`.
    pub fn summary(&self) -> String {
        format_bytes(self.max_bytes)
    }
}

/// Render a byte count the way the middleware report and the 413 detail do.
///
/// Binary units, at most one decimal: `2 MiB`, `1.5 GiB`, `512 B`. Shared so
/// the limit a client is told about is spelled the same as the limit an
/// operator configured.
pub fn format_bytes(bytes: usize) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }

    // A whole number prints without a decimal point: `2 MiB`, not `2.0 MiB`.
    if (value - value.round()).abs() < f64::EPSILON {
        format!("{} {}", value.round() as u64, UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// The cap the `body_limit` layer installed on this request, in bytes.
///
/// Placed in the request extensions on the way in, and read by
/// [`read_limited`](crate::extract::read_limited) so that the 413 an extractor
/// produces names the limit that actually applies rather than the one it was
/// configured with. Absent when the slot is disabled, which is exactly when
/// there is no outer cap to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BodyCap(pub usize);

/// Wrap `service` in the body cap.
pub fn layer(config: &BodyLimitConfig, service: Route) -> Route {
    Route::new(BodyLimit {
        inner: service,
        config: *config,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct BodyLimit {
    inner: Route,
    config: BodyLimitConfig,
}

impl Service<Request> for BodyLimit {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let config = self.config;

        if config.trust_content_length
            && let Some(declared) = content_length(&req)
            && declared > config.max_bytes
        {
            // Refused before a byte is read, and the client is told the limit,
            // because it cannot discover it any other way.
            let response = Error::payload_too_large(config.max_bytes).into_response();
            return Box::pin(async move { Ok(response) });
        }

        req.extensions_mut().insert(BodyCap(config.max_bytes));
        let req = req.map(|body| axum::body::Body::new(Limited::new(body, config.max_bytes)));
        Box::pin(async move { inner.call(req).await })
    }
}

/// The declared body length, when the header is present and sane.
fn content_length(req: &Request) -> Option<usize> {
    req.headers()
        .get(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    #[test]
    fn the_default_matches_the_documented_limit() {
        assert_eq!(BodyLimitConfig::default().max_bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn bytes_render_in_binary_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2 MiB");
        assert_eq!(format_bytes(1536 * 1024 * 1024), "1.5 GiB");
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(BodyLimitConfig::default().summary(), "2 MiB");
    }

    /// Reads the whole body and reports how it went, so the test can tell a
    /// refused request from a truncated one.
    fn draining_route() -> Route {
        Route::new(tower::service_fn(|req: Request| async move {
            let outcome = match req.into_body().collect().await {
                Ok(collected) => format!("read {}", collected.to_bytes().len()),
                Err(_) => "body error".to_owned(),
            };
            Ok::<_, Infallible>(Response::new(axum::body::Body::from(outcome)))
        }))
    }

    async fn body_string(response: Response) -> String {
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .expect("body")
                .to_bytes()
                .to_vec(),
        )
        .expect("utf-8")
    }

    #[tokio::test]
    async fn a_declared_oversize_body_is_a_413_before_it_is_read() {
        let request = http::Request::builder()
            .header(http::header::CONTENT_LENGTH, "5000")
            .body(axum::body::Body::from(vec![0_u8; 5000]))
            .expect("request");

        let response = layer(&BodyLimitConfig::new(1024), draining_route())
            .oneshot(request)
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let json: serde_json::Value =
            serde_json::from_str(&body_string(response).await).expect("json");
        assert_eq!(json["max_bytes"], 1024);
    }

    #[tokio::test]
    async fn a_body_within_the_limit_arrives_intact() {
        let request = http::Request::builder()
            .body(axum::body::Body::from(vec![0_u8; 100]))
            .expect("request");

        let response = layer(&BodyLimitConfig::new(1024), draining_route())
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(body_string(response).await, "read 100");
    }

    #[tokio::test]
    async fn a_body_that_lies_about_its_length_is_still_cut_off() {
        // No `Content-Length` to check, so the streaming cap is the only
        // defence — and it holds.
        let request = http::Request::builder()
            .body(axum::body::Body::from(vec![0_u8; 5000]))
            .expect("request");

        let response = layer(&BodyLimitConfig::new(1024), draining_route())
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(body_string(response).await, "body error");
    }

    #[tokio::test]
    async fn trusting_content_length_can_be_turned_off() {
        let config = BodyLimitConfig {
            max_bytes: 1024,
            trust_content_length: false,
        };
        let request = http::Request::builder()
            .header(http::header::CONTENT_LENGTH, "5000")
            .body(axum::body::Body::from(vec![0_u8; 5000]))
            .expect("request");

        let response = layer(&config, draining_route())
            .oneshot(request)
            .await
            .expect("infallible");
        // Not refused up front — but the cap still stops it.
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_string(response).await, "body error");
    }

    #[tokio::test]
    async fn the_cap_is_announced_to_whatever_reads_the_body() {
        /// Answers with whichever cap the layer recorded.
        fn reporting_route() -> Route {
            Route::new(tower::service_fn(|req: Request| async move {
                let reported = req
                    .extensions()
                    .get::<BodyCap>()
                    .map_or_else(|| "none".to_owned(), |cap| cap.0.to_string());
                Ok::<_, Infallible>(Response::new(axum::body::Body::from(reported)))
            }))
        }

        let response = layer(&BodyLimitConfig::new(4096), reporting_route())
            .oneshot(
                http::Request::builder()
                    .body(axum::body::Body::empty())
                    .expect("request"),
            )
            .await
            .expect("infallible");
        assert_eq!(body_string(response).await, "4096");
    }
}
