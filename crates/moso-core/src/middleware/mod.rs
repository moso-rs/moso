//! The middleware stack: named slots, one composition, and guards.
//!
//! Moso does not invent a middleware abstraction — Tower's `Service`/`Layer`
//! *is* the abstraction, and the whole `tower-http` ecosystem works unmodified.
//! What this module adds is:
//!
//! 1. a **default stack** that is correct and ordered without the reader having
//!    to know Tower,
//! 2. **named slots**, so "put CORS before auth" is one line rather than a
//!    rewrite,
//! 3. **[`Guard`]** — middleware that also documents itself in OpenAPI.
//!
//! # The default stack
//!
//! Outermost first.
//!
//! | # | [`Slot`] | Default | Notes |
//! | --- | --- | --- | --- |
//! | 1 | `CatchPanic` | on | 500 problem, logged, counter incremented |
//! | 2 | `RequestId` | on | reads `x-request-id` or generates a ULID |
//! | 3 | `Trace` | on | opens the span every later log inherits |
//! | 4 | `SensitiveHeaders` | on | marks auth and cookie headers redacted |
//! | 5 | `CatchError` | on | `Error` → problem+json; the logging boundary |
//! | 6 | `RequestLimits` | on | 414 and 431 problems, from the head alone |
//! | 7 | `Timeout` | 30 s | 504 problem |
//! | 8 | `BodyLimit` | 2 MiB | 413 problem |
//! | 9 | `NormalizePath` | trim trailing slash | configurable, including off |
//! | 10 | `Cors` | **off** | permissive CORS is never a default |
//! | 11 | `SecurityHeaders` | on | HSTS, nosniff, Referrer-Policy, frame-ancestors |
//! | 12 | `Compression` | on | br > gzip; skips already-compressed types and SSE |
//! | 13 | `RateLimit` | off | opt-in, usually per-router |
//! | 14 | `Session` | off | provided by the auth battery |
//! | 15 | `Metrics` | off | recorded **after** routing, so the label is the pattern |
//!
//! Two ordering choices people get wrong, and the reasons:
//!
//! - **`CatchError` inside `Trace`**, so the error log carries the span; and
//!   **outside `Timeout`**, so a timeout renders as a problem document rather
//!   than as a dropped connection.
//! - **`Metrics` after routing**, so the `route` label is `/users/{id}` and not
//!   one time series per user id. That is the classic cardinality explosion,
//!   and it is a production incident, not a tidiness matter.
//!
//! # Composition happens once
//!
//! The stack is folded into a single service at boot, not per request and not
//! per route. [`MiddlewareStack::compose`] walks the enabled entries from the
//! inside out, wrapping one [`Route`] each time, and the result is the service
//! the listener calls. Per-request cost of the default configuration is
//! budgeted at under 3 µs, dominated by span creation and header work.
//!
//! Because [`CustomLayer::apply`] is `Route -> Route`, every layer boundary
//! re-boxes: a layer's own future is a [`BoxFuture`]. That is one small
//! allocation per enabled layer per request, and it is the price of a stack
//! that can be reordered at runtime and printed by `moso middleware`. The
//! alternative — a type-level `ServiceBuilder` chain — cannot be reordered by a
//! configuration file and cannot be introspected at all.
//!
//! # Where the composed stack is installed, and how it still knows the route
//!
//! The composed stack is installed **outside** Axum's router, as the fallback
//! service of an outer router that also carries `/healthz`, `/readyz`,
//! `/openapi.json` and `/docs`. It has to be: [`Slot::NormalizePath`] rewrites
//! the URI, which only means anything before matching.
//!
//! Three slots nevertheless want the matched route *pattern* rather than the raw
//! path — [`Slot::Trace`] records it as a span field, [`Slot::Metrics`] labels
//! its series with it, [`Slot::Timeout`] matches its exemptions against it — and
//! Axum publishes the pattern only *during* routing. So the stack resolves it
//! itself, once, at the outside: [`RoutePatterns`] is built at boot from the
//! registered routes and [`route_pattern::layer`] publishes the answer as a
//! [`ResolvedRoute`] extension before the outermost slot runs. All three slots
//! then read that one value.
//!
//! Anything Moso cannot see the pattern of — a
//! [`Router::mount_axum`](crate::Router::mount_axum) mount, a static file mount,
//! a path that matches nothing — resolves to [`UNMATCHED_ROUTE`], one bounded
//! series rather than one per path. [`route_pattern`] is where the reasoning
//! lives, including why moving the stack inside routing is the wrong repair.
//!
//! # Middleware cannot use `Depends`
//!
//! Middleware runs before extraction, so request dependencies do not exist yet.
//! Values a middleware computes travel to handlers as request extensions, read
//! back with [`Extension<T>`](crate::extract::Extension) or inside a
//! [`Dependency`](crate::Dependency) impl. `#[middleware]` rejects a `Depends`
//! parameter with a message saying exactly that, and nothing in this module
//! provides an impl that would let one compile by accident.

pub mod body_limit;
pub mod catch_error;
pub mod catch_panic;
pub mod compression;
pub mod cors;
pub mod from_fn;
pub mod guard;
pub mod metrics;
pub mod normalize_path;
pub mod request_id;
pub mod request_limits;
pub mod route_pattern;
pub mod security_headers;
pub mod sensitive_headers;
pub mod timeout;
pub mod trace;

use std::borrow::Cow;
use std::convert::Infallible;
use std::sync::Arc;

use moso_openapi::OperationBuilder;

use crate::config::Profile;
use crate::ctx::Limits;
use crate::error::{BootError, Result};
use crate::http_config::HttpConfig;
use crate::router::{DynGuard, Route};
use crate::{BoxFuture, IntoResponse, Request, RequestCtx, Response};

pub use crate::middleware::body_limit::BodyLimitConfig;
pub use crate::middleware::catch_error::CatchErrorConfig;
pub use crate::middleware::catch_panic::CatchPanicConfig;
pub use crate::middleware::compression::{CompressionConfig, Encoding};
pub use crate::middleware::cors::CorsConfig;
pub use crate::middleware::from_fn::{FromFn, FromFnService, from_fn};
pub use crate::middleware::guard::RequireHeader;
pub use crate::middleware::metrics::{MetricsConfig, MetricsRecorder, RequestSample};
pub use crate::middleware::normalize_path::{NormalizePathConfig, TrailingSlash};
pub use crate::middleware::request_id::{RequestIdConfig, RequestIdSource};
pub use crate::middleware::route_pattern::{ResolvedRoute, RoutePatterns};
pub use crate::middleware::security_headers::{ReferrerPolicy, SecurityHeadersConfig};
pub use crate::middleware::sensitive_headers::SensitiveHeadersConfig;
pub use crate::middleware::timeout::TimeoutConfig;
pub use crate::middleware::trace::TraceConfig;

// ---------------------------------------------------------------------------
// Slot
// ---------------------------------------------------------------------------

/// A named position in the default stack.
///
/// Naming the positions is what makes the stack editable without a rewrite:
/// `s.disable(Slot::Compression)` and `s.insert_after(Slot::Trace, "tenant", l)`
/// say what they mean, and `moso middleware` prints the result.
///
/// ```
/// use moso::middleware::{MiddlewareStack, Slot};
/// use std::time::Duration;
///
/// let mut stack = MiddlewareStack::default();
///
/// // Every position has a name, so an edit says what it means.
/// stack.disable(Slot::Compression);
/// stack.timeout(Duration::from_secs(10));
/// ```
///
/// The order is fixed and outermost-first: `CatchPanic`, `RequestId`, `Trace`,
/// `SensitiveHeaders`, `CatchError`, and so on inwards. A custom layer goes in
/// relative to one of these rather than at an index, so inserting one does not
/// renumber anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Slot {
    /// Turns a panic into a 500 problem and keeps the connection alive.
    CatchPanic,
    /// Assigns the correlation id.
    RequestId,
    /// Opens the tracing span.
    Trace,
    /// Marks authorization and cookie headers as sensitive for tracing.
    SensitiveHeaders,
    /// Converts an `Error` into a problem document. The logging boundary.
    CatchError,
    /// Rejects a request whose URI or headers exceed the configured limits.
    RequestLimits,
    /// Fails a request that outlives its budget.
    Timeout,
    /// Rejects an oversized body before it is read.
    BodyLimit,
    /// Applies the trailing-slash policy.
    NormalizePath,
    /// Cross-origin resource sharing. Off unless configured.
    Cors,
    /// HSTS, `X-Content-Type-Options`, `Referrer-Policy`, CSP.
    SecurityHeaders,
    /// Response compression.
    Compression,
    /// Request rate limiting.
    RateLimit,
    /// Session loading.
    Session,
    /// Request metrics, recorded after routing.
    Metrics,
}

impl Slot {
    /// Every slot, outermost first. The canonical order of the default stack.
    pub const ORDER: [Slot; 15] = [
        Slot::CatchPanic,
        Slot::RequestId,
        Slot::Trace,
        Slot::SensitiveHeaders,
        Slot::CatchError,
        Slot::RequestLimits,
        Slot::Timeout,
        Slot::BodyLimit,
        Slot::NormalizePath,
        Slot::Cors,
        Slot::SecurityHeaders,
        Slot::Compression,
        Slot::RateLimit,
        Slot::Session,
        Slot::Metrics,
    ];

    /// The snake_case name `moso middleware` prints.
    pub const fn as_str(self) -> &'static str {
        match self {
            Slot::CatchPanic => "catch_panic",
            Slot::RequestId => "request_id",
            Slot::Trace => "trace",
            Slot::SensitiveHeaders => "sensitive_headers",
            Slot::CatchError => "catch_error",
            Slot::RequestLimits => "request_limits",
            Slot::Timeout => "timeout",
            Slot::BodyLimit => "body_limit",
            Slot::NormalizePath => "normalize_path",
            Slot::Cors => "cors",
            Slot::SecurityHeaders => "security_headers",
            Slot::Compression => "compression",
            Slot::RateLimit => "rate_limit",
            Slot::Session => "session",
            Slot::Metrics => "metrics",
        }
    }

    /// Its index in [`Slot::ORDER`].
    pub fn position(self) -> usize {
        Slot::ORDER
            .iter()
            .position(|slot| *slot == self)
            .unwrap_or(usize::MAX)
    }

    /// Whether this slot has a built-in implementation.
    ///
    /// [`Slot::RateLimit`] and [`Slot::Session`] do not: they are positions
    /// reserved for batteries, which fill them with
    /// [`MiddlewareStack::replace_custom`] — or with
    /// [`MiddlewareStack::replace`], if what they ship is a plain
    /// `tower::Layer<Route>`. Enabling one with nothing in it is a boot error
    /// rather than a silent no-op — see [`MiddlewareStack::validate`].
    pub const fn has_builtin(self) -> bool {
        !matches!(self, Slot::RateLimit | Slot::Session)
    }
}

impl core::fmt::Display for Slot {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// CustomLayer
// ---------------------------------------------------------------------------

/// A Tower layer with its type erased.
///
/// Tower's `Layer` is not usefully object-safe — `Layer<S>` is generic over the
/// service it wraps — so the stack stores this instead: a trait that applies a
/// layer to the one concrete service type routes are erased to.
///
/// Application code never implements this directly. [`Router::layer`] and
/// [`MiddlewareStack::insert_after`] wrap a real `tower::Layer` in an adapter,
/// and [`layer_fn`] builds one from a closure:
///
/// ```
/// use moso::middleware::{CustomLayer, layer_fn};
/// use std::time::Duration;
/// use tower_http::timeout::TimeoutLayer;
///
/// // `layer_fn` erases any real `tower::Layer<moso::Route>` into one of these.
/// let erased = layer_fn("timeout", TimeoutLayer::new(Duration::from_secs(5)));
///
/// assert_eq!(erased.name(), "timeout");
/// ```
///
/// [`Router::layer`]: crate::Router::layer
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a middleware layer",
    note = "pass a `tower::Layer<moso::Route>` — everything in `tower-http` qualifies",
    note = "help: for the function-shaped form, write `#[moso::middleware]` above an \
            `async fn name(req: Request, next: Next) -> Result<Response>`"
)]
pub trait CustomLayer: Send + Sync + 'static {
    /// The name `moso middleware` prints.
    fn name(&self) -> &'static str;

    /// Wrap `service`.
    ///
    /// Called once per route at boot, never per request.
    fn apply(&self, service: Route) -> Route;

    /// A one-line summary of the configuration, for `moso middleware`.
    fn summary(&self) -> String {
        String::new()
    }
}

/// The [`RequestCtx`] a `#[middleware]` with leading extractor parameters needs.
///
/// Middleware runs *outside* the router, so the context the router builds after
/// matching does not exist yet. This recovers one of the two contexts that can
/// legitimately be in scope at that point:
///
/// 1. one an **inner** layer already installed — a middleware added with
///    [`Router::layer`](crate::Router::layer) sits inside the route service, so
///    by the time it runs the router has already inserted the real context, and
///    that one is used unchanged (it carries the matched path and the
///    request-scoped dependency cache);
/// 2. otherwise one built over the `Arc<AppState>` that
///    [`App::build`](crate::app::AppBuilder::build) inserts on the way in.
///
/// The failure case is a router that no [`App`](crate::App) ever mounted: there
/// is no provider map to inject from, so this returns an error rather than
/// panicking, and the generated middleware turns it into a 500.
///
/// This is runtime support for `#[moso::middleware]`, re-exported through
/// `moso::__private`. Application code wants
/// [`extract::ctx_from_parts`](crate::extract::ctx_from_parts) instead, which
/// asks only the first question.
pub fn middleware_ctx(parts: &http::request::Parts) -> Result<RequestCtx> {
    crate::router::request_context(parts).ok_or_else(|| {
        crate::Error::internal_msg(
            "no application state in the request extensions: this middleware ran outside an \
             application, so there is no provider map to extract from. A `#[moso::middleware]` \
             with leading extractor parameters must be applied to a router that an `App` mounts",
        )
    })
}

/// Wrap a `tower::Layer` as a [`CustomLayer`].
///
/// The bounds are the ones `axum::Router::layer` uses, so anything that works
/// there works here.
pub fn layer_fn<L>(name: &'static str, layer: L) -> Arc<dyn CustomLayer>
where
    L: tower::Layer<Route> + Clone + Send + Sync + 'static,
    L::Service:
        tower::Service<Request, Error = core::convert::Infallible> + Clone + Send + Sync + 'static,
    <L::Service as tower::Service<Request>>::Response: crate::IntoResponse + 'static,
    <L::Service as tower::Service<Request>>::Future: Send + 'static,
{
    Arc::new(TowerLayer { name, layer })
}

/// The adapter [`layer_fn`] returns: a real `tower::Layer` plus its name.
struct TowerLayer<L> {
    name: &'static str,
    layer: L,
}

impl<L> CustomLayer for TowerLayer<L>
where
    L: tower::Layer<Route> + Clone + Send + Sync + 'static,
    L::Service: tower::Service<Request, Error = Infallible> + Clone + Send + Sync + 'static,
    <L::Service as tower::Service<Request>>::Response: IntoResponse + 'static,
    <L::Service as tower::Service<Request>>::Future: Send + 'static,
{
    fn name(&self) -> &'static str {
        self.name
    }

    fn apply(&self, service: Route) -> Route {
        use tower::ServiceExt as _;

        // `map_response` is what lets a layer whose service answers with, say,
        // `Response<CompressionBody<_>>` be re-boxed as a `Route`: the closure
        // is `IntoResponse`, which every Axum response type satisfies.
        Route::new(
            self.layer
                .layer(service)
                .map_response(IntoResponse::into_response),
        )
    }
}

// ---------------------------------------------------------------------------
// Next
// ---------------------------------------------------------------------------

/// The rest of the stack, from inside one middleware.
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::Next;
/// use moso::{Request, Response};
/// # /// One customer's slice of the system.
/// # #[derive(Clone)] pub struct Tenant(String);
/// # impl Tenant {
/// #     fn from_host(headers: &moso::deps::http::HeaderMap) -> Result<Self> {
/// #         let _ = headers;
/// #         Ok(Tenant("acme".to_owned()))
/// #     }
/// # }
/// /// Resolve the tenant before anything downstream runs.
/// #[moso::middleware]
/// async fn tenant(mut req: Request, next: Next) -> Result<Response> {
///     let tenant = Tenant::from_host(req.headers())?;
///     req.extensions_mut().insert(tenant);
///     Ok(next.run(req).await)
/// }
/// # fn main() { assert_eq!(TenantLayer::NAME, "tenant"); }
/// ```
///
/// Returning `Err(Error)` short-circuits with a problem response, which is why
/// `#[middleware]` functions return `Result<Response>` rather than juggling
/// `IntoResponse` by hand.
///
/// # A middleware cannot take `Depends<T>`
///
/// Middleware runs before extraction. A [`Depends<T>`](crate::Depends) is
/// resolved by the extractor pipeline from a [`RequestCtx`] that does not exist
/// yet, so there is deliberately no impl that would let one appear in a
/// `#[middleware]` signature — the attribute rejects it with a message naming
/// the two things to do instead: read a value a *previous* middleware inserted
/// with `req.extensions()`, or move the logic into an
/// [`impl Dependency`](crate::Dependency) and take it in the handler.
pub struct Next {
    inner: Route,
}

impl Next {
    /// Build from the inner service. Called by the code `#[middleware]`
    /// generates.
    pub fn new<S>(inner: S) -> Self
    where
        S: tower::Service<Request, Error = core::convert::Infallible>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Response: crate::IntoResponse + 'static,
        S::Future: Send + 'static,
    {
        use tower::ServiceExt as _;

        Next {
            inner: Route::new(inner.map_response(IntoResponse::into_response)),
        }
    }

    /// Build from an already-erased route, with no second boxing.
    pub(crate) fn from_route(inner: Route) -> Self {
        Next { inner }
    }

    /// Run the rest of the stack.
    pub async fn run(self, req: Request) -> Response {
        use tower::ServiceExt as _;

        match self.inner.oneshot(req).await {
            Ok(response) => response,
            // `Route`'s error type is `Infallible`, so this arm is uninhabited
            // and the compiler proves it rather than us asserting it.
            Err(never) => match never {},
        }
    }
}

impl core::fmt::Debug for Next {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Next")
    }
}

// ---------------------------------------------------------------------------
// Guard
// ---------------------------------------------------------------------------

/// Middleware that documents itself.
///
/// The gap in every framework: a middleware that can return 403 makes the
/// OpenAPI document wrong, because nothing tells the document about it. A guard
/// closes that gap by contributing its responses and security requirements to
/// every operation on the router it is applied to.
///
/// Guards run **after** routing and **before** extraction, so they can see path
/// parameters but not extracted values. Every built-in security middleware —
/// authentication, permission checks, CSRF, rate limiting — is a guard rather
/// than a bare layer, for exactly this reason.
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::Guard;
/// use moso::openapi::ResponseSpec;
/// use moso::{BoxFuture, Guard as _};
/// use moso::deps::http::request::Parts;
/// # /// Liveness.
/// # #[endpoint] async fn healthz() -> Result<moso::response::NoContent> {
/// #     Ok(moso::response::NoContent) }
/// /// Only let a request through when it carries an internal marker.
/// #[derive(Clone)]
/// pub struct RequireInternal;
///
/// impl Guard for RequireInternal {
///     fn describe(&self, op: &mut OperationBuilder) {
///         op.response(403, ResponseSpec::problem("Internal callers only"));
///     }
///
///     fn check<'a>(&'a self, parts: &'a Parts, _ctx: &'a RequestCtx)
///         -> BoxFuture<'a, Result<()>>
///     {
///         Box::pin(async move {
///             if parts.headers.contains_key("x-internal") {
///                 Ok(())
///             } else {
///                 Err(Error::forbidden("internal callers only"))
///             }
///         })
///     }
/// }
///
/// # fn main() {
/// let router = Router::new()
///     .get("/healthz", moso::ep!(healthz))
///     .guard(RequireInternal);
/// assert_eq!(router.entries()[0].guards.len(), 1);
/// # }
/// ```
///
/// [`RequireHeader`] is the worked reference implementation.
///
/// # Why `describe` takes `&self`
///
/// A guard is usually *configured* — with a permission, a header name, a rate —
/// and the configuration is exactly what the document should show. An
/// associated function could not see it, and would also make the trait
/// dyn-incompatible, which the route table needs.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a guard",
    label = "not a guard",
    note = "a guard is `Clone + Send + Sync + 'static` and implements `check` and `describe`",
    note = "help: write `impl Guard for {Self}` with a `describe(&self, op)` and a \
            `check(&self, parts, ctx)`",
    note = "for middleware that does not affect the API contract, use a `tower::Layer` with \
            `Router::layer` instead"
)]
pub trait Guard: Clone + Send + Sync + 'static {
    /// Contribute the responses and security requirements this guard implies.
    fn describe(&self, op: &mut OperationBuilder);

    /// Allow the request, or reject it with an [`Error`](crate::Error).
    ///
    /// Boxed rather than an `async fn` so the trait is dyn-compatible: the route
    /// table stores guards as trait objects.
    fn check<'a>(
        &'a self,
        parts: &'a http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<()>>;
}

// Without this, a type that is not a guard makes the compiler suggest
// "consider implementing `DynGuard`" — the internal half nobody writes — and
// bury the `Guard` bound that is actually unsatisfied. `do_not_recommend` takes
// this impl out of the suggestion set, so the reported bound is `Guard`.
#[diagnostic::do_not_recommend]
impl<G: Guard> DynGuard for G {
    fn check_dyn<'a>(
        &'a self,
        parts: &'a http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, Result<()>> {
        <G as Guard>::check(self, parts, ctx)
    }

    fn describe_dyn(&self, op: &mut OperationBuilder) {
        <G as Guard>::describe(self, op);
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// The route *pattern* this request matched, from whichever half knows it.
///
/// Two things can know, and they agree:
///
/// 1. **Axum**, for a layer that sits *inside* routing — anything added with
///    [`Router::layer`](crate::Router::layer), and every per-route layer. Its
///    `MatchedPath` is the router's own answer, so it is preferred whenever it
///    is there.
/// 2. **[`route_pattern`]**, for the composed stack, which sits outside routing
///    and therefore resolves the pattern itself before the outermost slot runs.
///
/// `None` when neither knows: a path outside the route table, a
/// [`Router::mount_axum`](crate::Router::mount_axum) mount whose patterns Moso
/// cannot see, or a fallback. Never the raw path: a raw path used as a metric
/// label or a timeout exemption is how you get a million time series, or an
/// exemption an attacker can widen with a crafted URL.
pub(crate) fn matched_route(extensions: &http::Extensions) -> Option<&str> {
    if let Some(matched) = extensions.get::<axum::extract::MatchedPath>() {
        return Some(matched.as_str());
    }
    extensions.get::<ResolvedRoute>().map(ResolvedRoute::as_str)
}

/// The `route` label used when there is no matched pattern.
///
/// A metric label and a log field have to be bounded, and a raw path is not, so
/// every request without a pattern is recorded under this one fixed string: a
/// path the route table does not cover, a path served by a
/// [`Router::mount_axum`](crate::Router::mount_axum) mount, and a path a
/// redirecting `normalize_path` answers before routing. A [`MetricsRecorder`]
/// comparing against it is asking "did this bypass the route table".
pub const UNMATCHED_ROUTE: &str = "<unmatched>";

// ---------------------------------------------------------------------------
// MiddlewareStack
// ---------------------------------------------------------------------------

/// The ordered, named middleware stack.
///
/// Every application gets the same stack in the same order; this is how you
/// adjust it. In an application the closure is handed to
/// `App::new(cfg).with_middleware(…)` rather than driven directly.
///
/// ```
/// use moso::prelude::*;
/// use moso::middleware::{MiddlewareStack, Slot};
/// use std::time::Duration;
/// use moso::middleware::Next;
/// use moso::{Request, Response};
/// # /// Resolve the tenant.
/// # #[moso::middleware]
/// # async fn tenant(req: Request, next: Next) -> Result<Response> { Ok(next.run(req).await) }
/// # fn main() {
/// let mut stack = MiddlewareStack::default();
/// stack.timeout(Duration::from_secs(10));
/// stack.body_limit(1 << 20);
/// stack.disable(Slot::Compression);
/// stack.insert_after(Slot::Trace, "tenant", TenantLayer::new());
/// stack.security_headers(|h| { h.csp("default-src \'self\'"); });
/// # }
/// ```
pub struct MiddlewareStack {
    entries: Vec<StackEntry>,
    config: StackConfig,
}

/// Every built-in slot's configuration, in one place.
///
/// Private: the entries carry a rendered `summary` for display, and the typed
/// setters keep the two in step. Splitting the configuration out of
/// [`StackEntry`] is what lets `describe()` stay a cheap clone of plain data.
#[derive(Debug, Clone, Default)]
struct StackConfig {
    catch_panic: CatchPanicConfig,
    request_id: RequestIdConfig,
    trace: TraceConfig,
    sensitive_headers: SensitiveHeadersConfig,
    catch_error: CatchErrorConfig,
    /// The head bounds `request_limits` enforces.
    ///
    /// The whole [`Limits`] snapshot rather than the three numbers it reads, so
    /// the layer and [`RequestCtx::limits`](crate::RequestCtx::limits) cannot
    /// hold different opinions about what `http.uri_max` is.
    request_limits: Limits,
    timeout: TimeoutConfig,
    body_limit: BodyLimitConfig,
    normalize_path: NormalizePathConfig,
    cors: CorsConfig,
    security_headers: SecurityHeadersConfig,
    compression: CompressionConfig,
    metrics: MetricsConfig,
    /// Paths kept out of the access log and out of the metrics, as prefixes.
    ///
    /// The probes and the docs UI: they are polled every few seconds by
    /// infrastructure nobody is debugging, and at one line each they are the
    /// bulk of a quiet service's log volume.
    silent: Arc<[String]>,
    /// Which of the above the application set for itself.
    explicit: Explicit,
}

/// Which settings the application chose, as opposed to inherited.
///
/// [`MiddlewareStack::configure`] derives values from the profile and the
/// `[http]` section; it must not undo a deliberate `with_middleware` edit, and
/// the two happen in the opposite order to the one they read in (the edit runs
/// on the builder, `configure` runs inside `build`). Recording the fact of the
/// call is the smallest thing that resolves that.
#[derive(Debug, Clone, Copy, Default)]
struct Explicit {
    /// [`MiddlewareStack::catch_panic`] was called.
    catch_panic: bool,
    /// [`MiddlewareStack::catch_error`] was called.
    catch_error: bool,
    /// [`MiddlewareStack::request_limits`] was called.
    request_limits: bool,
    /// [`MiddlewareStack::timeout`] was called.
    timeout: bool,
    /// [`MiddlewareStack::body_limit`] was called.
    body_limit: bool,
    /// [`MiddlewareStack::security_headers`] was called.
    security_headers: bool,
}

/// One entry in the stack, as [`MiddlewareStack::describe`] reports it.
#[derive(Clone)]
pub struct StackEntry {
    /// The slot, or `None` for a custom layer.
    pub slot: Option<Slot>,
    /// The name printed by `moso middleware`.
    pub name: Cow<'static, str>,
    /// Whether this entry will be applied.
    pub enabled: bool,
    /// A one-line summary of the configuration: `timeout 30s`, `br,gzip`.
    pub summary: String,
    /// The custom layer, when this is not a built-in slot.
    pub layer: Option<Arc<dyn CustomLayer>>,
}

impl core::fmt::Debug for StackEntry {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("StackEntry")
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("summary", &self.summary)
            .finish_non_exhaustive()
    }
}

impl core::fmt::Debug for MiddlewareStack {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MiddlewareStack")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl Default for MiddlewareStack {
    fn default() -> Self {
        Self::standard()
    }
}

impl MiddlewareStack {
    /// The default stack, in the documented order.
    pub fn standard() -> Self {
        let config = StackConfig::default();
        let entries = Slot::ORDER
            .iter()
            .map(|slot| entry_for(*slot, &config))
            .collect();
        MiddlewareStack { entries, config }
    }

    /// An empty stack, for an application that composes its own.
    ///
    /// Nothing is inserted, including `catch_panic` and `catch_error`, so a
    /// handler's `Error` will not become a problem document. Use it when you
    /// know exactly what you are replacing.
    pub fn bare() -> Self {
        Self {
            entries: Vec::new(),
            config: StackConfig::default(),
        }
    }

    /// Adopt the profile's and the HTTP section's defaults.
    ///
    /// [`AppBuilder::build`](crate::AppBuilder::build) calls it once, with the
    /// configuration it resolved. It fills in the four places where the stack
    /// and the configuration would otherwise be able to disagree:
    ///
    /// - `catch_error`'s disclosure policy, from `http.expose_internal_errors`
    ///   and the profile,
    /// - `timeout` from `http.timeout` and `body_limit` from `http.body_max`,
    /// - `catch_panic` and `security_headers` from the profile (`dev` renders
    ///   panic details and omits HSTS),
    /// - the probe and docs paths, which are kept out of the access log.
    ///
    /// **A setting the application made explicitly is never overwritten.** The
    /// typed setters — [`timeout`](Self::timeout), [`body_limit`](Self::body_limit),
    /// [`catch_panic`](Self::catch_panic), [`catch_error`](Self::catch_error),
    /// [`security_headers`](Self::security_headers) — each record that they were
    /// called, and this skips what they touched. That is what lets
    /// `with_middleware` run before the configuration is known (it runs on the
    /// builder, `build()` runs later) and still win, which is the order an
    /// application reads.
    pub fn configure(&mut self, profile: Profile, http: &HttpConfig) -> &mut Self {
        let explicit = self.config.explicit;
        if !explicit.catch_panic {
            self.config.catch_panic = CatchPanicConfig::for_profile(profile);
        }
        if !explicit.security_headers {
            self.config.security_headers = SecurityHeadersConfig::for_profile(profile);
        }
        if !explicit.catch_error {
            self.config.catch_error.problem = crate::error::problem::ProblemOptions {
                expose_internal_errors: http.expose_internal_errors,
                profile,
                html_errors: true,
                type_base: crate::error::ERROR_TYPE_BASE,
            };
        }
        if !explicit.request_limits {
            self.config.request_limits = http.limits();
        }
        if !explicit.timeout {
            self.config.timeout.timeout = http.timeout;
        }
        if !explicit.body_limit {
            self.config.body_limit.max_bytes = http.body_max;
        }

        // The probe paths join whatever the application already silenced,
        // rather than replacing it: `silence` may well have been called first.
        let mut silent = vec![
            http.health_path.clone(),
            http.ready_path.clone(),
            http.docs_path.clone(),
            http.openapi_path.clone(),
        ];
        for path in self.config.silent.iter() {
            if !silent.iter().any(|known| known == path) {
                silent.push(path.clone());
            }
        }
        self.config.silent = Arc::from(silent);

        self.refresh();
        self
    }

    /// Keep `path` and everything under it out of the access log and the
    /// metrics.
    ///
    /// The probes and the docs UI are silenced by [`MiddlewareStack::configure`]
    /// already; this is for an application's own health surface.
    pub fn silence(&mut self, path: impl Into<String>) -> &mut Self {
        let mut paths = self.config.silent.to_vec();
        paths.push(path.into());
        self.config.silent = Arc::from(paths);
        self
    }

    /// Turn a slot off.
    pub fn disable(&mut self, slot: Slot) -> &mut Self {
        if let Some(entry) = self.entry_mut(slot) {
            entry.enabled = false;
        }
        self
    }

    /// Turn a slot on, with its default configuration if it has none.
    pub fn enable(&mut self, slot: Slot) -> &mut Self {
        match self.index_of(slot) {
            Some(index) => self.entries[index].enabled = true,
            None => {
                let mut entry = entry_for(slot, &self.config);
                entry.enabled = true;
                let at = self.canonical_index(slot);
                self.entries.insert(at, entry);
            }
        }
        self
    }

    /// Insert a custom layer immediately outside `slot`.
    pub fn insert_before<L>(&mut self, slot: Slot, name: &'static str, layer: L) -> &mut Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<Request, Error = core::convert::Infallible>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<Request>>::Response: crate::IntoResponse + 'static,
        <L::Service as tower::Service<Request>>::Future: Send + 'static,
    {
        let at = self.index_of(slot).unwrap_or(self.entries.len());
        self.insert_custom(at, name, layer_fn(name, layer));
        self
    }

    /// Insert a custom layer immediately inside `slot`.
    pub fn insert_after<L>(&mut self, slot: Slot, name: &'static str, layer: L) -> &mut Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<Request, Error = core::convert::Infallible>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<Request>>::Response: crate::IntoResponse + 'static,
        <L::Service as tower::Service<Request>>::Future: Send + 'static,
    {
        let at = self
            .index_of(slot)
            .map_or(self.entries.len(), |index| index + 1);
        self.insert_custom(at, name, layer_fn(name, layer));
        self
    }

    // ── the same four, for a layer that is already erased ─────────────────

    /// Insert an already-erased layer immediately outside `slot`.
    ///
    /// The [`CustomLayer`] half of [`insert_before`](Self::insert_before). See
    /// [`append_custom`](Self::append_custom) for why the two are siblings
    /// rather than one widened method, and why this one takes no name.
    pub fn insert_before_custom(&mut self, slot: Slot, layer: impl CustomLayer) -> &mut Self {
        let at = self.index_of(slot).unwrap_or(self.entries.len());
        let erased: Arc<dyn CustomLayer> = Arc::new(layer);
        self.insert_custom(at, erased.name(), erased);
        self
    }

    /// Insert an already-erased layer immediately inside `slot`.
    ///
    /// The [`CustomLayer`] half of [`insert_after`](Self::insert_after).
    pub fn insert_after_custom(&mut self, slot: Slot, layer: impl CustomLayer) -> &mut Self {
        let at = self
            .index_of(slot)
            .map_or(self.entries.len(), |index| index + 1);
        let erased: Arc<dyn CustomLayer> = Arc::new(layer);
        self.insert_custom(at, erased.name(), erased);
        self
    }

    /// Replace a built-in slot with a `tower::Layer` of your own, keeping its
    /// position.
    ///
    /// How an application swaps out a built-in it disagrees with while keeping
    /// the ordering invariants intact. A **battery** layer usually implements
    /// [`CustomLayer`] rather than `tower::Layer<Route>` and goes in with
    /// [`replace_custom`](Self::replace_custom) instead.
    pub fn replace<L>(&mut self, slot: Slot, layer: L) -> &mut Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<Request, Error = core::convert::Infallible>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<Request>>::Response: crate::IntoResponse + 'static,
        <L::Service as tower::Service<Request>>::Future: Send + 'static,
    {
        self.replace_erased(slot, layer_fn(slot.as_str(), layer));
        self
    }

    /// Append a custom layer innermost, next to the handler.
    pub fn append<L>(&mut self, name: &'static str, layer: L) -> &mut Self
    where
        L: tower::Layer<Route> + Clone + Send + Sync + 'static,
        L::Service: tower::Service<Request, Error = core::convert::Infallible>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as tower::Service<Request>>::Response: crate::IntoResponse + 'static,
        <L::Service as tower::Service<Request>>::Future: Send + 'static,
    {
        let at = self.entries.len();
        self.insert_custom(at, name, layer_fn(name, layer));
        self
    }

    /// Replace a slot with an already-erased layer, keeping its position.
    ///
    /// This is how a battery fills [`Slot::RateLimit`] or [`Slot::Session`]:
    /// both are positions with no built-in, and a battery layer — `moso-auth`'s
    /// session layer, `moso-orm`'s request-transaction layer — implements
    /// [`CustomLayer`] directly, because [`CustomLayer::apply`] is
    /// `Route -> Route`, which is exactly what the stack folds. The entry keeps
    /// the slot's name, so `moso middleware` still prints `session` rather than
    /// the layer's type.
    ///
    /// ```
    /// use moso::middleware::{CustomLayer, MiddlewareStack, Slot};
    /// use moso::Route;
    ///
    /// /// A battery layer: it is already `Route -> Route`, so it never was a
    /// /// `tower::Layer`.
    /// struct SessionLayer;
    ///
    /// impl CustomLayer for SessionLayer {
    ///     fn name(&self) -> &'static str { "session" }
    ///     fn apply(&self, service: Route) -> Route { service }
    ///     fn summary(&self) -> String { "cookie=sid".to_owned() }
    /// }
    ///
    /// let mut stack = MiddlewareStack::standard();
    /// stack.replace_custom(Slot::Session, SessionLayer);
    ///
    /// assert!(stack.is_enabled(Slot::Session));
    /// assert_eq!(stack.entry(Slot::Session).expect("filled").summary, "cookie=sid");
    /// // …and the slot is no longer empty, so the boot error is gone.
    /// assert!(stack.validate().is_empty());
    /// ```
    pub fn replace_custom(&mut self, slot: Slot, layer: impl CustomLayer) -> &mut Self {
        self.replace_erased(slot, Arc::new(layer));
        self
    }

    /// Append an already-erased layer innermost, next to the handler.
    ///
    /// # Why these are siblings and not one widened method
    ///
    /// Widening [`append`](Self::append) to take "either a `tower::Layer<Route>`
    /// or a [`CustomLayer`]" needs a trait with a blanket impl for each, and
    /// those two impls overlap — nothing stops a type from implementing both, so
    /// the compiler rejects the pair (E0119). Siblings are what is left, and
    /// they are also the kinder half of the trade: every existing call site
    /// keeps inferring exactly what it inferred before.
    ///
    /// They take no `name`, because a [`CustomLayer`] already has one.
    /// [`CustomLayer::name`] is what `moso middleware` prints, and asking for it
    /// twice is a second place for it to be wrong.
    pub fn append_custom(&mut self, layer: impl CustomLayer) -> &mut Self {
        let at = self.entries.len();
        let erased: Arc<dyn CustomLayer> = Arc::new(layer);
        self.insert_custom(at, erased.name(), erased);
        self
    }

    // ── typed setters, one per configurable slot ──────────────────────────

    /// Configure CORS. Enabling the slot is implied; it is off until you do.
    pub fn cors(&mut self, config: CorsConfig) -> &mut Self {
        self.config.cors = config;
        self.enable(Slot::Cors);
        self.refresh();
        self
    }

    /// Adjust the request-head bounds: `uri_max`, `header_max_count` and
    /// `header_max_bytes`.
    ///
    /// Wins over the `[http]` section; see [`MiddlewareStack::configure`].
    /// Unlike [`body_limit`](Self::body_limit) there is no second number to
    /// disagree with — the extractors and this slot read the same [`Limits`].
    ///
    /// ```
    /// use moso::middleware::MiddlewareStack;
    ///
    /// let mut stack = MiddlewareStack::default();
    /// stack.request_limits(|limits| limits.uri_max = 2048);
    /// ```
    pub fn request_limits(&mut self, f: impl FnOnce(&mut Limits)) -> &mut Self {
        f(&mut self.config.request_limits);
        self.config.explicit.request_limits = true;
        self.refresh();
        self
    }

    /// Set the request timeout.
    ///
    /// Wins over `http.timeout`; see [`MiddlewareStack::configure`].
    pub fn timeout(&mut self, timeout: std::time::Duration) -> &mut Self {
        self.config.timeout.timeout = timeout;
        self.config.explicit.timeout = true;
        self.refresh();
        self
    }

    /// Exempt a route *pattern* from the request timeout.
    ///
    /// `/events/{id}`, never `/events/42`: an exemption matched against a raw
    /// path is an exemption a client can widen.
    pub fn timeout_exempt(&mut self, pattern: impl Into<String>) -> &mut Self {
        self.config.timeout.exempt.push(pattern.into());
        self.refresh();
        self
    }

    /// Set the maximum request body size.
    ///
    /// Wins over `http.body_max`; see [`MiddlewareStack::configure`]. Note that
    /// the *extractors* read `http.body_max` from the request context, so
    /// raising the stack's limit above the configured one does not raise
    /// theirs — the smaller of the two is what a handler observes.
    pub fn body_limit(&mut self, bytes: usize) -> &mut Self {
        self.config.body_limit.max_bytes = bytes;
        self.config.explicit.body_limit = true;
        self.refresh();
        self
    }

    /// Set the trailing-slash policy.
    pub fn normalize_path(&mut self, policy: TrailingSlash) -> &mut Self {
        self.config.normalize_path.trailing_slash = policy;
        self.refresh();
        self
    }

    /// Adjust the security headers.
    ///
    /// Wins over the profile's defaults; see [`MiddlewareStack::configure`].
    pub fn security_headers(&mut self, f: impl FnOnce(&mut SecurityHeadersConfig)) -> &mut Self {
        f(&mut self.config.security_headers);
        self.config.explicit.security_headers = true;
        self.refresh();
        self
    }

    /// Adjust the request-id policy.
    pub fn request_id(&mut self, f: impl FnOnce(&mut RequestIdConfig)) -> &mut Self {
        f(&mut self.config.request_id);
        self.refresh();
        self
    }

    /// Adjust compression.
    pub fn compression(&mut self, f: impl FnOnce(&mut CompressionConfig)) -> &mut Self {
        f(&mut self.config.compression);
        self.refresh();
        self
    }

    /// Adjust panic handling.
    ///
    /// Wins over the profile's defaults; see [`MiddlewareStack::configure`].
    pub fn catch_panic(&mut self, f: impl FnOnce(&mut CatchPanicConfig)) -> &mut Self {
        f(&mut self.config.catch_panic);
        self.config.explicit.catch_panic = true;
        self.refresh();
        self
    }

    /// Adjust error rendering and logging.
    ///
    /// Wins over `http.expose_internal_errors` and the profile; see
    /// [`MiddlewareStack::configure`].
    pub fn catch_error(&mut self, f: impl FnOnce(&mut CatchErrorConfig)) -> &mut Self {
        f(&mut self.config.catch_error);
        self.config.explicit.catch_error = true;
        self.refresh();
        self
    }

    /// Adjust the request span.
    pub fn trace(&mut self, f: impl FnOnce(&mut TraceConfig)) -> &mut Self {
        f(&mut self.config.trace);
        self.refresh();
        self
    }

    /// Adjust which headers are marked sensitive.
    pub fn sensitive_headers(&mut self, f: impl FnOnce(&mut SensitiveHeadersConfig)) -> &mut Self {
        f(&mut self.config.sensitive_headers);
        self.refresh();
        self
    }

    /// Record request metrics into `recorder`. Enabling the slot is implied.
    pub fn metrics(&mut self, recorder: Arc<dyn MetricsRecorder>) -> &mut Self {
        self.config.metrics.recorder = Some(recorder);
        self.enable(Slot::Metrics);
        self.refresh();
        self
    }

    // ── inspection and composition ────────────────────────────────────────

    /// The composed stack, outermost first. What `moso middleware` prints.
    pub fn describe(&self) -> Vec<StackEntry> {
        self.entries.clone()
    }

    /// The entry occupying `slot`, if it is in the stack at all.
    pub fn entry(&self, slot: Slot) -> Option<&StackEntry> {
        self.index_of(slot).map(|index| &self.entries[index])
    }

    /// Whether `slot` is present and enabled.
    pub fn is_enabled(&self, slot: Slot) -> bool {
        self.entry(slot).is_some_and(|entry| entry.enabled)
    }

    /// The stack as `moso middleware` prints it.
    ///
    /// ```text
    /// GLOBAL
    ///   1 catch_panic          render_details=false
    ///   2 request_id           header=x-request-id generator=ulid
    /// ```
    pub fn render(&self) -> String {
        let mut out = String::from("GLOBAL\n");
        for (index, entry) in self
            .entries
            .iter()
            .filter(|entry| entry.enabled)
            .enumerate()
        {
            let width = 20_usize.saturating_sub(entry.name.chars().count());
            out.push_str(&format!(
                "{:>3} {}{}{}\n",
                index + 1,
                entry.name,
                " ".repeat(width.max(1)),
                entry.summary
            ));
        }
        out
    }

    /// Check the ordering invariants, as `App::build()` does.
    ///
    /// Currently: `CatchError` must be inside `Trace` and outside `Timeout`,
    /// and `Metrics` must be innermost. A violation is a boot error naming the
    /// rule, because the consequences — logs without spans, timeouts that are
    /// not problem documents, exploded metric cardinality — are all subtle
    /// enough to survive review.
    ///
    /// It also reports the two ways a slot can be *silently* inert: enabled
    /// without the cargo feature that carries its implementation, and enabled
    /// with no implementation at all ([`Slot::RateLimit`], [`Slot::Session`]).
    pub fn validate(&self) -> Vec<BootError> {
        let mut errors = Vec::new();
        let stack = || -> Vec<String> {
            self.entries
                .iter()
                .filter(|entry| entry.enabled)
                .map(|entry| entry.name.clone().into_owned())
                .collect()
        };
        let mut order_error = |rule: &'static str| {
            errors.push(BootError::MiddlewareOrder {
                rule,
                stack: stack(),
            });
        };

        let position = |slot: Slot| -> Option<usize> {
            self.entries
                .iter()
                .filter(|entry| entry.enabled)
                .position(|entry| entry.slot == Some(slot))
        };

        if let (Some(trace), Some(catch_error)) =
            (position(Slot::Trace), position(Slot::CatchError))
            && trace > catch_error
        {
            order_error("`catch_error` must run inside `trace`, so the error log carries the span");
        }
        if let (Some(catch_error), Some(timeout)) =
            (position(Slot::CatchError), position(Slot::Timeout))
            && catch_error > timeout
        {
            order_error(
                "`catch_error` must run outside `timeout`, so an expiry renders as a problem \
                 document",
            );
        }
        if let Some(metrics) = position(Slot::Metrics) {
            let last = self
                .entries
                .iter()
                .filter(|entry| entry.enabled)
                .count()
                .saturating_sub(1);
            if metrics != last {
                order_error(
                    "`metrics` must be innermost, so the `route` label is the matched pattern",
                );
            }
        }

        for entry in &self.entries {
            if !entry.enabled || entry.layer.is_some() {
                continue;
            }
            let Some(slot) = entry.slot else { continue };
            if !slot.has_builtin() {
                errors.push(BootError::Other {
                    message: format!("middleware slot `{slot}` is enabled but empty"),
                    notes: vec![
                        format!(
                            "`{slot}` has no built-in implementation; it is a position a battery \
                             fills"
                        ),
                        "`replace_custom` takes a `CustomLayer`, which is what a battery layer \
                         implements; `replace` takes a `tower::Layer<Route>`"
                            .to_owned(),
                    ],
                    fix: Some(format!(
                        "s.replace_custom(Slot::{slot:?}, YourLayer::new())   // or \
                         s.disable(Slot::{slot:?})"
                    )),
                });
            }
        }

        if self.is_enabled(Slot::Cors) && !cfg!(feature = "cors") {
            errors.push(BootError::Other {
                message: "the `cors` middleware slot is enabled but the `cors` feature is off"
                    .to_owned(),
                notes: vec![
                    "`tower_http::cors` is only compiled in when the feature is on, so the slot \
                     would silently do nothing"
                        .to_owned(),
                ],
                fix: Some("moso-core = { version = \"0.1\", features = [\"cors\"] }".to_owned()),
            });
        }
        if self.is_enabled(Slot::Compression) && !cfg!(feature = "compression") {
            errors.push(BootError::Other {
                message:
                    "the `compression` middleware slot is enabled but the `compression` feature \
                     is off"
                        .to_owned(),
                notes: vec![
                    "the codec crates are only compiled in when the feature is on, so the slot \
                     would silently do nothing"
                        .to_owned(),
                ],
                fix: Some(
                    "moso-core = { version = \"0.1\", features = [\"compression\"] }".to_owned(),
                ),
            });
        }

        if self.is_enabled(Slot::Cors) {
            errors.extend(self.config.cors.validate());
        }

        errors
    }

    /// Fold the enabled entries around `service`, resolving the route pattern
    /// first.
    ///
    /// What [`App::build`](crate::AppBuilder::build) calls.
    /// [`compose`](Self::compose) alone leaves `trace`, `timeout` and `metrics`
    /// with no pattern to read, because the composed stack sits outside Axum's
    /// routing; this wraps the result in [`route_pattern::layer`], which
    /// resolves the request path against `patterns` and publishes the answer
    /// before the outermost slot runs.
    ///
    /// The stack is also the only thing that knows whether
    /// [`Slot::NormalizePath`] will rewrite the URI on the way in, so it hands
    /// that policy to the resolver: `/users/` must resolve to `/users` exactly
    /// when the normaliser would have made it `/users`, and to nothing when it
    /// would have answered a 308 instead.
    ///
    /// ```
    /// use moso::middleware::{MiddlewareStack, RoutePatterns};
    /// use moso::Route;
    /// use moso::deps::tower::service_fn;
    ///
    /// let service = Route::new(service_fn(|_req: moso::Request| async {
    ///     Ok::<_, std::convert::Infallible>(moso::Response::new(
    ///         moso::deps::axum::body::Body::empty(),
    ///     ))
    /// }));
    ///
    /// let stack = MiddlewareStack::standard();
    /// let composed = stack.compose_routed(RoutePatterns::new(["/users/{id}"]), service);
    /// # let _ = composed;
    /// ```
    #[must_use]
    pub fn compose_routed(&self, patterns: RoutePatterns, service: Route) -> Route {
        let patterns = match self.live_normalizer() {
            Some(config) => patterns.normalized_by(config),
            None => patterns,
        };
        route_pattern::layer(Arc::new(patterns), self.compose(service))
    }

    /// The trailing-slash policy that will actually run, if any.
    ///
    /// `None` when the slot is disabled, absent, or filled with a layer of the
    /// application's own — in all three cases the built-in normaliser does not
    /// run, so predicting its rewrite would be predicting something that never
    /// happens.
    fn live_normalizer(&self) -> Option<NormalizePathConfig> {
        self.entry(Slot::NormalizePath)
            .filter(|entry| entry.enabled && entry.layer.is_none())
            .map(|_| self.config.normalize_path)
    }

    /// Fold the enabled entries around `service`.
    ///
    /// Called once at boot. Entries apply innermost first, so the resulting
    /// call order matches [`MiddlewareStack::describe`] read top to bottom.
    ///
    /// This resolves no route pattern, so `trace`, `timeout` and `metrics` will
    /// see [`UNMATCHED_ROUTE`] unless something outside already published one.
    /// [`compose_routed`](Self::compose_routed) is the one an application wants.
    pub fn compose(&self, service: Route) -> Route {
        let mut service = service;
        for entry in self.entries.iter().rev() {
            if !entry.enabled {
                continue;
            }
            service = match (&entry.layer, entry.slot) {
                (Some(layer), _) => layer.apply(service),
                (None, Some(slot)) => self.apply_builtin(slot, service),
                (None, None) => service,
            };
        }
        service
    }

    // ── internals ─────────────────────────────────────────────────────────

    /// Wrap `service` in the built-in implementation of `slot`.
    fn apply_builtin(&self, slot: Slot, service: Route) -> Route {
        let config = &self.config;
        match slot {
            Slot::CatchPanic => catch_panic::layer(&config.catch_panic, service),
            Slot::RequestId => request_id::layer(&config.request_id, service),
            Slot::Trace => trace::layer(&config.trace, service),
            Slot::SensitiveHeaders => sensitive_headers::layer(&config.sensitive_headers, service),
            Slot::CatchError => {
                catch_error::layer(&config.catch_error, Arc::clone(&config.silent), service)
            }
            Slot::RequestLimits => request_limits::layer(&config.request_limits, service),
            Slot::Timeout => timeout::layer(&config.timeout, service),
            Slot::BodyLimit => body_limit::layer(&config.body_limit, service),
            Slot::NormalizePath => normalize_path::layer(&config.normalize_path, service),
            Slot::Cors => cors::layer(&config.cors, service),
            Slot::SecurityHeaders => security_headers::layer(&config.security_headers, service),
            Slot::Compression => compression::layer(&config.compression, service),
            // Reserved for a battery. `validate` refuses to boot with one of
            // these enabled and empty, so reaching here means the operator was
            // told and chose `build_unchecked`.
            Slot::RateLimit | Slot::Session => service,
            Slot::Metrics => metrics::layer(&config.metrics, Arc::clone(&config.silent), service),
        }
    }

    /// The index of `slot`, if it is present.
    fn index_of(&self, slot: Slot) -> Option<usize> {
        self.entries.iter().position(|e| e.slot == Some(slot))
    }

    /// A mutable handle on `slot`'s entry.
    fn entry_mut(&mut self, slot: Slot) -> Option<&mut StackEntry> {
        self.index_of(slot).map(|index| &mut self.entries[index])
    }

    /// Where `slot` belongs, given whatever slots are currently present.
    fn canonical_index(&self, slot: Slot) -> usize {
        let target = slot.position();
        self.entries
            .iter()
            .position(|entry| entry.slot.is_some_and(|other| other.position() > target))
            .unwrap_or(self.entries.len())
    }

    /// Put `layer` in `slot`, keeping the slot's position and printed name.
    ///
    /// The one body behind [`replace`](Self::replace) and
    /// [`replace_custom`](Self::replace_custom): they differ only in how the
    /// layer was erased, and the position, the name and the summary must not be
    /// decided twice.
    fn replace_erased(&mut self, slot: Slot, layer: Arc<dyn CustomLayer>) {
        let summary = layer.summary();
        match self.index_of(slot) {
            Some(index) => {
                let entry = &mut self.entries[index];
                entry.layer = Some(layer);
                entry.enabled = true;
                entry.summary = summary;
            }
            None => {
                let at = self.canonical_index(slot);
                self.entries.insert(
                    at,
                    StackEntry {
                        slot: Some(slot),
                        name: Cow::Borrowed(slot.as_str()),
                        enabled: true,
                        summary,
                        layer: Some(layer),
                    },
                );
            }
        }
    }

    /// Insert a custom entry at `at`.
    fn insert_custom(&mut self, at: usize, name: &'static str, layer: Arc<dyn CustomLayer>) {
        let summary = layer.summary();
        self.entries.insert(
            at,
            StackEntry {
                slot: None,
                name: Cow::Borrowed(name),
                enabled: true,
                summary,
                layer: Some(layer),
            },
        );
    }

    /// Re-render every built-in entry's summary from the configuration.
    ///
    /// Called after every setter. Recomputing all of them is cheaper to get
    /// right than remembering which setter touches which entry, and the stack
    /// is fourteen items long.
    fn refresh(&mut self) {
        for index in 0..self.entries.len() {
            let Some(slot) = self.entries[index].slot else {
                continue;
            };
            if self.entries[index].layer.is_some() {
                continue;
            }
            self.entries[index].summary = summary_for(slot, &self.config);
        }
    }
}

/// The entry a slot starts life as: enabled per the documented defaults, with
/// its summary already rendered.
fn entry_for(slot: Slot, config: &StackConfig) -> StackEntry {
    StackEntry {
        slot: Some(slot),
        name: Cow::Borrowed(slot.as_str()),
        enabled: default_enabled(slot, config),
        summary: summary_for(slot, config),
        layer: None,
    }
}

/// Whether `slot` is on in a freshly built [`MiddlewareStack::standard`].
///
/// Two of the defaults are decided by cargo features rather than by taste:
/// `trace` needs the `tracing` feature and `compression` needs the codec
/// crates. A slot whose feature is off starts disabled rather than enabled and
/// inert, so `moso middleware` never claims to be doing something it is not.
fn default_enabled(slot: Slot, config: &StackConfig) -> bool {
    match slot {
        Slot::CatchPanic
        | Slot::RequestId
        | Slot::SensitiveHeaders
        | Slot::CatchError
        | Slot::RequestLimits
        | Slot::Timeout
        | Slot::BodyLimit
        | Slot::NormalizePath
        | Slot::SecurityHeaders => true,
        Slot::Trace => cfg!(feature = "tracing"),
        Slot::Compression => cfg!(feature = "compression"),
        Slot::Cors => cfg!(feature = "cors") && config.cors.is_configured(),
        Slot::RateLimit | Slot::Session => false,
        Slot::Metrics => config.metrics.recorder.is_some(),
    }
}

/// The one-line summary `moso middleware` prints beside a built-in slot.
fn summary_for(slot: Slot, config: &StackConfig) -> String {
    match slot {
        Slot::CatchPanic => config.catch_panic.summary(),
        Slot::RequestId => config.request_id.summary(),
        Slot::Trace => config.trace.summary(),
        Slot::SensitiveHeaders => config.sensitive_headers.summary(),
        Slot::CatchError => config.catch_error.summary(),
        Slot::RequestLimits => request_limits::summary(&config.request_limits),
        Slot::Timeout => config.timeout.summary(),
        Slot::BodyLimit => config.body_limit.summary(),
        Slot::NormalizePath => config.normalize_path.summary(),
        Slot::Cors => config.cors.summary(),
        Slot::SecurityHeaders => config.security_headers.summary(),
        Slot::Compression => config.compression.summary(),
        Slot::RateLimit | Slot::Session => "(no built-in; fill with `replace_custom`)".to_owned(),
        Slot::Metrics => config.metrics.summary(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::StatusCode;

    /// A layer that stamps a header, for the reordering tests.
    #[derive(Clone)]
    struct Stamp(&'static str);

    impl tower::Layer<Route> for Stamp {
        type Service = StampService;

        fn layer(&self, inner: Route) -> Self::Service {
            StampService {
                inner,
                name: self.0,
            }
        }
    }

    #[derive(Clone)]
    struct StampService {
        inner: Route,
        name: &'static str,
    }

    impl tower::Service<Request> for StampService {
        type Response = Response;
        type Error = Infallible;
        type Future = BoxFuture<'static, core::result::Result<Response, Infallible>>;

        fn poll_ready(
            &mut self,
            cx: &mut core::task::Context<'_>,
        ) -> core::task::Poll<core::result::Result<(), Infallible>> {
            tower::Service::poll_ready(&mut self.inner, cx)
        }

        fn call(&mut self, req: Request) -> Self::Future {
            let ready = self.inner.clone();
            let mut inner = core::mem::replace(&mut self.inner, ready);
            let name = self.name;
            Box::pin(async move {
                let mut response = tower::Service::call(&mut inner, req).await?;
                response.headers_mut().append(
                    http::HeaderName::from_static("x-stamp"),
                    http::HeaderValue::from_static(name),
                );
                Ok(response)
            })
        }
    }

    fn ok_route() -> Route {
        Route::new(tower::service_fn(|_req: Request| async move {
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }))
    }

    fn names(stack: &MiddlewareStack) -> Vec<String> {
        stack
            .describe()
            .into_iter()
            .map(|entry| entry.name.into_owned())
            .collect()
    }

    #[test]
    fn slot_order_is_the_documented_order() {
        assert_eq!(Slot::ORDER[0], Slot::CatchPanic);
        assert_eq!(Slot::ORDER[4], Slot::CatchError);
        assert_eq!(Slot::ORDER[Slot::ORDER.len() - 1], Slot::Metrics);
        assert_eq!(Slot::ORDER.len(), 15);
    }

    #[test]
    fn catch_error_sits_inside_trace_and_outside_timeout() {
        assert!(Slot::Trace.position() < Slot::CatchError.position());
        assert!(Slot::CatchError.position() < Slot::Timeout.position());
    }

    #[test]
    fn the_head_limits_are_checked_inside_catch_error_and_before_any_budget() {
        // Inside `catch_error` so a 414 or a 431 is logged like every other
        // error; outside `timeout` and `body_limit` because refusing a head
        // costs a microsecond and should not first buy a thirty-second budget
        // or a body reader.
        assert!(Slot::CatchError.position() < Slot::RequestLimits.position());
        assert!(Slot::RequestLimits.position() < Slot::Timeout.position());
        assert!(Slot::RequestLimits.position() < Slot::BodyLimit.position());
    }

    #[test]
    fn slot_names_are_snake_case() {
        assert_eq!(Slot::SecurityHeaders.as_str(), "security_headers");
        assert_eq!(Slot::CatchPanic.to_string(), "catch_panic");
    }

    #[test]
    fn a_bare_stack_has_nothing_in_it() {
        assert!(MiddlewareStack::bare().describe().is_empty());
    }

    /// Acceptance criterion 1: the exact ordering, asserted over `describe()`.
    #[test]
    fn the_standard_stack_is_the_documented_table() {
        assert_eq!(
            names(&MiddlewareStack::standard()),
            [
                "catch_panic",
                "request_id",
                "trace",
                "sensitive_headers",
                "catch_error",
                "request_limits",
                "timeout",
                "body_limit",
                "normalize_path",
                "cors",
                "security_headers",
                "compression",
                "rate_limit",
                "session",
                "metrics",
            ]
        );
    }

    #[test]
    fn the_defaults_that_are_off_are_off() {
        let stack = MiddlewareStack::standard();
        assert!(stack.is_enabled(Slot::CatchPanic));
        assert!(stack.is_enabled(Slot::CatchError));
        assert!(stack.is_enabled(Slot::SecurityHeaders));
        assert!(!stack.is_enabled(Slot::Cors));
        assert!(!stack.is_enabled(Slot::RateLimit));
        assert!(!stack.is_enabled(Slot::Session));
        assert!(!stack.is_enabled(Slot::Metrics));
        assert_eq!(stack.is_enabled(Slot::Trace), cfg!(feature = "tracing"));
        assert_eq!(
            stack.is_enabled(Slot::Compression),
            cfg!(feature = "compression")
        );
    }

    /// Acceptance criterion 2, part one.
    #[test]
    fn disable_turns_a_slot_off_without_removing_it() {
        let mut stack = MiddlewareStack::standard();
        stack.disable(Slot::Compression);
        assert!(!stack.is_enabled(Slot::Compression));
        // Still listed, so `moso middleware` can say it is off rather than
        // leaving the reader to wonder whether it exists.
        assert!(names(&stack).contains(&"compression".to_owned()));
    }

    /// Acceptance criterion 2, part two.
    #[test]
    fn insert_after_lands_immediately_inside_the_slot() {
        let mut stack = MiddlewareStack::standard();
        stack.insert_after(Slot::Trace, "tenant", Stamp("tenant"));
        let names = names(&stack);
        let trace = names.iter().position(|n| n == "trace").expect("trace");
        assert_eq!(names[trace + 1], "tenant");
    }

    #[test]
    fn insert_before_lands_immediately_outside_the_slot() {
        let mut stack = MiddlewareStack::standard();
        stack.insert_before(Slot::Cors, "tenant", Stamp("tenant"));
        let names = names(&stack);
        let cors = names.iter().position(|n| n == "cors").expect("cors");
        assert_eq!(names[cors - 1], "tenant");
    }

    /// Acceptance criterion 2, part three.
    #[test]
    fn replace_keeps_the_position_and_the_name() {
        let mut stack = MiddlewareStack::standard();
        stack.replace(Slot::RateLimit, Stamp("rate"));
        let names = names(&stack);
        assert_eq!(names[Slot::RateLimit.position()], "rate_limit");
        assert!(stack.is_enabled(Slot::RateLimit));
        assert!(stack.entry(Slot::RateLimit).expect("entry").layer.is_some());
        // …and the "empty slot" boot error is gone, because it is no longer
        // empty.
        assert!(stack.validate().is_empty());
    }

    #[test]
    fn append_lands_innermost() {
        let mut stack = MiddlewareStack::standard();
        stack.append("last", Stamp("last"));
        assert_eq!(names(&stack).last().map(String::as_str), Some("last"));
    }

    #[test]
    fn enable_reinserts_a_slot_a_bare_stack_never_had() {
        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::CatchError);
        stack.enable(Slot::CatchPanic);
        stack.enable(Slot::Timeout);
        assert_eq!(names(&stack), ["catch_panic", "catch_error", "timeout"]);
    }

    #[test]
    fn a_setter_updates_the_summary_describe_reports() {
        let mut stack = MiddlewareStack::standard();
        stack.timeout(std::time::Duration::from_secs(5));
        assert_eq!(stack.entry(Slot::Timeout).expect("timeout").summary, "5s");
    }

    #[test]
    fn configuring_cors_enables_the_slot() {
        let mut stack = MiddlewareStack::standard();
        stack.cors(CorsConfig::allow_origins(["https://a.example"]));
        assert!(stack.is_enabled(Slot::Cors));
    }

    // ── validation ────────────────────────────────────────────────────────

    #[test]
    fn the_standard_stack_validates() {
        assert!(MiddlewareStack::standard().validate().is_empty());
    }

    #[test]
    fn catch_error_outside_trace_is_a_boot_error() {
        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::CatchError);
        stack.enable(Slot::Trace);
        // Force the wrong order by hand; no public API produces it.
        stack.entries.swap(0, 1);
        let errors = stack.validate();
        assert!(matches!(
            errors.as_slice(),
            [BootError::MiddlewareOrder { rule, .. }] if rule.contains("inside `trace`")
        ));
    }

    #[test]
    fn metrics_must_be_innermost() {
        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::Metrics);
        stack.append("after_metrics", Stamp("x"));
        let errors = stack.validate();
        assert!(errors.iter().any(
            |error| matches!(error, BootError::MiddlewareOrder { rule, .. }
                    if rule.contains("innermost"))
        ));
    }

    #[test]
    fn an_empty_reserved_slot_is_a_boot_error() {
        let mut stack = MiddlewareStack::standard();
        stack.enable(Slot::Session);
        let errors = stack.validate();
        assert!(errors.iter().any(|error| {
            error
                .headline()
                .contains("middleware slot `session` is enabled but empty")
        }));
    }

    // ── composition ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn compose_applies_entries_outermost_first() {
        use tower::ServiceExt as _;

        let mut stack = MiddlewareStack::bare();
        stack.append("outer", Stamp("outer"));
        stack.append("inner", Stamp("inner"));

        let service = stack.compose(ok_route());
        let response = service
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        // Both ran, and the innermost stamped first — which is what "entries
        // apply innermost first" means on the way out.
        let stamps: Vec<_> = response
            .headers()
            .get_all("x-stamp")
            .iter()
            .map(|value| value.to_str().expect("ascii").to_owned())
            .collect();
        assert_eq!(stamps, ["inner", "outer"]);
    }

    #[tokio::test]
    async fn a_disabled_entry_is_not_applied() {
        use tower::ServiceExt as _;

        let mut stack = MiddlewareStack::bare();
        stack.append("outer", Stamp("outer"));
        stack.entries[0].enabled = false;

        let response = stack
            .compose(ok_route())
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        assert!(response.headers().get("x-stamp").is_none());
    }

    #[tokio::test]
    async fn the_standard_stack_serves_a_request_end_to_end() {
        use tower::ServiceExt as _;

        let stack = MiddlewareStack::standard();
        let response = stack
            .compose(ok_route())
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");

        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(crate::REQUEST_ID_HEADER));
        assert_eq!(
            response
                .headers()
                .get("x-content-type-options")
                .and_then(|value| value.to_str().ok()),
            Some("nosniff")
        );
    }

    #[test]
    fn render_lists_only_the_enabled_entries() {
        let mut stack = MiddlewareStack::standard();
        stack.disable(Slot::Compression);
        let rendered = stack.render();
        assert!(rendered.starts_with("GLOBAL\n"));
        assert!(rendered.contains("catch_panic"));
        assert!(!rendered.contains("compression"));
    }

    #[test]
    fn configure_adopts_the_profile_and_the_http_section() {
        let mut stack = MiddlewareStack::standard();
        let http = HttpConfig {
            timeout: std::time::Duration::from_secs(7),
            body_max: 4096,
            ..HttpConfig::default()
        };
        stack.configure(Profile::Dev, &http);

        assert_eq!(stack.entry(Slot::Timeout).expect("timeout").summary, "7s");
        assert_eq!(
            stack.entry(Slot::BodyLimit).expect("body_limit").summary,
            "4 KiB"
        );
        // `dev` renders panic details and omits HSTS.
        assert!(
            stack
                .entry(Slot::CatchPanic)
                .expect("catch_panic")
                .summary
                .contains("render_details=true")
        );
        assert!(
            !stack
                .entry(Slot::SecurityHeaders)
                .expect("security_headers")
                .summary
                .contains("hsts=")
        );
    }

    #[test]
    fn configure_does_not_undo_an_explicit_edit() {
        // The application edits the stack on the builder; `configure` runs
        // later, inside `build`. The later write must not win, or every
        // `with_middleware` call would be silently reverted by the defaults.
        let mut stack = MiddlewareStack::standard();
        stack.timeout(std::time::Duration::from_millis(250));
        stack.body_limit(64);
        stack.catch_panic(|config| config.render_details = true);
        stack.catch_error(|config| config.log_headers = true);
        stack.security_headers(|headers| {
            headers.frame_options("SAMEORIGIN");
        });

        stack.configure(
            Profile::Production,
            &HttpConfig {
                timeout: std::time::Duration::from_secs(90),
                body_max: 8 * 1024 * 1024,
                ..HttpConfig::default()
            },
        );

        assert_eq!(
            stack.config.timeout.timeout,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(stack.config.body_limit.max_bytes, 64);
        assert!(stack.config.catch_panic.render_details);
        assert!(stack.config.catch_error.log_headers);
        assert!(
            stack
                .config
                .security_headers
                .headers()
                .iter()
                .any(|(name, value)| name == "x-frame-options" && value == "SAMEORIGIN")
        );
    }

    #[test]
    fn configure_fills_in_everything_that_was_not_set() {
        let mut stack = MiddlewareStack::standard();
        stack.timeout(std::time::Duration::from_millis(250));

        stack.configure(
            Profile::Production,
            &HttpConfig {
                timeout: std::time::Duration::from_secs(90),
                body_max: 4096,
                expose_internal_errors: true,
                ..HttpConfig::default()
            },
        );

        // Only `timeout` was claimed; the rest still come from the section.
        assert_eq!(
            stack.config.timeout.timeout,
            std::time::Duration::from_millis(250)
        );
        assert_eq!(stack.config.body_limit.max_bytes, 4096);
        assert!(stack.config.catch_error.problem.expose_internal_errors);
    }

    #[test]
    fn configure_keeps_the_paths_the_application_silenced() {
        let mut stack = MiddlewareStack::standard();
        stack.silence("/internal/metrics");
        stack.configure(Profile::Test, &HttpConfig::default());

        let silent = stack.config.silent.to_vec();
        assert!(silent.iter().any(|path| path == "/internal/metrics"));
        assert!(silent.iter().any(|path| path == "/healthz"));
        assert!(silent.iter().any(|path| path == "/openapi.json"));
    }

    #[test]
    fn configure_is_idempotent() {
        let http = HttpConfig {
            body_max: 4096,
            ..HttpConfig::default()
        };
        let mut once = MiddlewareStack::standard();
        once.configure(Profile::Test, &http);
        let mut twice = MiddlewareStack::standard();
        twice.configure(Profile::Test, &http);
        twice.configure(Profile::Test, &http);

        assert_eq!(once.config.silent.to_vec(), twice.config.silent.to_vec());
        assert_eq!(
            once.config.body_limit.max_bytes,
            twice.config.body_limit.max_bytes
        );
    }

    #[test]
    fn a_next_built_from_a_route_runs_it() {
        // A compile-time check that both constructors exist and agree on the
        // shape; the behavioural test lives in `from_fn`.
        let next = Next::from_route(ok_route());
        assert_eq!(format!("{next:?}"), "Next");
        let next = Next::new(ok_route());
        assert_eq!(format!("{next:?}"), "Next");
    }

    // ── filling a reserved slot with a `CustomLayer` ──────────────────────

    /// The same stamp as [`Stamp`], reached through [`CustomLayer`] instead of
    /// through `tower::Layer` — which is the shape every battery layer has,
    /// because [`CustomLayer::apply`] is already `Route -> Route`.
    struct StampCustom(&'static str);

    impl CustomLayer for StampCustom {
        fn name(&self) -> &'static str {
            self.0
        }

        fn apply(&self, service: Route) -> Route {
            Route::new(StampService {
                inner: service,
                name: self.0,
            })
        }

        fn summary(&self) -> String {
            format!("stamps {}", self.0)
        }
    }

    /// Every `x-stamp` the response carries, innermost first.
    async fn stamps(stack: &MiddlewareStack) -> Vec<String> {
        use tower::ServiceExt as _;

        let response = stack
            .compose(ok_route())
            .oneshot(Request::new(axum::body::Body::empty()))
            .await
            .expect("infallible");
        response
            .headers()
            .get_all("x-stamp")
            .iter()
            .map(|value| value.to_str().expect("ascii").to_owned())
            .collect()
    }

    #[test]
    fn replace_custom_fills_a_reserved_slot_and_keeps_its_name() {
        let mut stack = MiddlewareStack::standard();
        stack.replace_custom(Slot::RateLimit, StampCustom("rate"));
        stack.replace_custom(Slot::Session, StampCustom("session"));

        for slot in [Slot::RateLimit, Slot::Session] {
            let entry = stack.entry(slot).expect("filled");
            assert!(entry.enabled);
            assert!(entry.layer.is_some());
            // The slot's name, not the layer's: `moso middleware` prints the
            // position, and the position is what an operator is looking for.
            assert_eq!(entry.name, slot.as_str());
        }
        assert_eq!(
            stack.entry(Slot::Session).expect("filled").summary,
            "stamps session"
        );
        // …and the "enabled but empty" boot error is gone, because it is not.
        assert!(stack.validate().is_empty());
    }

    #[tokio::test]
    async fn a_custom_layer_in_a_reserved_slot_runs_where_the_slot_sits() {
        let mut stack = MiddlewareStack::bare();
        stack.replace_custom(Slot::Session, StampCustom("session"));
        stack.replace_custom(Slot::RateLimit, StampCustom("rate"));

        // `rate_limit` is outside `session` in the canonical order, whichever
        // order the two calls were written in…
        assert_eq!(names(&stack), ["rate_limit", "session"]);
        // …so on the way out the inner one stamps first.
        assert_eq!(stamps(&stack).await, ["session", "rate"]);
    }

    #[tokio::test]
    async fn the_custom_installers_land_where_their_tower_siblings_do() {
        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::NormalizePath);
        stack.insert_before_custom(Slot::NormalizePath, StampCustom("before"));
        stack.insert_after_custom(Slot::NormalizePath, StampCustom("after"));
        stack.append_custom(StampCustom("last"));

        // The name comes from `CustomLayer::name`, so there is one place for it.
        assert_eq!(names(&stack), ["before", "normalize_path", "after", "last"]);
        assert_eq!(stamps(&stack).await, ["last", "after", "before"]);
    }

    #[test]
    fn an_empty_reserved_slot_points_at_the_installer_that_takes_a_battery_layer() {
        let mut stack = MiddlewareStack::standard();
        stack.enable(Slot::RateLimit);
        let errors = stack.validate();
        let fixes: Vec<String> = errors.iter().filter_map(BootError::fix).collect();
        assert!(
            fixes
                .iter()
                .any(|fix| fix.contains("replace_custom(Slot::RateLimit")),
            "{fixes:?}"
        );
    }

    // ── the resolved route pattern ────────────────────────────────────────

    /// Records the `route` every request was labelled with.
    #[derive(Clone, Default)]
    struct Routes(Arc<std::sync::Mutex<Vec<String>>>);

    impl MetricsRecorder for Routes {
        fn record(&self, sample: &RequestSample<'_>) {
            self.0.lock().expect("lock").push(sample.route.to_owned());
        }
    }

    impl Routes {
        fn seen(&self) -> Vec<String> {
            self.0.lock().expect("lock").clone()
        }
    }

    /// Send `path` through the composed stack, with the pattern resolver in
    /// front of it exactly as `App::build` installs it.
    async fn through(stack: &MiddlewareStack, patterns: RoutePatterns, path: &str) {
        use tower::ServiceExt as _;

        let request = http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("request");
        stack
            .compose_routed(patterns, ok_route())
            .oneshot(request)
            .await
            .expect("infallible");
    }

    #[tokio::test]
    async fn the_metrics_label_is_the_pattern_and_two_paths_are_one_series() {
        let recorder = Routes::default();
        let mut stack = MiddlewareStack::bare();
        stack.metrics(Arc::new(recorder.clone()));

        for path in ["/users/1", "/users/2", "/users/3"] {
            through(&stack, RoutePatterns::new(["/users/{id}"]), path).await;
        }

        assert_eq!(recorder.seen(), ["/users/{id}"; 3]);
    }

    #[tokio::test]
    async fn a_path_outside_the_route_table_folds_into_one_bounded_series() {
        // The cardinality cap is the backstop; this is the property that keeps
        // it from ever being reached by ordinary 404 traffic.
        let recorder = Routes::default();
        let mut stack = MiddlewareStack::bare();
        stack.metrics(Arc::new(recorder.clone()));

        for path in ["/nope/1", "/nope/2", "/nope/3"] {
            through(&stack, RoutePatterns::new(["/users/{id}"]), path).await;
        }

        assert_eq!(recorder.seen(), [UNMATCHED_ROUTE; 3]);
    }

    #[tokio::test]
    async fn a_route_moso_cannot_see_the_pattern_of_is_unmatched() {
        // `Router::mount_axum` contributes no patterns, so a request it serves
        // has none — the same answer that keeps it out of the document.
        let recorder = Routes::default();
        let mut stack = MiddlewareStack::bare();
        stack.metrics(Arc::new(recorder.clone()));

        through(
            &stack,
            RoutePatterns::new(["/users/{id}"]),
            "/mounted/thing",
        )
        .await;

        assert_eq!(recorder.seen(), [UNMATCHED_ROUTE]);
    }

    #[tokio::test]
    async fn the_resolver_follows_the_stack_s_own_trailing_slash_policy() {
        let recorder = Routes::default();
        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::NormalizePath);
        stack.metrics(Arc::new(recorder.clone()));

        through(&stack, RoutePatterns::new(["/users"]), "/users/").await;
        assert_eq!(recorder.seen(), ["/users"]);
    }

    #[tokio::test]
    async fn with_normalisation_off_the_two_spellings_stay_two_paths() {
        let recorder = Routes::default();
        let mut stack = MiddlewareStack::bare();
        stack.metrics(Arc::new(recorder.clone()));

        // The slot is absent from a bare stack, so nothing will rewrite the URI
        // and the router would not have matched either.
        through(&stack, RoutePatterns::new(["/users"]), "/users/").await;
        assert_eq!(recorder.seen(), [UNMATCHED_ROUTE]);
    }

    #[tokio::test(start_paused = true)]
    async fn timeout_exempt_now_matches_the_pattern_it_names() {
        use tower::ServiceExt as _;

        let slow = Route::new(tower::service_fn(|_req: Request| async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }));

        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::Timeout);
        stack.timeout(std::time::Duration::from_millis(10));
        stack.timeout_exempt("/events/{id}");

        let patterns = RoutePatterns::new(["/events/{id}", "/users/{id}"]);
        let service = stack.compose_routed(patterns, slow);

        let exempt = http::Request::builder()
            .uri("/events/42")
            .body(axum::body::Body::empty())
            .expect("request");
        let timed = http::Request::builder()
            .uri("/users/42")
            .body(axum::body::Body::empty())
            .expect("request");

        // The exempt pattern outlives the budget…
        let exempt = tokio::spawn(service.clone().oneshot(exempt));
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        assert!(!exempt.is_finished(), "an exempt route must not be timed");
        exempt.abort();

        // …and everything else still gets its 504.
        let response = service.oneshot(timed).await.expect("infallible");
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn spelling_the_exempt_pattern_into_the_url_buys_nothing() {
        use tower::ServiceExt as _;

        let slow = Route::new(tower::service_fn(|_req: Request| async move {
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            Ok::<_, Infallible>(Response::new(axum::body::Body::empty()))
        }));

        let mut stack = MiddlewareStack::bare();
        stack.enable(Slot::Timeout);
        stack.timeout(std::time::Duration::from_millis(10));
        stack.timeout_exempt("/events/{id}");

        // The exemption is compared against the pattern the *route table*
        // produced, never against the text of the request. This application has
        // no `/events` route at all, so a client writing the exemption out
        // percent-encoded resolves to nothing and is timed like anything else.
        let request = http::Request::builder()
            .uri("/events/%7Bid%7D")
            .body(axum::body::Body::empty())
            .expect("request");
        let response = stack
            .compose_routed(RoutePatterns::new(["/logs/{id}"]), slow)
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.status(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn the_resolver_is_not_a_slot_and_does_not_move_metrics_off_the_inside() {
        // `metrics must be innermost` is checked over the stack's entries, and
        // resolving the pattern is not one — it is part of how the stack is
        // installed, not a position an application can reorder.
        let mut stack = MiddlewareStack::standard();
        stack.metrics(Arc::new(Routes::default()));
        assert!(stack.validate().is_empty());
        assert_eq!(names(&stack).last().map(String::as_str), Some("metrics"));
    }
}
