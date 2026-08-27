//! Self-describing extractors.
//!
//! An Axum extractor answers "how do I build myself from a request". A Moso
//! extractor answers that **and** "what does my presence mean for the API
//! contract". The second question is what lets the OpenAPI document be
//! generated with no per-handler annotation, and what lets boot-time validation
//! know which providers a route needs.
//!
//! # The two traits
//!
//! - [`Extract`] builds from the request *parts* — headers, URI, extensions.
//!   Any number per handler, in any order.
//! - [`ExtractBody`] consumes the request body. **At most one per handler, and
//!   it must be last**, which `#[endpoint]` checks at macro time so the error
//!   points at the parameter rather than into a trait resolution.
//!
//! There is deliberately no blanket `impl<T: Extract> ExtractBody for T`. It
//! would collide with the real body extractors under coherence, and it would
//! make the marker types that distinguish the two [`Handler`](crate::Handler)
//! families ambiguous. The macro enforces the ordering instead, where the error
//! message can be written by hand.
//!
//! # The built-in set
//!
//! | Extractor | Kind | Contributes to the document |
//! | --- | --- | --- |
//! | [`Path<T>`] | parts | path parameters, from `T`'s fields |
//! | [`Query<T>`] | parts | query parameters, with defaults and constraints |
//! | [`Headers<T>`] | parts | header parameters |
//! | [`Cookies`] | parts | nothing |
//! | [`Inject<T>`](crate::Inject) | parts | nothing; contributes a `ProviderReq` |
//! | [`Depends<T>`](crate::Depends) | parts | whatever `T::describe` says |
//! | [`RequestId`], [`ClientIp`], [`Extension<T>`] | parts | nothing |
//! | [`Json<T>`] | body | `requestBody`, plus 400 and 422 |
//! | [`Form<T>`] | body | `requestBody` (urlencoded), plus 400 and 422 |
//! | [`Bytes`], [`Text`] | body | `requestBody` as binary or text |
//! | [`BodyStream`] | body | `requestBody` as a stream |
//! | [`RawBody`] | body | `requestBody: {}` — the honest escape hatch |
//!
//! # Validation happens through exactly one constructor
//!
//! Five extractors validate — [`Json<T>`], [`Form<T>`], [`Query<T>`],
//! [`Headers<T>`] and [`Path<T>`] — and none of them builds its own
//! `ValidationCtx`. They all call
//! [`RequestCtx::validation`](crate::RequestCtx::validation) with their pointer
//! root:
//!
//! | Extractor | Root | Constant |
//! | --- | --- | --- |
//! | [`Json<T>`], [`Form<T>`] | `""` | [`BODY_POINTER_ROOT`] |
//! | [`Query<T>`] | `/query` | [`QUERY_POINTER_ROOT`] |
//! | [`Path<T>`] | `/path` | [`PATH_POINTER_ROOT`] |
//! | [`Headers<T>`] | `/header` | [`HEADER_POINTER_ROOT`] |
//!
//! That one constructor is where the registered
//! [`MessageProvider`](moso_schema::MessageProvider) and the request's
//! `Accept-Language` locale are attached, so a sixth extractor written next year
//! cannot ship the bundled English by forgetting a line. An extractor of your
//! own gets the same behaviour for free by calling it.
//!
//! # Which extractors describe nothing, and why
//!
//! [`Inject<T>`](crate::Inject), [`Cookies`], [`RequestId`], [`ClientIp`],
//! [`ConnectInfo<T>`], [`Extension<T>`], [`MatchedPath`], `Method`, `Uri`,
//! `Version` and `HeaderMap` contribute nothing. None of them corresponds to a
//! fact a client can act on: "this handler read the request method" is not part
//! of an API contract, and an injected connection pool certainly is not. The
//! one that looks like an exception is [`Cookies`] — a cookie *can* be a
//! security scheme, but then it is documented by the
//! [`Dependency`](crate::Dependency) that authenticates with it, not twice.
//!
//! # Interoperating with Axum
//!
//! [`Opaque<T>`] adapts any Axum `FromRequestParts` into an [`Extract`], and
//! [`OpaqueBody<T>`] adapts any Axum `FromRequest` into an [`ExtractBody`].
//! They are separate types on purpose: a single wrapper implementing both
//! traits would make the handler marker ambiguous for every handler ending in
//! one.
//!
//! Neither contributes anything to the OpenAPI document. That is the honest
//! default — an adapter cannot know what the wrapped extractor means — and the
//! documentation says so rather than inventing a plausible-looking schema.
//!
//! The reverse direction is [`MosoExt<T>`] and [`MosoExtBody<T>`], which make a
//! Moso extractor usable in a plain Axum handler. It is a wrapper rather than
//! the blanket `impl<T: Extract> axum::extract::FromRequestParts<()> for T` the
//! design sketch imagined: that impl is forbidden by the orphan rule, since
//! `T` is an uncovered type parameter in an impl of a foreign trait. Both
//! wrappers read the [`RequestCtx`] the handler adapter places in the request
//! extensions.

pub mod body;
pub mod cookies;
pub mod form;
pub mod headers;
pub mod json;
pub mod misc;
#[cfg(feature = "multipart")]
pub mod multipart;
pub mod path;
pub mod query;

use std::future::Future;

use futures_util::FutureExt;
use http::StatusCode;
use http_body_util::BodyExt;
use moso_openapi::OperationBuilder;

use crate::Request;
use crate::ctx::RequestCtx;
use crate::di::ProviderReq;
use crate::error::{Error, ErrorKind, Result};

pub use crate::di::{Depends, Inject};
pub use crate::extract::body::{BodyStream, Bytes, RawBody, Text, read_body_limited, read_limited};
#[cfg(feature = "private-cookies")]
pub use crate::extract::cookies::PrivateCookies;
pub use crate::extract::cookies::{
    Cookie, CookieDefaults, CookieJar, CookieKey, Cookies, SameSite, SignedCookies,
    jar_from_headers,
};
pub use crate::extract::form::{Form, TRUTHY_FORM_VALUES, is_truthy};
pub use crate::extract::headers::{
    HEADER_POINTER_ROOT, Headers, REDACTED_HEADERS, header_name_for_field, is_redacted,
};
pub use crate::extract::json::{BODY_POINTER_ROOT, Json, check_json_depth, is_json_content_type};
pub use crate::extract::misc::{ClientIp, ConnectInfo, Extension, MatchedPath, RequestId};
#[cfg(feature = "multipart")]
pub use crate::extract::multipart::{Field, Multipart, MultipartLimits};
pub use crate::extract::path::{
    PATH_POINTER_ROOT, Path, PathShape, assert_modern_path, has_legacy_syntax, path_shape,
    template_parameters,
};
pub use crate::extract::query::{QUERY_POINTER_ROOT, Query, QueryMap, QueryValue};

/// Built from the request head. Any number per handler.
///
/// Implemented by every built-in extractor — `Path<T>`, `Query<T>`,
/// `Headers<T>`, `Cookies`, `Inject<T>`, `Depends<T>`, `RequestId`, `ClientIp`
/// — and by anything you write yourself. `#[endpoint]` calls [`Extract::extract`]
/// once per parameter, in declaration order, before the handler body runs, and
/// calls [`Extract::describe`] once per route at `App::build()` to put the
/// parameter in the OpenAPI document.
///
/// ```
/// use moso::prelude::*;
/// use moso::openapi::Param;
/// use moso::deps::http::request::Parts;
/// use moso::{Extract, ProviderReq};
///
/// /// Which customer this request belongs to.
/// pub struct Tenant(pub String);
///
/// impl Extract for Tenant {
///     const PROVIDER_REQ: &'static [ProviderReq] = &[];
///
///     fn describe(op: &mut OperationBuilder) {
///         op.parameter(Param::header("x-tenant").required(false).schema_of::<String>());
///         op.response(404, ResponseSpec::problem("Unknown tenant"));
///     }
///
///     async fn extract(parts: &mut Parts, _ctx: &RequestCtx) -> Result<Self> {
///         parts
///             .headers
///             .get("x-tenant")
///             .and_then(|v| v.to_str().ok())
///             .map(|v| Tenant(v.to_owned()))
///             .ok_or_else(|| Error::not_found("tenant"))
///     }
/// }
///
/// /// Show the caller's own tenant.
/// #[endpoint]
/// async fn whoami(tenant: Tenant) -> Result<Json<String>> {
///     Ok(Json(tenant.0))
/// }
/// # fn main() { assert_eq!(Router::new().get("/me", moso::ep!(whoami)).len(), 1); }
/// ```
///
/// [`Extract::describe`] has no default body. A default of "contributes
/// nothing" would silently produce wrong documentation for the extractor that
/// most needed to speak up, so writing one line is required.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used as a handler parameter",
    label = "not an extractor",
    note = "extractors: `Path<T>`, `Query<T>`, `Headers<T>`, `Cookies`, `Inject<T>`, `Depends<T>`",
    note = "a request body is `Json<T>`, `Form<T>`, `Bytes` or `Text`, and must be last",
    note = "help: for an application-lifetime value: `Inject<{Self}>`, registered with \
            `App::provide`",
    note = "help: for a per-request value: `#[derive(moso::Dependency)]` on `{Self}`, then take \
            `Depends<{Self}>`",
    note = "help: to use an Axum extractor unchanged, wrap it: `Opaque<{Self}>`"
)]
pub trait Extract: Sized + Send {
    /// Contribute parameters, security requirements and responses to the
    /// operation being described.
    fn describe(op: &mut OperationBuilder);

    /// Providers this extractor reads. Checked at boot, never at runtime.
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    /// Build the value, or fail with an [`Error`] that becomes the response.
    ///
    /// `parts` is `&mut` so an extractor may take a header out of the map
    /// rather than clone it; later extractors see the modified parts, which is
    /// how `Cookies` hands its jar to a dependency.
    fn extract<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> impl Future<Output = Result<Self>> + Send + 'a;
}

/// Consumes the request body. At most one per handler, and it must be last.
///
/// The ordering rule is enforced by `#[endpoint]` rather than by the type
/// system, because a hand-written macro error beats anything trait resolution
/// can produce:
///
/// ```text
/// error: request body extractor must be the last parameter
///   --> src/routes/users.rs:12:5
///    |
/// 11 |     Json(body): Json<CreateUser>,
///    |                 ---------------- this extractor consumes the request body
/// 12 |     Inject(db): Inject<Db>,
///    |     ^^^^^^^^^^^^^^^^^^^^^^ ...so no parameter may follow it
/// ```
///
/// Implementing it by hand is for a media type Moso does not ship. The whole
/// contract is two methods: say what the body *is*, then read it.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::{ExtractBody, read_limited};
/// use moso::openapi::{ContentType, OperationBuilder, ResponseSpec};
/// use moso::schema::json_schema::{JsonType, SchemaNode, SchemaRef};
/// use moso::Request;
///
/// /// A body of newline-separated lines, read as text.
/// ///
/// /// The name ends in `Body` on purpose — see the note below.
/// pub struct LinesBody(pub Vec<String>);
///
/// impl ExtractBody for LinesBody {
///     fn describe(op: &mut OperationBuilder) {
///         op.request_body(
///             ContentType::Text,
///             SchemaRef::inline(SchemaNode::of_type(JsonType::String)),
///             true,
///         );
///         op.response(413, ResponseSpec::problem("The body exceeded `http.body_max`"));
///     }
///
///     async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
///         let bytes = read_limited(req, ctx.limits().body_max).await?;
///         let text = core::str::from_utf8(bytes.as_slice())
///             .map_err(|_| Error::bad_request("the body is not UTF-8"))?;
///         Ok(LinesBody(text.lines().map(str::to_owned).collect()))
///     }
/// }
///
/// /// Count the lines a client sent.
/// #[endpoint]
/// async fn count(LinesBody(lines): LinesBody) -> Result<Json<usize>> {
///     Ok(Json(lines.len()))
/// }
/// # fn main() { assert_eq!(Router::new().post("/count", moso::ep!(count)).len(), 1); }
/// ```
///
/// # Name your body extractor so `#[endpoint]` can see it
///
/// A proc macro sees tokens, not types, so `#[endpoint]` decides which of the
/// last parameter's two traits to name from the type's **name**: one of the
/// built-ins (`Json`, `Form`, `Bytes`, `Text`, `RawBody`, `BodyStream`,
/// `Multipart`, `OpaqueBody`), or a name that starts or ends with `Body`, or
/// ends with `Multipart` or `Upload`. A body extractor called anything else is
/// treated as a parts extractor, and the failure is
/// `` `YourType` cannot be used as a handler parameter `` — accurate, but about
/// the wrong trait. Ending the name in `Body` is the fix.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used as a request body",
    label = "not a body extractor",
    note = "built-in body extractors: `Json<T>`, `Form<T>`, `Bytes`, `Text`, `BodyStream`, \
            `RawBody`",
    note = "for `Json<T>` and `Form<T>`, `T` must derive `moso::Schema`",
    note = "help: to use an Axum body extractor unchanged, wrap it: `OpaqueBody<{Self}>`",
    note = "a handler has at most one body extractor and it must be the last parameter"
)]
pub trait ExtractBody: Sized + Send {
    /// Contribute the `requestBody` and the responses reading it can produce.
    fn describe(op: &mut OperationBuilder);

    /// Providers this extractor reads. Checked at boot.
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    /// Consume the request and build the value.
    fn extract_body<'a>(
        req: Request,
        ctx: &'a RequestCtx,
    ) -> impl Future<Output = Result<Self>> + Send + 'a;
}

// ---------------------------------------------------------------------------
// Axum interop — Axum extractor in a Moso handler
// ---------------------------------------------------------------------------

/// Use any Axum `FromRequestParts` extractor in a Moso handler.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::Opaque;
/// use moso::response::NoContent;
///
/// /// Log the URI as the client wrote it, before any nesting rewrote it.
/// #[endpoint]
/// async fn show(Opaque(uri): Opaque<axum::extract::OriginalUri>) -> Result<NoContent> {
///     let _ = uri;
///     Ok(NoContent)
/// }
/// # fn main() { assert_eq!(Router::new().get("/x", moso::ep!(show)).len(), 1); }
/// ```
///
/// Contributes nothing to the OpenAPI document. If the wrapped extractor
/// corresponds to a documented parameter, say so explicitly with
/// `#[endpoint(response(..))]` or by writing a small [`Extract`] impl of your
/// own — an adapter that guessed would produce documentation that lies.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Opaque<T>(pub T);

impl<T> Opaque<T> {
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> Extract for Opaque<T>
where
    T: axum::extract::FromRequestParts<()> + Send + 'static,
{
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let _ = ctx;
        match T::from_request_parts(parts, &()).await {
            Ok(value) => Ok(Opaque(value)),
            Err(rejection) => Err(axum_rejection(rejection)),
        }
    }
}

/// Use any Axum `FromRequest` (body) extractor in a Moso handler.
///
/// Separate from [`Opaque`] on purpose: one wrapper implementing both
/// [`Extract`] and [`ExtractBody`] would make the handler marker type ambiguous
/// for every handler that ended in one, and the resulting inference error is
/// exactly the kind of message this framework exists to avoid.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpaqueBody<T>(pub T);

impl<T> OpaqueBody<T> {
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> ExtractBody for OpaqueBody<T>
where
    T: axum::extract::FromRequest<()> + Send + 'static,
{
    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        let _ = ctx;
        match T::from_request(req, &()).await {
            Ok(value) => Ok(OpaqueBody(value)),
            Err(rejection) => Err(axum_rejection(rejection)),
        }
    }
}

// ---------------------------------------------------------------------------
// Axum interop — Moso extractor in an Axum handler
// ---------------------------------------------------------------------------

/// Use a Moso [`Extract`] in a plain Axum handler.
///
/// ```
/// use moso::prelude::*;
/// use moso::extract::MosoExt;
///
/// /// The query string this listing accepts.
/// #[derive(Schema)]
/// pub struct ListPosts {
///     /// How many rows to return.
///     pub limit: Option<u32>,
/// }
///
/// // A plain Axum handler, not a Moso `#[endpoint]`.
/// async fn handler(MosoExt(Query(q)): MosoExt<Query<ListPosts>>) -> impl IntoResponse {
///     format!("{:?}", q.limit)
/// }
/// # fn main() {
/// #     let _: axum::Router = axum::Router::new().route("/", axum::routing::get(handler));
/// # }
/// ```
///
/// A wrapper rather than a blanket `impl<T: Extract> FromRequestParts<()> for T`
/// because the orphan rule forbids one: `T` is an uncovered type parameter in
/// an impl of a trait this crate does not own. The wrapper costs one pattern in
/// the parameter list and is the only form that compiles.
///
/// Requires the request to have passed through a Moso router, which is what
/// puts a [`RequestCtx`] in the extensions. Mounting an Axum router *outside*
/// Moso and using this inside it yields a 500 that says exactly that.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MosoExt<T>(pub T);

impl<T> MosoExt<T> {
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<S, T> axum::extract::FromRequestParts<S> for MosoExt<T>
where
    T: Extract,
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> core::result::Result<Self, Error> {
        let ctx = ctx_from_parts(parts)?;
        <T as Extract>::extract(parts, &ctx).await.map(MosoExt)
    }
}

/// Use a Moso [`ExtractBody`] in a plain Axum handler. See [`MosoExt`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MosoExtBody<T>(pub T);

impl<T> MosoExtBody<T> {
    /// The wrapped value.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<S, T> axum::extract::FromRequest<S> for MosoExtBody<T>
where
    T: ExtractBody,
    S: Send + Sync,
{
    type Rejection = Error;

    async fn from_request(req: Request, _state: &S) -> core::result::Result<Self, Error> {
        let (parts, body) = req.into_parts();
        let ctx = ctx_from_parts(&parts)?;
        let req = Request::from_parts(parts, body);
        <T as ExtractBody>::extract_body(req, &ctx)
            .await
            .map(MosoExtBody)
    }
}

/// Convert an Axum rejection into a Moso [`Error`].
///
/// Axum rejections already carry a status and a message; this maps the status
/// onto the taxonomy so the result renders as `problem+json` like every other
/// error, rather than as Axum's plain-text body.
pub fn axum_rejection<R: axum::response::IntoResponse>(rejection: R) -> Error {
    let response = rejection.into_response();
    let (parts, body) = response.into_parts();
    let error = Error::new(kind_for_status(parts.status));
    match rendered_body(body) {
        Some(detail) if !detail.is_empty() => error.with_detail(detail),
        _ => error,
    }
}

/// Read a rejection's body without awaiting.
///
/// Every Axum rejection renders a complete in-memory body, so the future is
/// already resolved and `now_or_never` returns it. Returning `None` rather than
/// blocking is the right failure for the one-in-a-thousand rejection that
/// streams: the status still carries the meaning.
fn rendered_body(body: axum::body::Body) -> Option<String> {
    let collected = body.collect().now_or_never()?.ok()?;
    String::from_utf8(collected.to_bytes().to_vec()).ok()
}

/// The taxonomy entry an HTTP status belongs to.
///
/// Used for foreign statuses — an Axum rejection, a proxied upstream response —
/// where all we have is the number.
pub(crate) fn kind_for_status(status: StatusCode) -> ErrorKind {
    // Matched on the number rather than on `StatusCode` constants: an
    // associated const is only usable as a pattern if its type is
    // structural-match, which `StatusCode` does not promise.
    match status.as_u16() {
        400 => ErrorKind::BadRequest,
        401 => ErrorKind::Unauthenticated,
        403 => ErrorKind::Forbidden,
        404 => ErrorKind::NotFound,
        405 => ErrorKind::MethodNotAllowed,
        406 => ErrorKind::NotAcceptable,
        409 => ErrorKind::Conflict,
        410 => ErrorKind::Gone,
        412 => ErrorKind::PreconditionFailed,
        413 => ErrorKind::PayloadTooLarge,
        414 => ErrorKind::UriTooLong,
        415 => ErrorKind::UnsupportedMedia,
        416 => ErrorKind::RangeNotSatisfiable,
        422 => ErrorKind::Validation,
        423 => ErrorKind::Locked,
        429 => ErrorKind::TooManyRequests,
        501 => ErrorKind::NotImplemented,
        502 => ErrorKind::BadGateway,
        503 => ErrorKind::Unavailable,
        504 => ErrorKind::GatewayTimeout,
        other if (400..500).contains(&other) => ErrorKind::BadRequest,
        _ => ErrorKind::Internal,
    }
}

/// The [`RequestCtx`] the handler adapter stored in the request extensions.
///
/// This is the bridge that lets a Moso extractor be used from an Axum handler:
/// Axum's traits have nowhere to pass a context, so the adapter puts one in the
/// request where anything downstream can find it.
///
/// # Errors
/// A 500 naming the problem when the request did not come through a Moso router
/// — which means the caller mounted an Axum router directly and then used a
/// Moso extractor inside it.
pub fn ctx_from_parts(parts: &http::request::Parts) -> Result<RequestCtx> {
    parts
        .extensions
        .get::<RequestCtx>()
        .cloned()
        .ok_or_else(|| {
            Error::internal_msg(
                "no `RequestCtx` in the request extensions: this request did not pass through a \
             Moso router. `MosoExt<T>` and `ctx_from_parts` only work inside a route mounted \
             with `Router::get`, `Router::post`, … — an `axum::Router` mounted with \
             `Router::mount_axum` runs outside the adapter that installs the context",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_extract<T: Extract>() {}
    fn assert_extract_body<T: ExtractBody>() {}
    fn assert_axum_parts<T: axum::extract::FromRequestParts<()>>() {}
    fn assert_axum_body<T: axum::extract::FromRequest<()>>() {}

    #[test]
    fn an_axum_extractor_works_in_a_moso_handler() {
        assert_extract::<Opaque<axum::extract::OriginalUri>>();
        assert_extract::<Opaque<http::Method>>();
        assert_extract_body::<OpaqueBody<axum::body::Bytes>>();
        assert_extract_body::<OpaqueBody<String>>();
        #[cfg(feature = "multipart")]
        assert_extract_body::<OpaqueBody<axum::extract::Multipart>>();
    }

    #[test]
    fn a_moso_extractor_works_in_an_axum_handler() {
        assert_axum_parts::<MosoExt<RequestId>>();
        assert_axum_parts::<MosoExt<Cookies>>();
        assert_axum_parts::<MosoExt<http::HeaderMap>>();
        assert_axum_body::<MosoExtBody<Bytes>>();
        assert_axum_body::<MosoExtBody<Text>>();
        assert_axum_body::<MosoExtBody<RawBody>>();
    }

    #[test]
    fn every_built_in_extractor_satisfies_its_trait() {
        assert_extract::<Cookies>();
        assert_extract::<RequestId>();
        assert_extract::<ClientIp>();
        assert_extract::<ConnectInfo<std::net::SocketAddr>>();
        assert_extract::<Extension<u32>>();
        assert_extract::<MatchedPath>();
        assert_extract::<RequestCtx>();
        assert_extract::<http::HeaderMap>();
        assert_extract::<http::Method>();
        assert_extract::<http::Uri>();
        assert_extract::<http::Version>();
        assert_extract::<()>();
        assert_extract::<Option<RequestId>>();
        assert_extract_body::<Bytes>();
        assert_extract_body::<Text>();
        assert_extract_body::<RawBody>();
        assert_extract_body::<BodyStream>();
        #[cfg(feature = "multipart")]
        assert_extract_body::<crate::extract::multipart::Multipart>();
    }

    #[test]
    fn opaque_wrappers_are_transparent() {
        assert_eq!(Opaque(7u8).into_inner(), 7);
        assert_eq!(OpaqueBody("body").into_inner(), "body");
        assert_eq!(MosoExt(7u8).into_inner(), 7);
        assert_eq!(MosoExtBody("body").into_inner(), "body");
    }

    #[test]
    fn statuses_map_onto_the_taxonomy() {
        assert_eq!(
            kind_for_status(StatusCode::BAD_REQUEST),
            ErrorKind::BadRequest
        );
        assert_eq!(
            kind_for_status(StatusCode::UNPROCESSABLE_ENTITY),
            ErrorKind::Validation
        );
        assert_eq!(
            kind_for_status(StatusCode::PAYLOAD_TOO_LARGE),
            ErrorKind::PayloadTooLarge
        );
        // An unmapped 4xx is still the client's problem…
        assert_eq!(
            kind_for_status(StatusCode::EXPECTATION_FAILED),
            ErrorKind::BadRequest
        );
        // …and anything else is ours.
        assert_eq!(
            kind_for_status(StatusCode::INTERNAL_SERVER_ERROR),
            ErrorKind::Internal
        );
        assert_eq!(kind_for_status(StatusCode::OK), ErrorKind::Internal);
    }

    #[test]
    fn a_rejection_body_is_read_without_awaiting() {
        let body = axum::body::Body::from("Failed to deserialize the JSON body");
        assert_eq!(
            rendered_body(body).as_deref(),
            Some("Failed to deserialize the JSON body")
        );
    }
}
