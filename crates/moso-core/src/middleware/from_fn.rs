//! The runtime half of `#[middleware]`.
//!
//! Writing a `Layer`, a `Service` and a hand-rolled `Future` is the single
//! most-cited Tower papercut, and 90% of the time the middleware is one
//! `async fn`. [`from_fn`] turns that function into a layer:
//!
//! ```
//! use moso::prelude::*;
//! use moso::deps::http::header::HOST;
//! use moso::middleware::Next;
//! use moso::{Request, Response};
//! # /// One customer's slice of the system.
//! # #[derive(Clone)] pub struct Tenant(String);
//! # impl Tenant {
//! #     fn from_host(host: &str) -> Option<Self> {
//! #         host.split('.').next().map(|s| Tenant(s.to_owned()))
//! #     }
//! # }
//! /// Resolve the tenant from the `Host` header.
//! #[moso::middleware]
//! async fn tenant(mut req: Request, next: Next) -> Result<Response> {
//!     let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
//!     let tenant = Tenant::from_host(host).ok_or_else(|| Error::not_found("tenant"))?;
//!     req.extensions_mut().insert(tenant);
//!     Ok(next.run(req).await)
//! }
//! # fn main() { assert_eq!(TenantLayer::NAME, "tenant"); }
//! ```
//!
//! `#[middleware]` expands to a named, `Clone` layer type whose `Layer::layer`
//! is [`from_fn`] applied to the function. Nothing here is macro-only: an
//! application can call [`from_fn`] with a closure and hand the result to
//! [`Router::layer`](crate::Router::layer) or
//! [`MiddlewareStack::insert_after`](crate::MiddlewareStack::insert_after).
//!
//! # Returning `Err` short-circuits
//!
//! The function returns [`Result<Response>`](crate::Result), so `?` works and
//! an [`Error`](crate::Error) becomes a problem document with the right status
//! — rendered through the same path as every other error, so it carries the
//! request id and honours the disclosure policy. There is no `IntoResponse`
//! juggling and no way to accidentally return a 200 with an error body.
//!
//! # `Depends<T>` is not available here
//!
//! Middleware runs before extraction. A [`Depends<T>`](crate::Depends) is
//! resolved from a [`RequestCtx`](crate::RequestCtx) that the extractor
//! pipeline has not built yet, so there is deliberately no impl anywhere that
//! would let one appear in a middleware signature — `#[middleware]` rejects the
//! parameter with a message naming the two things to do instead:
//!
//! ```text
//! error: `Depends<CurrentUser>` cannot be used in middleware
//!   = note: middleware runs before extractors, so request dependencies are not yet available
//!   = help: read a middleware-inserted value with `req.extensions()`, or move this logic into
//!           a `Dependency` impl and use it in the handler
//! ```
//!
//! `Inject<T>` *is* available, because a provider is application-lifetime and
//! exists before the first request: take it as a parameter and its
//! `ProviderReq` participates in boot validation exactly like a handler's.

use std::convert::Infallible;
use std::future::Future;
use std::task::{Context, Poll};

use tower::{Layer, Service};

use crate::error::Result;
use crate::middleware::Next;
use crate::router::Route;
use crate::{BoxFuture, IntoResponse, Request, Response};

/// Turn an `async fn(Request, Next) -> Result<Response>` into a layer.
///
/// The returned value is `Clone + Send + Sync + 'static` and implements
/// `tower::Layer<Route>`, which is exactly what every Moso API that takes a
/// layer asks for.
pub fn from_fn<F, Fut>(f: F) -> FromFn<F>
where
    F: Fn(Request, Next) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    FromFn { f }
}

/// The layer [`from_fn`] returns.
#[derive(Debug, Clone, Copy)]
pub struct FromFn<F> {
    f: F,
}

impl<F> Layer<Route> for FromFn<F>
where
    F: Clone,
{
    type Service = FromFnService<F>;

    fn layer(&self, inner: Route) -> Self::Service {
        FromFnService {
            f: self.f.clone(),
            inner,
        }
    }
}

/// The service [`FromFn`] builds.
#[derive(Debug, Clone)]
pub struct FromFnService<F> {
    f: F,
    inner: Route,
}

impl<F, Fut> Service<Request> for FromFnService<F>
where
    F: Fn(Request, Next) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Response>> + Send + 'static,
{
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, core::result::Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<core::result::Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request) -> Self::Future {
        // The polled-ready instance is the one that must be called; its clone
        // takes its place for the next request.
        let ready = self.inner.clone();
        let inner = core::mem::replace(&mut self.inner, ready);
        let f = self.f.clone();

        Box::pin(async move {
            // `from_route` rather than `Next::new`, because `inner` is already
            // the erased service `Next` holds: re-boxing it would cost an
            // allocation to arrive back where it started.
            let next = Next::from_route(inner);
            Ok(match f(req, next).await {
                Ok(response) => response,
                Err(error) => error.into_response(),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
    use http::StatusCode;
    use tower::ServiceExt as _;

    /// Echoes back whatever a middleware put in the extensions.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Tenant(&'static str);

    fn echo_tenant() -> Route {
        Route::new(tower::service_fn(|req: Request| async move {
            let tenant = req
                .extensions()
                .get::<Tenant>()
                .copied()
                .unwrap_or(Tenant("none"));
            Ok::<_, Infallible>(Response::new(axum::body::Body::from(tenant.0)))
        }))
    }

    async fn body_string(response: Response) -> String {
        use http_body_util::BodyExt as _;
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
    async fn a_function_middleware_sees_the_request_and_the_response() {
        let layer = from_fn(|mut req: Request, next: Next| async move {
            req.extensions_mut().insert(Tenant("acme"));
            let mut response = next.run(req).await;
            response
                .headers_mut()
                .insert("x-tenant", http::HeaderValue::from_static("acme"));
            Ok(response)
        });

        let service = Layer::layer(&layer, echo_tenant());
        let response = service
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        assert_eq!(response.headers()["x-tenant"], "acme");
        assert_eq!(body_string(response).await, "acme");
    }

    /// Acceptance criterion 3: returning `Err` yields a problem response with
    /// the right status.
    #[tokio::test]
    async fn returning_err_short_circuits_with_a_problem() {
        let layer = from_fn(|_req: Request, _next: Next| async move {
            Err(Error::forbidden("this tenant is suspended"))
        });

        let response = Layer::layer(&layer, echo_tenant())
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/problem+json")
        );
        let body = body_string(response).await;
        assert!(body.contains("this tenant is suspended"), "{body}");
    }

    #[tokio::test]
    async fn the_inner_stack_is_not_run_when_the_middleware_refuses() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let reached = Arc::new(AtomicBool::new(false));
        let inner = {
            let reached = Arc::clone(&reached);
            Route::new(tower::service_fn(move |_req: Request| {
                let reached = Arc::clone(&reached);
                async move {
                    reached.store(true, Ordering::Relaxed);
                    Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
                }
            }))
        };

        let layer =
            from_fn(|_req: Request, _next: Next| async move { Err(Error::unauthenticated()) });
        Layer::layer(&layer, inner)
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        assert!(!reached.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn it_composes_through_the_stack_like_any_other_layer() {
        use crate::middleware::{MiddlewareStack, Slot};

        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::CatchError);
        stack.insert_after(
            Slot::CatchError,
            "tenant",
            from_fn(|mut req: Request, next: Next| async move {
                req.extensions_mut().insert(Tenant("acme"));
                Ok(next.run(req).await)
            }),
        );

        let response = stack
            .compose(echo_tenant())
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        assert_eq!(body_string(response).await, "acme");
    }
}
