//! `request_limits` — refuse an oversized request *head* before anything reads
//! it.
//!
//! The body has [`body_limit`](crate::middleware::body_limit); this is the same
//! idea one step earlier, for the two parts of a request that arrive before the
//! body does. It reads three numbers from [`Limits`]:
//!
//! | Limit | Measured | Answer |
//! | --- | --- | --- |
//! | `http.uri_max` | the request target, scheme and authority included | 414 |
//! | `http.header_max_count` | header *fields*, repeats counted separately | 431 |
//! | `http.header_max_bytes` | header names plus values, framing excluded | 431 |
//!
//! # Moso is not the first line of defence here, and says so
//!
//! By the time this layer runs, hyper has already read the head off the socket
//! and parsed it, under limits of its own: a maximum header count, a maximum
//! buffer, and — for HTTP/2 — `SETTINGS_MAX_HEADER_LIST_SIZE`. Nothing in a
//! Rust web framework can inspect a request target before the server that
//! framed it has allocated one.
//!
//! What this layer adds is **policy**, and the difference is visible to the
//! client. Hyper's rejection is a bare connection-level refusal with no body a
//! client can parse and no number it can act on. This one is an RFC 9457
//! problem document naming the limit that fired, generated from the operator's
//! own configuration, and it happens before routing, guards, extraction or the
//! timeout budget. A production deployment should still have a reverse proxy
//! enforcing the same bounds further out — the two do not conflict, and the
//! tighter one wins.
//!
//! # Why it sits where it sits
//!
//! Immediately inside [`Slot::CatchError`](crate::middleware::Slot::CatchError)
//! and outside [`Slot::Timeout`](crate::middleware::Slot::Timeout). Inside
//! `catch_error` so the refusal is logged like every other error and inherits
//! the request span; outside `timeout` because there is no sense starting a
//! thirty-second budget for a request that is about to be refused in a
//! microsecond.
//!
//! # It cannot disagree with the extractors
//!
//! Its configuration *is* the [`Limits`] snapshot every extractor reads from
//! [`RequestCtx::limits`](crate::RequestCtx::limits), and the check itself is
//! [`Limits::check_head`] — one home, one set of numbers. That is why there is
//! no `RequestLimitsConfig` type and no equivalent of
//! [`BodyCap`](crate::middleware::body_limit::BodyCap): the layer and the
//! request context read the same value, so there is nothing for them to
//! disagree about.

use std::convert::Infallible;
use std::task::{Context, Poll};

use tower::Service;

use crate::ctx::Limits;
use crate::middleware::body_limit::format_bytes;
use crate::router::Route;
use crate::{BoxFuture, IntoResponse, Request, Response};

/// A one-line summary of the head limits, for `moso middleware`.
///
/// Rendered with the same [`format_bytes`] the 413 uses, so the number an
/// operator reads in the stack listing is spelled the way the number in a
/// problem document is.
///
/// ```
/// use moso::ctx::Limits;
/// use moso::middleware::request_limits::summary;
///
/// assert_eq!(summary(&Limits::DEFAULT), "uri 8 KiB headers 100/16 KiB");
/// ```
#[must_use]
pub fn summary(limits: &Limits) -> String {
    format!(
        "uri {} headers {}/{}",
        format_bytes(limits.uri_max),
        limits.header_max_count,
        format_bytes(limits.header_max_bytes)
    )
}

/// Wrap `service` in the head-limit check.
pub fn layer(limits: &Limits, service: Route) -> Route {
    Route::new(RequestLimits {
        inner: service,
        limits: *limits,
    })
}

/// The service [`layer`] builds.
#[derive(Clone)]
struct RequestLimits {
    inner: Route,
    limits: Limits,
}

impl Service<Request> for RequestLimits {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);

        if let Err(error) = self.limits.check_head(req.uri(), req.headers()) {
            let response = error.into_response();
            return Box::pin(async move { Ok(response) });
        }

        Box::pin(async move { inner.call(req).await })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;
    use http_body_util::BodyExt as _;
    use tower::ServiceExt as _;

    /// Answers 200 with the path, so a test can tell "let through" from
    /// "refused" without reading a status twice.
    fn passthrough() -> Route {
        Route::new(tower::service_fn(|req: Request| async move {
            let path = req.uri().path().to_owned();
            Ok::<_, Infallible>(Response::new(axum::body::Body::from(path)))
        }))
    }

    async fn problem(response: Response) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("an RFC 9457 document")
    }

    fn request(uri: &str, headers: &[(&str, &str)]) -> Request {
        let mut builder = http::Request::builder().uri(uri);
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }
        builder.body(axum::body::Body::empty()).expect("a request")
    }

    #[test]
    fn the_summary_reads_like_the_configuration() {
        assert_eq!(summary(&Limits::DEFAULT), "uri 8 KiB headers 100/16 KiB");
    }

    #[tokio::test]
    async fn a_head_inside_the_limits_is_passed_through() {
        let response = layer(&Limits::DEFAULT, passthrough())
            .oneshot(request("/posts?page=2", &[("accept", "*/*")]))
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn an_over_long_target_is_a_414_problem_document() {
        let limits = Limits {
            uri_max: 16,
            ..Limits::DEFAULT
        };
        let response = layer(&limits, passthrough())
            .oneshot(request("/posts?search=something-much-longer", &[]))
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
        assert_eq!(
            response.headers()["content-type"],
            crate::error::problem::PROBLEM_CONTENT_TYPE
        );

        let document = problem(response).await;
        assert_eq!(document["status"], 414);
        assert_eq!(document["type"], "https://moso.rs/errors/uri-too-long");
        assert_eq!(document["max_bytes"], 16);
    }

    #[tokio::test]
    async fn too_many_header_fields_are_a_431_problem_document() {
        let limits = Limits {
            header_max_count: 2,
            ..Limits::DEFAULT
        };
        let response = layer(&limits, passthrough())
            .oneshot(request(
                "/posts",
                &[("x-a", "1"), ("x-b", "2"), ("x-c", "3")],
            ))
            .await
            .expect("infallible");

        assert_eq!(
            response.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );

        let document = problem(response).await;
        assert_eq!(document["status"], 431);
        assert_eq!(
            document["type"],
            "https://moso.rs/errors/header-fields-too-large"
        );
        assert_eq!(document["max_count"], 2);
    }

    #[tokio::test]
    async fn over_large_headers_are_a_431_reporting_the_byte_limit() {
        let limits = Limits {
            header_max_bytes: 16,
            ..Limits::DEFAULT
        };
        let response = layer(&limits, passthrough())
            .oneshot(request("/posts", &[("x-note", &"a".repeat(64))]))
            .await
            .expect("infallible");

        assert_eq!(
            response.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE
        );
        assert_eq!(problem(response).await["max_bytes"], 16);
    }

    #[tokio::test]
    async fn the_refusal_happens_before_the_inner_service_is_called() {
        /// Panics if it is ever reached, which is the assertion.
        fn never() -> Route {
            Route::new(tower::service_fn(|_req: Request| async move {
                panic!("the inner service must not run for a refused head");
                #[allow(unreachable_code)]
                Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
            }))
        }

        let limits = Limits {
            uri_max: 1,
            ..Limits::DEFAULT
        };
        let response = layer(&limits, never())
            .oneshot(request("/posts", &[]))
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::URI_TOO_LONG);
    }
}
