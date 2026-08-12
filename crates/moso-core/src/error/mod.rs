//! One concrete error type, one wire format.
//!
//! Handlers return [`Result<T>`], so `?` works across every battery without a
//! `map_err`. [`Error`] carries a taxonomy ([`ErrorKind`]) that decides the
//! status code, the machine-readable `type` URI, and — the part that matters —
//! whether the detail is safe to show the client.
//!
//! # Rules this module exists to enforce
//!
//! 1. **The wire format is RFC 9457** `application/problem+json`, for framework
//!    errors as well as application ones. See [`problem`].
//! 2. **Client-safe by default.** A 5xx never emits its `detail`, its `source`
//!    or its backtrace unless `http.expose_internal_errors` is set. The client
//!    gets a title and a `request_id` an operator can grep for.
//! 3. **Logged exactly once**, at the boundary, by the `catch_error` layer —
//!    never at the construction site. [`Error`] is a value, not an event.
//! 4. **Documented.** Every error a handler can produce appears in the
//!    generated OpenAPI responses.
//!
//! # Layout
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`problem`] | the [`Problem`] wire type, `IntoResponse`, the HTML fallback |
//! | [`boot`] | [`BootError`], [`BootErrors`] and the grouped boot report |

pub mod boot;
pub mod problem;

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use moso_schema::{FieldError, ValidationErrors};
use serde::Serialize;
use serde_json::Value;

pub use crate::error::boot::{BootError, BootErrors, ProviderRequirement, RouteRef};
pub use crate::error::problem::Problem;

/// The result type every Moso API returns.
///
/// Defaulting `E` to [`Error`] means `Result<T>` reads as "this can fail the
/// way everything else fails", while `Result<T, MyError>` is still available
/// where a caller genuinely needs the narrower type.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// A type-erased error, as `std` models it.
pub type BoxError = Box<dyn core::error::Error + Send + Sync + 'static>;

/// The base URI space for Moso's own `type` values.
///
/// An application overrides it per error with [`Error::with_type`], or wholesale
/// through `#[derive(moso::Error)]`.
pub const ERROR_TYPE_BASE: &str = "https://moso.rs/errors/";

// ---------------------------------------------------------------------------
// ErrorKind
// ---------------------------------------------------------------------------

/// The taxonomy that decides status, `type` URI and disclosure.
///
/// `#[non_exhaustive]`: new kinds are added in minor releases, so match with a
/// `_` arm. Use [`Error::status`] rather than matching to obtain a status code.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ErrorKind {
    /// 400 — the request is syntactically malformed.
    BadRequest,
    /// 401 — no credentials, or credentials that do not identify anyone.
    Unauthenticated,
    /// 403 — identified, but not permitted.
    Forbidden,
    /// 404 — no such resource.
    NotFound,
    /// 405 — the path exists, the method does not.
    MethodNotAllowed,
    /// 406 — no representation matches the `Accept` header.
    NotAcceptable,
    /// 409 — unique violation, optimistic-lock failure, state conflict.
    Conflict,
    /// 410 — the resource existed and is permanently gone.
    Gone,
    /// 412 — an `If-Match` / `If-Unmodified-Since` precondition failed.
    PreconditionFailed,
    /// 413 — the body exceeded a configured limit.
    PayloadTooLarge,
    /// 414 — the URI exceeded `http.uri_max`.
    UriTooLong,
    /// 415 — the `Content-Type` is not one this operation accepts.
    UnsupportedMedia,
    /// 416 — the `Range` header cannot be satisfied.
    RangeNotSatisfiable,
    /// 422 — the request parsed but failed validation. Carries field errors.
    Validation,
    /// 423 — the resource is locked by another actor.
    Locked,
    /// 429 — rate limited. Carries `Retry-After`.
    TooManyRequests,
    /// 431 — the head carried more header fields, or more header bytes, than
    /// `http.header_max_count` / `http.header_max_bytes` allow.
    HeaderFieldsTooLarge,
    /// 500 — a bug, or a dependency failing in a way we did not classify.
    Internal,
    /// 501 — the operation is routed but not implemented.
    NotImplemented,
    /// 502 — an upstream returned something unusable.
    BadGateway,
    /// 503 — shutting down, or a hard dependency is down.
    Unavailable,
    /// 504 — an upstream did not answer in time.
    GatewayTimeout,
    /// 504 — *our* timeout layer fired.
    Timeout,
    /// Build-time only. Never reaches a client; rendered by the boot report.
    Boot(BootErrors),
}

impl ErrorKind {
    /// Every kind that is a response, in taxonomy order.
    ///
    /// [`ErrorKind::Boot`] is absent because it carries data and is not a
    /// response. Used by the status/type snapshot tests and by `moso openapi`
    /// when it emits the error-code reference.
    pub const RESPONSE_KINDS: &'static [ErrorKind] = &[
        ErrorKind::BadRequest,
        ErrorKind::Unauthenticated,
        ErrorKind::Forbidden,
        ErrorKind::NotFound,
        ErrorKind::MethodNotAllowed,
        ErrorKind::NotAcceptable,
        ErrorKind::Conflict,
        ErrorKind::Gone,
        ErrorKind::PreconditionFailed,
        ErrorKind::PayloadTooLarge,
        ErrorKind::UriTooLong,
        ErrorKind::UnsupportedMedia,
        ErrorKind::RangeNotSatisfiable,
        ErrorKind::Validation,
        ErrorKind::Locked,
        ErrorKind::TooManyRequests,
        ErrorKind::HeaderFieldsTooLarge,
        ErrorKind::Internal,
        ErrorKind::NotImplemented,
        ErrorKind::BadGateway,
        ErrorKind::Unavailable,
        ErrorKind::GatewayTimeout,
        ErrorKind::Timeout,
    ];

    /// The HTTP status this kind maps to.
    ///
    /// [`ErrorKind::Boot`] maps to 500 for completeness; it is never a response
    /// in practice because the process exits before the listener binds.
    pub fn status(&self) -> StatusCode {
        match self {
            ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
            ErrorKind::Unauthenticated => StatusCode::UNAUTHORIZED,
            ErrorKind::Forbidden => StatusCode::FORBIDDEN,
            ErrorKind::NotFound => StatusCode::NOT_FOUND,
            ErrorKind::MethodNotAllowed => StatusCode::METHOD_NOT_ALLOWED,
            ErrorKind::NotAcceptable => StatusCode::NOT_ACCEPTABLE,
            ErrorKind::Conflict => StatusCode::CONFLICT,
            ErrorKind::Gone => StatusCode::GONE,
            ErrorKind::PreconditionFailed => StatusCode::PRECONDITION_FAILED,
            ErrorKind::PayloadTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            ErrorKind::UriTooLong => StatusCode::URI_TOO_LONG,
            ErrorKind::UnsupportedMedia => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ErrorKind::RangeNotSatisfiable => StatusCode::RANGE_NOT_SATISFIABLE,
            ErrorKind::Validation => StatusCode::UNPROCESSABLE_ENTITY,
            ErrorKind::Locked => StatusCode::LOCKED,
            ErrorKind::TooManyRequests => StatusCode::TOO_MANY_REQUESTS,
            ErrorKind::HeaderFieldsTooLarge => StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            ErrorKind::Internal | ErrorKind::Boot(_) => StatusCode::INTERNAL_SERVER_ERROR,
            ErrorKind::NotImplemented => StatusCode::NOT_IMPLEMENTED,
            ErrorKind::BadGateway => StatusCode::BAD_GATEWAY,
            ErrorKind::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            ErrorKind::GatewayTimeout | ErrorKind::Timeout => StatusCode::GATEWAY_TIMEOUT,
        }
    }

    /// The `type` URI slug, appended to [`ERROR_TYPE_BASE`].
    ///
    /// Stable across releases: it is a public identifier clients match on. Each
    /// slug is the variant's name in kebab case, which is the rule a reader can
    /// apply without consulting a table.
    pub fn slug(&self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "bad-request",
            ErrorKind::Unauthenticated => "unauthenticated",
            ErrorKind::Forbidden => "forbidden",
            ErrorKind::NotFound => "not-found",
            ErrorKind::MethodNotAllowed => "method-not-allowed",
            ErrorKind::NotAcceptable => "not-acceptable",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Gone => "gone",
            ErrorKind::PreconditionFailed => "precondition-failed",
            ErrorKind::PayloadTooLarge => "payload-too-large",
            ErrorKind::UriTooLong => "uri-too-long",
            ErrorKind::UnsupportedMedia => "unsupported-media",
            ErrorKind::RangeNotSatisfiable => "range-not-satisfiable",
            ErrorKind::Validation => "validation",
            ErrorKind::Locked => "locked",
            ErrorKind::TooManyRequests => "too-many-requests",
            ErrorKind::HeaderFieldsTooLarge => "header-fields-too-large",
            ErrorKind::Internal => "internal",
            ErrorKind::NotImplemented => "not-implemented",
            ErrorKind::BadGateway => "bad-gateway",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::GatewayTimeout => "gateway-timeout",
            ErrorKind::Timeout => "timeout",
            ErrorKind::Boot(_) => "boot",
        }
    }

    /// The full default `type` URI: [`ERROR_TYPE_BASE`] plus [`slug`].
    ///
    /// A `&'static str` rather than a formatted `String`, because every
    /// constructed error pays for it and none of them need an allocation.
    ///
    /// [`slug`]: ErrorKind::slug
    pub fn type_uri(&self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "https://moso.rs/errors/bad-request",
            ErrorKind::Unauthenticated => "https://moso.rs/errors/unauthenticated",
            ErrorKind::Forbidden => "https://moso.rs/errors/forbidden",
            ErrorKind::NotFound => "https://moso.rs/errors/not-found",
            ErrorKind::MethodNotAllowed => "https://moso.rs/errors/method-not-allowed",
            ErrorKind::NotAcceptable => "https://moso.rs/errors/not-acceptable",
            ErrorKind::Conflict => "https://moso.rs/errors/conflict",
            ErrorKind::Gone => "https://moso.rs/errors/gone",
            ErrorKind::PreconditionFailed => "https://moso.rs/errors/precondition-failed",
            ErrorKind::PayloadTooLarge => "https://moso.rs/errors/payload-too-large",
            ErrorKind::UriTooLong => "https://moso.rs/errors/uri-too-long",
            ErrorKind::UnsupportedMedia => "https://moso.rs/errors/unsupported-media",
            ErrorKind::RangeNotSatisfiable => "https://moso.rs/errors/range-not-satisfiable",
            ErrorKind::Validation => "https://moso.rs/errors/validation",
            ErrorKind::Locked => "https://moso.rs/errors/locked",
            ErrorKind::TooManyRequests => "https://moso.rs/errors/too-many-requests",
            ErrorKind::HeaderFieldsTooLarge => "https://moso.rs/errors/header-fields-too-large",
            ErrorKind::Internal => "https://moso.rs/errors/internal",
            ErrorKind::NotImplemented => "https://moso.rs/errors/not-implemented",
            ErrorKind::BadGateway => "https://moso.rs/errors/bad-gateway",
            ErrorKind::Unavailable => "https://moso.rs/errors/unavailable",
            ErrorKind::GatewayTimeout => "https://moso.rs/errors/gateway-timeout",
            ErrorKind::Timeout => "https://moso.rs/errors/timeout",
            ErrorKind::Boot(_) => "https://moso.rs/errors/boot",
        }
    }

    /// The default `title`, which is the human-readable name of the *kind* and
    /// never contains request-specific text.
    pub fn title(&self) -> &'static str {
        match self {
            ErrorKind::BadRequest => "Bad Request",
            ErrorKind::Unauthenticated => "Unauthenticated",
            ErrorKind::Forbidden => "Forbidden",
            ErrorKind::NotFound => "Not Found",
            ErrorKind::MethodNotAllowed => "Method Not Allowed",
            ErrorKind::NotAcceptable => "Not Acceptable",
            ErrorKind::Conflict => "Conflict",
            ErrorKind::Gone => "Gone",
            ErrorKind::PreconditionFailed => "Precondition Failed",
            ErrorKind::PayloadTooLarge => "Payload Too Large",
            ErrorKind::UriTooLong => "URI Too Long",
            ErrorKind::UnsupportedMedia => "Unsupported Media Type",
            ErrorKind::RangeNotSatisfiable => "Range Not Satisfiable",
            ErrorKind::Validation => "Validation Failed",
            ErrorKind::Locked => "Locked",
            ErrorKind::TooManyRequests => "Too Many Requests",
            ErrorKind::HeaderFieldsTooLarge => "Request Header Fields Too Large",
            ErrorKind::Internal => "Internal Server Error",
            ErrorKind::NotImplemented => "Not Implemented",
            ErrorKind::BadGateway => "Bad Gateway",
            ErrorKind::Unavailable => "Service Unavailable",
            ErrorKind::GatewayTimeout => "Gateway Timeout",
            ErrorKind::Timeout => "Timeout",
            ErrorKind::Boot(_) => "Application Failed To Build",
        }
    }

    /// Whether the `detail` may be sent to the client.
    ///
    /// True for every 4xx, false for every 5xx. `http.expose_internal_errors`
    /// overrides this at render time, never here.
    pub fn detail_is_client_safe(&self) -> bool {
        !self.status().is_server_error()
    }

    /// Whether a client may sensibly retry the same request.
    ///
    /// Drives `moso-jobs` retry decisions and the `retryable` flag in generated
    /// clients, so the semantic is the same everywhere.
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            ErrorKind::TooManyRequests
                | ErrorKind::BadGateway
                | ErrorKind::Unavailable
                | ErrorKind::GatewayTimeout
                | ErrorKind::Timeout
        )
    }

    /// The log level this kind is reported at by the `catch_error` layer.
    ///
    /// 5xx are `ERROR`; 401/403/409/429 (plus 410 and 423, which are the same
    /// shape of "someone is doing something they should not") are `WARN`; every
    /// other 4xx is `DEBUG`, because 404 and 422 are routine and would
    /// otherwise drown the log.
    pub fn log_level(&self) -> tracing::Level {
        if self.status().is_server_error() {
            return tracing::Level::ERROR;
        }
        match self {
            ErrorKind::Unauthenticated
            | ErrorKind::Forbidden
            | ErrorKind::Conflict
            | ErrorKind::Gone
            | ErrorKind::Locked
            | ErrorKind::TooManyRequests => tracing::Level::WARN,
            _ => tracing::Level::DEBUG,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// The error type at the HTTP boundary.
///
/// Construct one with a named constructor ([`Error::not_found`],
/// [`Error::conflict`], …) and refine it with the `with_*` builders. Convert
/// your own error types with `impl From<MyError> for Error` or
/// `#[derive(moso::Error)]`.
///
/// ```
/// use moso::Error;
///
/// let error = Error::conflict("A user with this email already exists")
///     .with_field("/email", "unique", "already taken");
///
/// assert_eq!(error.status(), 409);
///
/// // On the wire it is an RFC 9457 `application/problem+json` document.
/// let response = moso::IntoResponse::into_response(error);
/// assert_eq!(response.headers()["content-type"], "application/problem+json");
/// ```
///
/// The `/email` pointer is an RFC 6901 JSON Pointer into the request body, so a
/// client can attach the message to the field that caused it.
///
/// # Why the payload is boxed
///
/// `Error` is one pointer wide, and everything it carries lives behind it. The
/// error's own members add up to more than 250 bytes — the taxonomy, two
/// `Cow`s, an extension map, the field errors — and `Result<T, Error>` is the
/// return type of **every handler, extractor and dependency in the program**.
/// An unboxed error would make every success path carry that width in a
/// register pair or on the stack.
///
/// The trade is one allocation on the failure path, which is not the hot path
/// and is dominated by rendering a problem document anyway. `anyhow` and
/// `eyre` make the same trade for the same reason.
pub struct Error(Box<ErrorInner>);

/// The contents of an [`Error`]. Private: every member is reachable through an
/// accessor, and keeping the layout private is what lets it change.
struct ErrorInner {
    kind: ErrorKind,
    type_uri: Cow<'static, str>,
    title: Cow<'static, str>,
    detail: Option<Cow<'static, str>>,
    extensions: BTreeMap<Cow<'static, str>, Value>,
    fields: Option<ValidationErrors>,
    source: Option<BoxError>,
    backtrace: Option<Box<std::backtrace::Backtrace>>,
    headers: Option<Box<HeaderMap>>,
}

impl Error {
    // ── constructors ──────────────────────────────────────────────────────

    /// An error of `kind` with that kind's default `type`, `title` and no
    /// detail. The general form; prefer the named constructors.
    pub fn new(kind: ErrorKind) -> Self {
        // Only an `Internal` is worth a backtrace: every other kind is a
        // decision the code made on purpose, and the stack that led there says
        // nothing the route and the detail do not already say. `capture()` is
        // itself a no-op unless `RUST_BACKTRACE` is set, so the common path
        // costs a status check.
        let backtrace = if matches!(kind, ErrorKind::Internal) {
            let backtrace = std::backtrace::Backtrace::capture();
            match backtrace.status() {
                std::backtrace::BacktraceStatus::Captured => Some(Box::new(backtrace)),
                _ => None,
            }
        } else {
            None
        };

        Self(Box::new(ErrorInner {
            type_uri: Cow::Borrowed(kind.type_uri()),
            title: Cow::Borrowed(kind.title()),
            kind,
            detail: None,
            extensions: BTreeMap::new(),
            fields: None,
            source: None,
            backtrace,
            headers: None,
        }))
    }

    /// 400 — the request is malformed in a way the client can fix.
    pub fn bad_request(detail: impl Into<Cow<'static, str>>) -> Self {
        Error::new(ErrorKind::BadRequest).with_detail(detail)
    }

    /// 401 — authentication is required and was absent or unusable.
    pub fn unauthenticated() -> Self {
        Error::new(ErrorKind::Unauthenticated)
            .with_detail("Authentication is required to access this resource")
    }

    /// 403 — the actor is known and not permitted.
    pub fn forbidden(detail: impl Into<Cow<'static, str>>) -> Self {
        Error::new(ErrorKind::Forbidden).with_detail(detail)
    }

    /// 404 — `resource` names what was looked for, not what was asked for:
    /// `Error::not_found("user")` renders as `User not found`.
    pub fn not_found(resource: impl Into<Cow<'static, str>>) -> Self {
        let resource = resource.into();
        let detail = if resource.is_empty() {
            Cow::Borrowed("Not found")
        } else {
            Cow::Owned(format!("{} not found", capitalise(&resource)))
        };
        Error::new(ErrorKind::NotFound).with_detail(detail)
    }

    /// 405, carrying the `Allow` header the caller should have honoured.
    pub fn method_not_allowed(allowed: &[http::Method]) -> Self {
        let mut error = Error::new(ErrorKind::MethodNotAllowed);
        if allowed.is_empty() {
            return error.with_detail("This path does not accept this method");
        }
        let allow = allowed
            .iter()
            .map(http::Method::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        error = error.with_detail(format!("The allowed methods are {allow}"));
        if let Ok(value) = HeaderValue::from_str(&allow) {
            error = error.with_header(http::header::ALLOW, value);
        }
        error
    }

    /// 409 — a unique violation, a lost optimistic lock, or a state conflict.
    pub fn conflict(detail: impl Into<Cow<'static, str>>) -> Self {
        Error::new(ErrorKind::Conflict).with_detail(detail)
    }

    /// 413 — the body exceeded `limit` bytes. The limit is reported to the
    /// client, since a client cannot otherwise discover it.
    pub fn payload_too_large(limit: usize) -> Self {
        Error::new(ErrorKind::PayloadTooLarge)
            .with_detail(format!("The request body exceeds the {limit} byte limit"))
            .with_extension("max_bytes", limit)
    }

    /// 414 — the request target exceeded `limit` bytes.
    ///
    /// The limit is reported the way [`Error::payload_too_large`] reports its
    /// own, and for the same reason: a client cannot discover it any other way,
    /// and "your URL is too long, by an amount I will not tell you" is not
    /// actionable.
    pub fn uri_too_long(limit: usize) -> Self {
        Error::new(ErrorKind::UriTooLong)
            .with_detail(format!("The request target exceeds the {limit} byte limit"))
            .with_extension("max_bytes", limit)
    }

    /// 431 — the head carried more than `limit` header fields.
    ///
    /// Separate from [`Error::headers_too_large`] because the two configured
    /// limits fail for different reasons and a client fixes them differently:
    /// one says "send fewer headers", the other "send smaller ones".
    pub fn too_many_headers(limit: usize) -> Self {
        Error::new(ErrorKind::HeaderFieldsTooLarge)
            .with_detail(format!(
                "The request carries more than the {limit} permitted header fields"
            ))
            .with_extension("max_count", limit)
    }

    /// 431 — the head's header names and values totalled more than `limit`
    /// bytes.
    pub fn headers_too_large(limit: usize) -> Self {
        Error::new(ErrorKind::HeaderFieldsTooLarge)
            .with_detail(format!("The request headers exceed the {limit} byte limit"))
            .with_extension("max_bytes", limit)
    }

    /// 415 — `content_type` is not one this operation accepts.
    pub fn unsupported_media(content_type: impl Into<Cow<'static, str>>) -> Self {
        let content_type = content_type.into();
        let detail = if content_type.is_empty() {
            Cow::Borrowed("A `Content-Type` header is required")
        } else {
            Cow::Owned(format!("This operation does not accept `{content_type}`"))
        };
        Error::new(ErrorKind::UnsupportedMedia).with_detail(detail)
    }

    /// 422 — the request parsed but failed validation.
    ///
    /// The field errors become the `errors` member of the problem document,
    /// each with an RFC 6901 JSON Pointer.
    pub fn validation(errors: ValidationErrors) -> Self {
        let detail = match errors.len() {
            0 => Cow::Borrowed("The request did not pass validation"),
            1 => Cow::Borrowed("1 field failed validation"),
            n => Cow::Owned(format!("{n} fields failed validation")),
        };
        let mut error = Error::new(ErrorKind::Validation).with_detail(detail);
        error.0.fields = Some(errors);
        error
    }

    /// 429, with a `Retry-After` header derived from `retry_after`.
    pub fn too_many(retry_after: Duration) -> Self {
        // `Retry-After` is whole seconds, and rounding down would invite the
        // client back before the window closes.
        let seconds = retry_after.as_secs() + u64::from(retry_after.subsec_nanos() > 0);
        let mut error = Error::new(ErrorKind::TooManyRequests)
            .with_detail(format!("Rate limit exceeded; retry in {seconds}s"))
            .with_extension("retry_after", seconds);
        if let Ok(value) = HeaderValue::from_str(&seconds.to_string()) {
            error = error.with_header(http::header::RETRY_AFTER, value);
        }
        error
    }

    /// 500, wrapping the cause. The cause is logged and never serialised.
    pub fn internal(source: impl Into<BoxError>) -> Self {
        let source = source.into();
        // The detail is set even though a 5xx suppresses it: it is what
        // `http.expose_internal_errors` exposes, and what the dev error page
        // shows. Suppression happens at render time, in exactly one place.
        let detail = source.to_string();
        Error::new(ErrorKind::Internal)
            .with_detail(detail)
            .with_source(source)
    }

    /// 500 from a message rather than a cause, for the "this cannot happen"
    /// paths where there is nothing to wrap.
    pub fn internal_msg(detail: impl Into<Cow<'static, str>>) -> Self {
        Error::new(ErrorKind::Internal).with_detail(detail)
    }

    /// 503 — shutting down, or a hard dependency is unavailable.
    pub fn unavailable(detail: impl Into<Cow<'static, str>>) -> Self {
        Error::new(ErrorKind::Unavailable).with_detail(detail)
    }

    /// 504 — our own timeout layer fired after `after`.
    pub fn timeout(after: Duration) -> Self {
        Error::new(ErrorKind::Timeout).with_detail(format!(
            "The request exceeded the {} timeout",
            humantime::format_duration(after)
        ))
    }

    /// Wrap a boot report. Only [`crate::app::AppBuilder::build`] produces this.
    pub fn boot(errors: BootErrors) -> Self {
        let count = errors.len();
        let noun = if count == 1 { "problem" } else { "problems" };
        Error::new(ErrorKind::Boot(errors))
            .with_detail(format!("The application failed to build ({count} {noun})"))
    }

    // ── builders ──────────────────────────────────────────────────────────

    /// Replace the `type` URI with the application's own.
    pub fn with_type(mut self, uri: &'static str) -> Self {
        self.0.type_uri = Cow::Borrowed(uri);
        self
    }

    /// Replace the `title`. Keep it a description of the *class* of problem;
    /// the specific is what `detail` is for.
    pub fn with_title(mut self, title: impl Into<Cow<'static, str>>) -> Self {
        self.0.title = title.into();
        self
    }

    /// Set the `detail`. Suppressed on 5xx unless `http.expose_internal_errors`.
    pub fn with_detail(mut self, detail: impl Into<Cow<'static, str>>) -> Self {
        self.0.detail = Some(detail.into());
        self
    }

    /// Merge an extra member into the problem document.
    ///
    /// Serialisation failure is swallowed — an error path must not itself fail.
    ///
    /// Extensions are flattened into the top level of the document, so a `key`
    /// that names a structural member is published under its
    /// [`extension_key`](problem::extension_key) instead: `status` becomes
    /// `x_status`. The alternative is a document with two `status` members, one
    /// of which a client will believe. Picking a name outside
    /// [`RESERVED_MEMBERS`](problem::RESERVED_MEMBERS) avoids the rename
    /// entirely.
    ///
    /// ```
    /// use moso::Error;
    ///
    /// let error = Error::conflict("nope")
    ///     .with_extension("order_id", 7)
    ///     .with_extension("status", "ignored");
    ///
    /// assert!(error.extensions().contains_key("order_id"));
    /// assert!(error.extensions().contains_key("x_status"));
    /// assert_eq!(error.status(), 409);
    /// ```
    pub fn with_extension(mut self, key: &'static str, value: impl Serialize) -> Self {
        if let Ok(value) = serde_json::to_value(value) {
            let key = match problem::extension_key(key) {
                Cow::Borrowed(_) => Cow::Borrowed(key),
                Cow::Owned(renamed) => Cow::Owned(renamed),
            };
            self.0.extensions.insert(key, value);
        }
        self
    }

    /// Attach one field-level error, addressed by an RFC 6901 JSON Pointer.
    pub fn with_field(mut self, pointer: &str, code: &'static str, message: &str) -> Self {
        self.0
            .fields
            .get_or_insert_with(ValidationErrors::new)
            .push(FieldError::new(
                pointer.to_owned(),
                code,
                message.to_owned(),
            ));
        self
    }

    /// Attach a whole set of field errors, merging with any already present.
    pub fn with_fields(mut self, errors: ValidationErrors) -> Self {
        match &mut self.0.fields {
            Some(existing) => existing.merge(errors),
            slot @ None => *slot = Some(errors),
        }
        self
    }

    /// Add a response header. Used for `Retry-After`, `Allow`, `WWW-Authenticate`.
    pub fn with_header(mut self, name: HeaderName, value: HeaderValue) -> Self {
        self.0
            .headers
            .get_or_insert_with(|| Box::new(HeaderMap::new()))
            .append(name, value);
        self
    }

    /// Attach the underlying cause. Logged with its full chain, never serialised.
    pub fn with_source(mut self, source: impl Into<BoxError>) -> Self {
        self.0.source = Some(source.into());
        self
    }

    // ── accessors ─────────────────────────────────────────────────────────

    /// The taxonomy entry this error belongs to.
    pub fn kind(&self) -> &ErrorKind {
        &self.0.kind
    }

    /// The HTTP status, from the kind.
    pub fn status(&self) -> StatusCode {
        self.0.kind.status()
    }

    /// The machine-readable `type` URI.
    pub fn type_uri(&self) -> &str {
        &self.0.type_uri
    }

    /// The `title` member.
    pub fn title(&self) -> &str {
        &self.0.title
    }

    /// The `detail` member, *before* the disclosure rule is applied. Use
    /// [`Problem::from_error`] to obtain what the client will actually see.
    pub fn detail(&self) -> Option<&str> {
        self.0.detail.as_deref()
    }

    /// The field-level errors, if any.
    pub fn fields(&self) -> Option<&ValidationErrors> {
        self.0.fields.as_ref()
    }

    /// Extra members merged into the problem document.
    pub fn extensions(&self) -> &BTreeMap<Cow<'static, str>, Value> {
        &self.0.extensions
    }

    /// Headers to add to the error response.
    pub fn headers(&self) -> Option<&HeaderMap> {
        self.0.headers.as_deref()
    }

    /// The backtrace, captured only for [`ErrorKind::Internal`] and only when
    /// `RUST_BACKTRACE` is enabled.
    pub fn backtrace(&self) -> Option<&std::backtrace::Backtrace> {
        self.0.backtrace.as_deref()
    }

    /// `true` for a 4xx.
    pub fn is_client_error(&self) -> bool {
        self.status().is_client_error()
    }

    /// `true` for a 5xx.
    pub fn is_server_error(&self) -> bool {
        self.status().is_server_error()
    }

    /// Whether a client may sensibly retry, from [`ErrorKind::retryable`].
    pub fn retryable(&self) -> bool {
        self.0.kind.retryable()
    }

    /// Render the source chain as `outer: inner: innermost`, for the one log
    /// line the boundary emits.
    ///
    /// Starts at this error's own `Display` and walks `source()` to the root,
    /// so the chain reads as a sentence and the reader never has to correlate
    /// it with the message beside it.
    pub fn chain(&self) -> String {
        use core::error::Error as _;

        let mut chain = self.to_string();
        let mut current = self.source();
        while let Some(error) = current {
            chain.push_str(": ");
            chain.push_str(&error.to_string());
            current = error.source();
        }
        chain
    }

    // ── conversions used by extractors ────────────────────────────────────

    /// A 400 from a `serde_path_to_error` failure, carrying the JSON Pointer of
    /// the offending member.
    ///
    /// This is why `serde_path_to_error` is mandatory rather than optional:
    /// `invalid type: string, expected u32` without `/items/2/quantity` is not
    /// an actionable error message.
    ///
    /// A failure a Moso constrained type raised — `Email`, `Slug`, `Password` —
    /// is recognised by its marker prefix and becomes a **422** with the
    /// constraint's own code, not a 400: violating a documented invariant is
    /// validation, and the fact that it was caught during deserialisation
    /// rather than after is an implementation detail the client must not see.
    pub fn from_json_path(error: serde_path_to_error::Error<serde_json::Error>) -> Self {
        let pointer = json_pointer_from_path(error.path());
        let inner = error.into_inner();
        let is_io = matches!(inner.classify(), serde_json::error::Category::Io);
        Error::from_deserialise(pointer, &inner.to_string(), is_io).with_source(inner)
    }

    /// A 400 from a form-decoding failure, with the field name as a pointer
    /// where `serde_urlencoded` gives us one.
    pub fn from_form_path(error: serde_path_to_error::Error<serde::de::value::Error>) -> Self {
        let pointer = json_pointer_from_path(error.path());
        let inner = error.into_inner();
        Error::from_deserialise(pointer, &inner.to_string(), false).with_source(inner)
    }

    /// The shared body of every deserialisation conversion.
    ///
    /// `pointer` is `""` when the caller had no path to work from, which is the
    /// RFC 6901 pointer to the whole document — an honest answer rather than a
    /// fabricated one.
    fn from_deserialise(pointer: String, message: &str, is_io: bool) -> Self {
        if is_io {
            // Reading the socket failed. Not the client's fault, not the
            // client's business.
            return Error::new(ErrorKind::Internal)
                .with_detail(format!("Failed to read the request body: {message}"));
        }

        let message = strip_line_column(message);

        if let Some((code, detail)) = moso_schema::parse_serde_message(message) {
            let field = FieldError::new(pointer, static_code(code), detail.to_owned());
            return Error::validation(ValidationErrors::from(field));
        }

        // `serde_path_to_error` stops at the *container*, because the member it
        // wanted was never visited — so the path for `missing field `email`` is
        // the object, not the field. The name is right there in the message,
        // and a pointer at the object is a pointer the reader has to decode by
        // hand.
        let (code, pointer) = match missing_field_name(message) {
            Some(name) => (moso_schema::codes::REQUIRED, push_token(&pointer, name)),
            None if message.starts_with("missing field") => (moso_schema::codes::REQUIRED, pointer),
            None => (moso_schema::codes::TYPE, pointer),
        };
        Error::bad_request(message.to_owned()).with_fields(ValidationErrors::from(FieldError::new(
            pointer,
            code,
            message.to_owned(),
        )))
    }
}

impl fmt::Debug for Error {
    /// Redacts nothing: `Debug` on an `Error` is an operator-facing view and is
    /// only ever reached through the logging boundary, which redacts headers
    /// and secret fields itself.
    ///
    /// [`ErrorKind::Boot`] is special-cased to the grouped report, because
    /// `fn main() -> Result<(), Error>` prints its error with `Debug` and a
    /// derived dump of the report's internals would be unreadable.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let ErrorKind::Boot(errors) = &self.0.kind {
            return f.write_str(&errors.render(boot::stderr_is_tty()));
        }

        let mut debug = f.debug_struct("Error");
        debug
            .field("kind", &self.0.kind)
            .field("status", &self.status().as_u16())
            .field("title", &self.0.title)
            .field("detail", &self.0.detail)
            .field(
                "fields",
                &self.0.fields.as_ref().map_or(0, ValidationErrors::len),
            );
        if self.0.source.is_some() {
            debug.field("chain", &self.chain());
        }
        debug.finish_non_exhaustive()
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let ErrorKind::Boot(errors) = &self.0.kind {
            return fmt::Display::fmt(errors, f);
        }
        f.write_str(&self.0.title)?;
        if let Some(detail) = &self.0.detail {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        self.0
            .source
            .as_ref()
            .map(|source| &**source as &(dyn core::error::Error + 'static))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Upper-case the first character, leaving the rest alone.
///
/// `char::to_uppercase` rather than `to_ascii_uppercase`, so `Error::not_found`
/// works on a non-ASCII resource name.
fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Drop the ` at line N column M` suffix `serde_json` appends.
///
/// The position is useful in a log and noise in a field-level message, which
/// already carries a JSON Pointer to the exact member.
fn strip_line_column(message: &str) -> &str {
    match message.rfind(" at line ") {
        Some(index) if message[index..].contains(" column ") => &message[..index],
        _ => message,
    }
}

/// Borrow one of the built-in codes when `code` is one, otherwise own it.
///
/// [`FieldError::code`] is a `Cow<'static, str>`; a code recovered from a
/// `serde` error message is a borrowed `&str` with no useful lifetime, so the
/// common cases are looked up in the closed set to avoid an allocation and the
/// `custom:` codes allocate.
fn static_code(code: &str) -> Cow<'static, str> {
    for known in moso_schema::codes::ALL {
        if *known == code {
            return Cow::Borrowed(*known);
        }
    }
    Cow::Owned(code.to_owned())
}

/// Turn a `serde_path_to_error` path into an RFC 6901 JSON Pointer.
///
/// `items[2].quantity` becomes `/items/2/quantity`. Map keys are escaped per
/// RFC 6901 (`~` → `~0`, `/` → `~1`), which matters for the free-form keys a
/// `HashMap<String, _>` field admits.
fn json_pointer_from_path(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;

    let mut pointer = String::new();
    for segment in path.iter() {
        match segment {
            Segment::Seq { index } => {
                pointer.push('/');
                pointer.push_str(itoa(*index).as_str());
            }
            Segment::Map { key } => moso_schema::push_token(&mut pointer, key),
            Segment::Enum { variant } => moso_schema::push_token(&mut pointer, variant),
            Segment::Unknown => pointer.push_str("/?"),
        }
    }
    pointer
}

/// `usize` to `String` without pulling in a formatting dependency.
fn itoa(value: usize) -> String {
    value.to_string()
}

/// The field name out of serde's ``missing field `email``` message.
///
/// The wording is `serde`'s and has been stable for the life of the 1.0 series;
/// if it ever changes, [`Error::from_deserialise`] falls back to the pointer it
/// already had rather than inventing one.
fn missing_field_name(message: &str) -> Option<&str> {
    let rest = message.strip_prefix("missing field `")?;
    let (name, _) = rest.split_once('`')?;
    (!name.is_empty()).then_some(name)
}

/// Append one RFC 6901 token to a pointer, escaping it.
fn push_token(pointer: &str, token: &str) -> String {
    let mut out = pointer.to_owned();
    moso_schema::push_token(&mut out, token);
    out
}

// ---------------------------------------------------------------------------
// From impls — the `?` path
// ---------------------------------------------------------------------------

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Error::new(kind)
    }
}

impl From<ValidationErrors> for Error {
    fn from(errors: ValidationErrors) -> Self {
        Error::validation(errors)
    }
}

impl From<BootErrors> for Error {
    fn from(errors: BootErrors) -> Self {
        Error::boot(errors)
    }
}

impl From<serde_json::Error> for Error {
    /// 400 without a pointer. Prefer [`Error::from_json_path`], which has one.
    fn from(error: serde_json::Error) -> Self {
        let is_io = matches!(error.classify(), serde_json::error::Category::Io);
        let message = error.to_string();
        Error::from_deserialise(String::new(), &message, is_io).with_source(error)
    }
}

impl From<serde_path_to_error::Error<serde_json::Error>> for Error {
    /// 400 with the JSON Pointer of the offending member.
    fn from(error: serde_path_to_error::Error<serde_json::Error>) -> Self {
        Error::from_json_path(error)
    }
}

impl From<serde_path_to_error::Error<serde::de::value::Error>> for Error {
    /// 400 with the field name of the offending member.
    fn from(error: serde_path_to_error::Error<serde::de::value::Error>) -> Self {
        Error::from_form_path(error)
    }
}

impl From<std::io::Error> for Error {
    /// 500. I/O failures are operational, never client-caused.
    fn from(error: std::io::Error) -> Self {
        Error::internal(error)
    }
}

impl From<http::Error> for Error {
    /// 500 — a malformed header or status *we* tried to build.
    fn from(error: http::Error) -> Self {
        Error::internal(error)
    }
}

impl From<axum::Error> for Error {
    /// 400 — Axum's `Error` is a body-stream failure, which is the client
    /// hanging up or sending a malformed chunked encoding.
    fn from(error: axum::Error) -> Self {
        Error::bad_request("Failed to read the request body").with_source(error)
    }
}

impl From<moso_schema::ConstraintError> for Error {
    /// 422 with the constraint's own code, at the root pointer.
    ///
    /// A [`ConstraintError`](moso_schema::ConstraintError) is raised by a
    /// constructor, which does not know where the value will live; `""` is the
    /// RFC 6901 pointer to the whole document. An extractor that *does* know
    /// the position should call
    /// [`into_validation_errors`](moso_schema::ConstraintError::into_validation_errors)
    /// with it instead.
    fn from(error: moso_schema::ConstraintError) -> Self {
        Error::validation(error.into_validation_errors(""))
    }
}

impl From<std::str::Utf8Error> for Error {
    /// 400 — the bytes the client sent are not text.
    fn from(error: std::str::Utf8Error) -> Self {
        Error::bad_request("The request body is not valid UTF-8").with_source(error)
    }
}

impl From<std::string::FromUtf8Error> for Error {
    /// 400 — the owned form of [`Utf8Error`](std::str::Utf8Error).
    fn from(error: std::string::FromUtf8Error) -> Self {
        Error::bad_request("The request body is not valid UTF-8").with_source(error)
    }
}

impl From<std::num::ParseIntError> for Error {
    /// 400 — a path or query segment that should have been an integer.
    fn from(error: std::num::ParseIntError) -> Self {
        Error::bad_request(format!("Expected an integer: {error}")).with_source(error)
    }
}

impl From<std::num::ParseFloatError> for Error {
    /// 400 — a path or query segment that should have been a number.
    fn from(error: std::num::ParseFloatError) -> Self {
        Error::bad_request(format!("Expected a number: {error}")).with_source(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_defaults_to_the_framework_error() {
        fn probe() -> Result<()> {
            Ok(())
        }
        assert!(probe().is_ok());
    }

    #[test]
    fn error_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<Error>();
    }

    #[test]
    fn the_error_is_one_pointer_wide_and_its_payload_is_why() {
        // `Result<T, Error>` is what every handler, extractor and dependency
        // returns, so the error's width is paid on the *success* path of the
        // whole program. This is the assertion behind "Why the payload is
        // boxed": clippy's `result_large_err` fires above 128 bytes, and the
        // payload is what would sit inline if the box went away.
        assert_eq!(size_of::<Error>(), size_of::<*const ()>());
        assert!(
            size_of::<ErrorInner>() > 128,
            "the payload no longer justifies the box: {} bytes",
            size_of::<ErrorInner>()
        );
    }

    // ── the taxonomy ─────────────────────────────────────────────────────

    /// The snapshot the acceptance criteria name: every kind, its status and
    /// its stable `type` URI. Changing a row here is an API break.
    #[test]
    fn every_kind_maps_to_its_documented_status_and_type() {
        let expected: &[(ErrorKind, u16, &str)] = &[
            (ErrorKind::BadRequest, 400, "bad-request"),
            (ErrorKind::Unauthenticated, 401, "unauthenticated"),
            (ErrorKind::Forbidden, 403, "forbidden"),
            (ErrorKind::NotFound, 404, "not-found"),
            (ErrorKind::MethodNotAllowed, 405, "method-not-allowed"),
            (ErrorKind::NotAcceptable, 406, "not-acceptable"),
            (ErrorKind::Conflict, 409, "conflict"),
            (ErrorKind::Gone, 410, "gone"),
            (ErrorKind::PreconditionFailed, 412, "precondition-failed"),
            (ErrorKind::PayloadTooLarge, 413, "payload-too-large"),
            (ErrorKind::UriTooLong, 414, "uri-too-long"),
            (ErrorKind::UnsupportedMedia, 415, "unsupported-media"),
            (ErrorKind::RangeNotSatisfiable, 416, "range-not-satisfiable"),
            (ErrorKind::Validation, 422, "validation"),
            (ErrorKind::Locked, 423, "locked"),
            (ErrorKind::TooManyRequests, 429, "too-many-requests"),
            (
                ErrorKind::HeaderFieldsTooLarge,
                431,
                "header-fields-too-large",
            ),
            (ErrorKind::Internal, 500, "internal"),
            (ErrorKind::NotImplemented, 501, "not-implemented"),
            (ErrorKind::BadGateway, 502, "bad-gateway"),
            (ErrorKind::Unavailable, 503, "unavailable"),
            (ErrorKind::GatewayTimeout, 504, "gateway-timeout"),
            (ErrorKind::Timeout, 504, "timeout"),
            (ErrorKind::Boot(BootErrors::new()), 500, "boot"),
        ];

        for (kind, status, slug) in expected {
            assert_eq!(kind.status().as_u16(), *status, "{kind}");
            assert_eq!(kind.slug(), *slug, "{kind}");
            assert_eq!(
                kind.type_uri(),
                format!("{ERROR_TYPE_BASE}{slug}"),
                "{kind}"
            );
        }
    }

    #[test]
    fn response_kinds_covers_every_non_boot_variant() {
        // 24 rows in the snapshot above, one of which is Boot.
        assert_eq!(ErrorKind::RESPONSE_KINDS.len(), 23);
        assert!(
            ErrorKind::RESPONSE_KINDS
                .iter()
                .all(|kind| !matches!(kind, ErrorKind::Boot(_)))
        );
    }

    #[test]
    fn every_type_uri_is_unique() {
        let mut seen: Vec<&str> = ErrorKind::RESPONSE_KINDS
            .iter()
            .map(ErrorKind::type_uri)
            .collect();
        let total = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), total);
    }

    #[test]
    fn client_safety_follows_the_status_class() {
        for kind in ErrorKind::RESPONSE_KINDS {
            assert_eq!(
                kind.detail_is_client_safe(),
                !kind.status().is_server_error(),
                "{kind}"
            );
        }
    }

    #[test]
    fn only_the_documented_kinds_are_retryable() {
        let retryable: Vec<&'static str> = ErrorKind::RESPONSE_KINDS
            .iter()
            .filter(|kind| kind.retryable())
            .map(ErrorKind::slug)
            .collect();
        assert_eq!(
            retryable,
            [
                "too-many-requests",
                "bad-gateway",
                "unavailable",
                "gateway-timeout",
                "timeout"
            ]
        );
    }

    #[test]
    fn log_levels_follow_the_documented_split() {
        assert_eq!(ErrorKind::Internal.log_level(), tracing::Level::ERROR);
        assert_eq!(ErrorKind::Unavailable.log_level(), tracing::Level::ERROR);
        assert_eq!(ErrorKind::Unauthenticated.log_level(), tracing::Level::WARN);
        assert_eq!(ErrorKind::Forbidden.log_level(), tracing::Level::WARN);
        assert_eq!(ErrorKind::Conflict.log_level(), tracing::Level::WARN);
        assert_eq!(ErrorKind::TooManyRequests.log_level(), tracing::Level::WARN);
        assert_eq!(ErrorKind::NotFound.log_level(), tracing::Level::DEBUG);
        assert_eq!(ErrorKind::Validation.log_level(), tracing::Level::DEBUG);
    }

    // ── constructors ─────────────────────────────────────────────────────

    #[test]
    fn not_found_capitalises_the_resource() {
        assert_eq!(Error::not_found("user").detail(), Some("User not found"));
        assert_eq!(
            Error::not_found("order line").detail(),
            Some("Order line not found")
        );
        assert_eq!(Error::not_found("").detail(), Some("Not found"));
    }

    #[test]
    fn method_not_allowed_sets_the_allow_header() {
        let error = Error::method_not_allowed(&[http::Method::GET, http::Method::POST]);
        assert_eq!(error.status(), StatusCode::METHOD_NOT_ALLOWED);
        let allow = error
            .headers()
            .and_then(|headers| headers.get(http::header::ALLOW))
            .expect("Allow header");
        assert_eq!(allow, "GET, POST");
    }

    #[test]
    fn too_many_rounds_the_retry_after_up() {
        let error = Error::too_many(Duration::from_millis(1500));
        let retry = error
            .headers()
            .and_then(|headers| headers.get(http::header::RETRY_AFTER))
            .expect("Retry-After header");
        assert_eq!(retry, "2");
        assert_eq!(error.extensions()["retry_after"], serde_json::json!(2));
    }

    #[test]
    fn validation_carries_its_field_errors() {
        let mut errors = ValidationErrors::new();
        errors.add("/email", "unique", "already taken");
        errors.add("/name", "len", "too short");
        let error = Error::validation(errors);
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.detail(), Some("2 fields failed validation"));
        assert_eq!(error.fields().map(ValidationErrors::len), Some(2));
    }

    #[test]
    fn internal_keeps_the_cause_as_a_source() {
        let error = Error::internal(std::io::Error::other("connection refused"));
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(error.detail(), Some("connection refused"));
        assert!(core::error::Error::source(&error).is_some());
        assert!(error.chain().contains("connection refused"));
    }

    #[test]
    fn timeout_renders_a_human_duration() {
        assert_eq!(
            Error::timeout(Duration::from_secs(30)).detail(),
            Some("The request exceeded the 30s timeout")
        );
    }

    #[test]
    fn payload_too_large_reports_the_limit() {
        let error = Error::payload_too_large(1024);
        assert_eq!(error.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(error.extensions()["max_bytes"], serde_json::json!(1024));
    }

    #[test]
    fn the_head_limits_report_the_limit_that_stopped_them() {
        let error = Error::uri_too_long(8192);
        assert_eq!(error.status(), StatusCode::URI_TOO_LONG);
        assert_eq!(error.extensions()["max_bytes"], serde_json::json!(8192));

        let error = Error::too_many_headers(100);
        assert_eq!(
            error.status(),
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "a header flood is a 431"
        );
        assert_eq!(error.extensions()["max_count"], serde_json::json!(100));

        let error = Error::headers_too_large(16 * 1024);
        assert_eq!(error.status(), StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE);
        assert_eq!(error.extensions()["max_bytes"], serde_json::json!(16384));

        // Both 431s carry the same `type`, because a client that handles one
        // handles the other; the extension says which limit fired.
        assert_eq!(
            Error::too_many_headers(1).type_uri(),
            Error::headers_too_large(1).type_uri()
        );
    }

    // ── builders ─────────────────────────────────────────────────────────

    #[test]
    fn builders_override_the_defaults() {
        let error = Error::conflict("email taken")
            .with_type("https://shop.example/errors/duplicate-email")
            .with_title("Duplicate Email")
            .with_field("/email", "unique", "already taken")
            .with_extension("attempted", "a@b.test");

        assert_eq!(
            error.type_uri(),
            "https://shop.example/errors/duplicate-email"
        );
        assert_eq!(error.title(), "Duplicate Email");
        assert_eq!(error.fields().map(ValidationErrors::len), Some(1));
        assert_eq!(
            error.extensions()["attempted"],
            serde_json::json!("a@b.test")
        );
    }

    #[test]
    fn with_fields_merges_rather_than_replaces() {
        let mut more = ValidationErrors::new();
        more.add("/name", "len", "too short");
        let error =
            Error::validation(ValidationErrors::one("/email", "unique", "taken")).with_fields(more);
        assert_eq!(error.fields().map(ValidationErrors::len), Some(2));
    }

    #[test]
    fn an_unserialisable_extension_is_dropped_rather_than_fatal() {
        struct Bad;
        impl Serialize for Bad {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> core::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("nope"))
            }
        }
        let error = Error::bad_request("x").with_extension("bad", Bad);
        assert!(error.extensions().is_empty());
    }

    #[test]
    fn multiple_headers_of_one_name_are_kept() {
        let error = Error::unauthenticated()
            .with_header(
                http::header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Bearer"),
            )
            .with_header(
                http::header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic"),
            );
        let count = error
            .headers()
            .expect("headers")
            .get_all(http::header::WWW_AUTHENTICATE)
            .iter()
            .count();
        assert_eq!(count, 2);
    }

    // ── formatting ───────────────────────────────────────────────────────

    #[test]
    fn display_is_title_then_detail() {
        assert_eq!(Error::new(ErrorKind::Conflict).to_string(), "Conflict");
        assert_eq!(
            Error::conflict("email taken").to_string(),
            "Conflict: email taken"
        );
    }

    #[test]
    fn the_chain_reads_outermost_first() {
        let inner = std::io::Error::other("tcp connect error");
        let error = Error::unavailable("database is down").with_source(inner);
        assert_eq!(
            error.chain(),
            "Service Unavailable: database is down: tcp connect error"
        );
    }

    #[test]
    fn a_boot_error_debugs_as_the_report() {
        let mut errors = BootErrors::new();
        errors.push(BootError::Other {
            message: "something".to_owned(),
            notes: Vec::new(),
            fix: None,
        });
        let error = Error::boot(errors);
        let debug = format!("{error:?}");
        assert!(
            debug.contains("application failed to build (1 problem)"),
            "{debug}"
        );
        assert!(debug.contains("something"), "{debug}");
    }

    // ── the `?` path ─────────────────────────────────────────────────────

    #[test]
    fn a_json_syntax_error_is_a_400() {
        let error: Error = serde_json::from_str::<serde_json::Value>("{oops")
            .unwrap_err()
            .into();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn a_json_path_error_carries_a_pointer() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(
            dead_code,
            reason = "the fields exist to give `serde_path_to_error` a path to report; \
                      nothing reads them"
        )]
        struct Line {
            quantity: u32,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code, reason = "as above")]
        struct Order {
            items: Vec<Line>,
        }

        let json = br#"{"items":[{"quantity":1},{"quantity":2},{"quantity":"three"}]}"#;
        let deserializer = &mut serde_json::Deserializer::from_slice(json);
        let path_error = serde_path_to_error::deserialize::<_, Order>(deserializer).unwrap_err();

        let error = Error::from_json_path(path_error);
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
        let fields = error.fields().expect("field errors");
        assert_eq!(fields.as_slice()[0].pointer, "/items/2/quantity");
        assert_eq!(fields.as_slice()[0].code, "type");
        // The `at line N column M` suffix is noise beside a JSON Pointer.
        assert!(!fields.as_slice()[0].message.contains("at line"));
    }

    #[test]
    fn a_missing_field_is_coded_required() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(
            dead_code,
            reason = "the fields exist to give `serde_path_to_error` a path to report; \
                      nothing reads them"
        )]
        struct Body {
            email: String,
        }
        let deserializer = &mut serde_json::Deserializer::from_slice(b"{}");
        let path_error = serde_path_to_error::deserialize::<_, Body>(deserializer).unwrap_err();
        let error = Error::from_json_path(path_error);
        assert_eq!(
            error.fields().expect("fields").as_slice()[0].code,
            "required"
        );
    }

    #[test]
    fn a_missing_field_points_at_the_field_and_not_at_its_container() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(
            dead_code,
            reason = "the fields exist to give `serde_path_to_error` a path to report; \
                      nothing reads them"
        )]
        struct Address {
            postcode: String,
        }
        #[derive(Debug, serde::Deserialize)]
        #[allow(dead_code, reason = "as above")]
        struct Body {
            address: Address,
        }

        // `serde_path_to_error` stops at `/address`, because `postcode` was
        // never visited. `/address` alone tells the reader an object is wrong
        // without saying which member.
        let deserializer = &mut serde_json::Deserializer::from_slice(br#"{"address":{}}"#);
        let path_error = serde_path_to_error::deserialize::<_, Body>(deserializer).unwrap_err();
        let error = Error::from_json_path(path_error);
        let field = &error.fields().expect("fields").as_slice()[0];
        assert_eq!(field.pointer, "/address/postcode");
        assert_eq!(field.code, "required");
    }

    #[test]
    fn a_missing_field_name_is_escaped_like_any_other_token() {
        let error = Error::from_deserialise(
            "/config".to_owned(),
            "missing field `a/b`",
            /* is_io = */ false,
        );
        assert_eq!(
            error.fields().expect("fields").as_slice()[0].pointer,
            "/config/a~1b"
        );
    }

    #[test]
    fn a_message_that_is_not_serdes_wording_keeps_the_path_it_had() {
        // The fallback matters: it is what keeps this honest if serde ever
        // rewords the message.
        let error = Error::from_deserialise("/a".to_owned(), "missing field", false);
        let field = &error.fields().expect("fields").as_slice()[0];
        assert_eq!(field.pointer, "/a");
        assert_eq!(field.code, "required");
    }

    #[test]
    fn a_constrained_type_failure_is_a_422_with_its_own_code() {
        // What `Email::deserialize` raises: a marker-prefixed serde message.
        let constraint =
            moso_schema::ConstraintError::format("email", "must be a valid email address");
        let message = constraint.to_serde_message();

        let error = Error::from_deserialise("/email".to_owned(), &message, false);
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let field = &error.fields().expect("fields").as_slice()[0];
        assert_eq!(field.pointer, "/email");
        assert_eq!(field.code, "format");
        assert_eq!(field.message, "must be a valid email address");
    }

    #[test]
    fn a_json_io_error_is_a_500() {
        let error = Error::from_deserialise(String::new(), "unexpected end of file", true);
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn pointers_escape_rfc_6901_reserved_characters() {
        #[derive(Debug, serde::Deserialize)]
        #[allow(
            dead_code,
            reason = "the fields exist to give `serde_path_to_error` a path to report; \
                      nothing reads them"
        )]
        struct Body {
            map: std::collections::BTreeMap<String, u32>,
        }
        let json = br#"{"map":{"a/b":"x"}}"#;
        let deserializer = &mut serde_json::Deserializer::from_slice(json);
        let path_error = serde_path_to_error::deserialize::<_, Body>(deserializer).unwrap_err();
        let error = Error::from_json_path(path_error);
        assert_eq!(
            error.fields().expect("fields").as_slice()[0].pointer,
            "/map/a~1b"
        );
    }

    #[test]
    fn io_errors_are_internal() {
        let error: Error = std::io::Error::other("disk on fire").into();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!error.is_client_error());
        assert!(error.is_server_error());
    }

    #[test]
    fn axum_body_errors_are_client_errors() {
        let error: Error = axum::Error::new(std::io::Error::other("early eof")).into();
        assert_eq!(error.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn validation_errors_convert_to_422() {
        let error: Error = ValidationErrors::one("/email", "format", "bad").into();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn constraint_errors_convert_to_422_at_the_root() {
        let error: Error = moso_schema::ConstraintError::format("slug", "must be a slug").into();
        assert_eq!(error.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.fields().expect("fields").as_slice()[0].pointer, "");
    }

    #[test]
    fn parse_failures_are_400() {
        let int: Error = "x".parse::<u32>().unwrap_err().into();
        assert_eq!(int.status(), StatusCode::BAD_REQUEST);
        let float: Error = "x".parse::<f64>().unwrap_err().into();
        assert_eq!(float.status(), StatusCode::BAD_REQUEST);
        // Built at runtime: a literal would be diagnosed by `invalid_from_utf8`
        // rather than reaching the conversion under test.
        let invalid: Vec<u8> = vec![0x66, 0xff, 0x6f];
        let utf8: Error = core::str::from_utf8(&invalid).unwrap_err().into();
        assert_eq!(utf8.status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn http_errors_are_internal() {
        let bad = http::Response::builder().status(99).body(()).unwrap_err();
        let error: Error = bad.into();
        assert_eq!(error.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ── helpers ──────────────────────────────────────────────────────────

    #[test]
    fn strip_line_column_only_strips_the_suffix() {
        assert_eq!(
            strip_line_column("invalid type: string at line 3 column 5"),
            "invalid type: string"
        );
        assert_eq!(strip_line_column("plain message"), "plain message");
        assert_eq!(
            strip_line_column("mentions at line but not the other word"),
            "mentions at line but not the other word"
        );
    }

    #[test]
    fn static_code_borrows_the_closed_set() {
        assert!(matches!(static_code("required"), Cow::Borrowed("required")));
        assert!(matches!(static_code("custom:x"), Cow::Owned(_)));
    }

    #[test]
    fn capitalise_handles_non_ascii_and_empty() {
        assert_eq!(capitalise("étage"), "Étage");
        assert_eq!(capitalise(""), "");
        assert_eq!(capitalise("a"), "A");
    }
}
