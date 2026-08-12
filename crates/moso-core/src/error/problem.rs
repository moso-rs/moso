//! RFC 9457 `application/problem+json`, and the HTML page a browser gets.
//!
//! # How an [`Error`] becomes bytes
//!
//! ```text
//! Error ──IntoResponse──▶ Response          (conservative: no 5xx detail, no ids)
//!   │                        │
//!   └── Arc<Error> stored in `Response::extensions`
//!                            │
//!                    catch_error layer
//!                            │  has the request id, the path, the trace id,
//!                            │  the profile and `http.expose_internal_errors`
//!                            ▼
//!                        Response          (final: enriched, logged exactly once)
//! ```
//!
//! Rendering twice on the error path is deliberate. `IntoResponse` has no
//! access to the request context — macro-generated handler glue calls it
//! directly on an extraction failure — so it must be correct in isolation.
//! Making it *conservative* in isolation and *complete* in the layer is the
//! only arrangement where a misconfigured stack cannot leak a 5xx detail.
//!
//! An [`ErrorContext`] installed with [`with_error_context`] closes the gap for
//! the common case: the layer that owns the request scopes one around the
//! inner service, and every `IntoResponse` inside that scope renders with the
//! request id, the trace id and the configured disclosure policy. When nothing
//! installed one, the conservative rendering above is what happens — an
//! absent context can only ever disclose *less*.
//!
//! # Content negotiation on the error path
//!
//! A request whose `Accept` prefers `text/html` gets [`html_page`] instead: a
//! wall of JSON in a browser is a bad first impression, and in the `dev`
//! profile the page carries the error chain, the backtrace and the problem
//! document itself. In `production` it is four lines and a request id.
//!
//! What it does **not** carry, so that nobody plans around it: the matched
//! route and a `file:line` link to the handler, the dependencies the request
//! resolved, and the statements it issued. [`ErrorContext`] is everything the
//! renderer knows, and it knows the path, the two ids and the disclosure
//! policy — nothing that would let it name a route or a query. Adding any of
//! them means widening that context first, not reaching for ambient state from
//! inside the renderer.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use http::header::CONTENT_TYPE;
use http::{HeaderMap, HeaderValue, StatusCode};
use moso_schema::ValidationErrors;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::Profile;
use crate::error::Error;
use crate::response::IntoResponse;
use crate::{Request, Response};

/// The media type every Moso error is served as.
pub const PROBLEM_CONTENT_TYPE: &str = "application/problem+json";

/// The media type the HTML fallback is served as.
pub const HTML_CONTENT_TYPE: &str = "text/html; charset=utf-8";

/// The problem member carrying the source chain, present only when
/// `http.expose_internal_errors` is set and the status is a 5xx.
///
/// Written by [`Problem::from_error`] *after* the application's own extensions,
/// so an extension of this name is replaced rather than merged. That is the one
/// name in the document Moso will overwrite; it is not in
/// [`RESERVED_MEMBERS`] because it is itself an extension and therefore cannot
/// collide with a structural member.
pub const CHAIN_MEMBER: &str = "chain";

/// The member names [`Problem`] serialises from its own fields.
///
/// Extension members are flattened into the top level of the document, so an
/// extension sharing one of these names would emit the member twice. Neither
/// outcome is acceptable: a `serde_json::Value` keeps the last write and a
/// streaming parser keeps whichever it likes, so a `status` extension can
/// silently turn a 422 into whatever the application happened to attach. This
/// list is what [`extension_key`] checks against, and it is the one home for
/// the fact — the `serde` attributes on [`Problem`] and this constant must be
/// read together, which is why the field docs point here.
pub const RESERVED_MEMBERS: &[&str] = &[
    "type",
    "title",
    "status",
    "detail",
    "instance",
    "errors",
    "request_id",
    "trace_id",
];

/// Whether `name` is a member [`Problem`] serialises itself.
///
/// ```
/// use moso::error::problem::is_reserved_member;
///
/// assert!(is_reserved_member("status"));
/// assert!(!is_reserved_member("order_id"));
/// ```
#[must_use]
pub fn is_reserved_member(name: &str) -> bool {
    RESERVED_MEMBERS.contains(&name)
}

/// The name an extension is actually published under.
///
/// A name that would collide with a structural member is prefixed with `x_`;
/// everything else is returned untouched, and untouched is the overwhelmingly
/// common case, so the borrow is the fast path.
///
/// Renaming rather than dropping is deliberate. Dropping loses whatever the
/// application was trying to say and does it invisibly; renaming keeps the
/// value, keeps the document valid, and makes the collision obvious the first
/// time anybody looks at the response — which is the difference between a bug
/// you find and a bug you ship.
///
/// ```
/// use moso::error::problem::extension_key;
///
/// assert_eq!(extension_key("order_id"), "order_id");
/// assert_eq!(extension_key("status"), "x_status");
/// ```
#[must_use]
pub fn extension_key(key: &str) -> Cow<'_, str> {
    if is_reserved_member(key) {
        Cow::Owned(format!("x_{key}"))
    } else {
        Cow::Borrowed(key)
    }
}

/// The RFC 9457 problem document, as it goes on the wire.
///
/// `type`, `title` and `status` are always present. `detail` is present when it
/// is safe to disclose. `errors`, `request_id` and `trace_id` are Moso
/// extensions; the first is what makes a 422 actionable and the last two are
/// what make a 500 diagnosable.
///
/// ```
/// use moso::prelude::*;
/// use moso::error::problem::Problem;
///
/// let error = Error::not_found("post");
/// let problem = Problem::from_error(&error, &Default::default());
///
/// assert_eq!(problem.status, 404);
/// assert!(problem.type_uri.starts_with("https://"));
///
/// // It round-trips, which is what lets a test — or a client — parse one back.
/// let json = serde_json::to_string(&problem).unwrap();
/// let parsed: Problem = serde_json::from_str(&json).unwrap();
/// assert_eq!(parsed, problem);
/// ```
///
/// Served as `application/problem+json`. The detail of a 5xx is suppressed unless
/// `http.expose_internal_errors` is set, so an internal message cannot reach a
/// client by accident.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Problem {
    /// The `type` URI. Dereferenceable documentation for the error class.
    #[serde(rename = "type")]
    pub type_uri: String,
    /// A short, human-readable summary of the *class* of problem.
    pub title: String,
    /// The HTTP status, repeated in the body as RFC 9457 requires.
    pub status: u16,
    /// What went wrong with *this* request. Absent when disclosure is refused.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The request path, identifying the specific occurrence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instance: Option<String>,
    /// Field-level errors, each with an RFC 6901 JSON Pointer.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<Vec<ProblemField>>,
    /// The correlation id, always present on a served response.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    /// The W3C trace id, present when a tracing context was propagated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trace_id: Option<String>,
    /// Application-defined extra members, merged at the top level.
    ///
    /// A name in [`RESERVED_MEMBERS`] would be emitted twice, so [`to_bytes`]
    /// republishes it through [`extension_key`] — `status` goes out as
    /// `x_status`. Inserting here directly is therefore safe; it is simply not
    /// a way to override a structural member.
    ///
    /// [`to_bytes`]: Problem::to_bytes
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// One field-level error, as it appears in the `errors` member.
///
/// The wire twin of [`moso_schema::FieldError`], which is `Serialize` only.
/// A problem document has to round-trip — `moso-test` asserts against a parsed
/// response, and `moso openapi` reads committed fixtures — so the wire type
/// owns its data and derives both halves.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProblemField {
    /// RFC 6901 JSON Pointer into the request: `/address/postcode`,
    /// `/query/limit`.
    pub pointer: String,
    /// A stable machine-readable code from `moso_schema::codes`.
    pub code: String,
    /// A human-readable message. Localisable, and never part of the contract —
    /// clients match on `code`.
    pub message: String,
    /// The constraint's parameters, so a client can render its own message.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, Value>,
}

impl ProblemField {
    /// Convert from the validation error moso-schema produces.
    pub fn from_field_error(error: &moso_schema::FieldError) -> Self {
        Self {
            pointer: error.pointer.clone(),
            code: error.code.clone().into_owned(),
            message: error.message.clone().into_owned(),
            params: error
                .params
                .iter()
                .map(|(key, value)| ((*key).to_owned(), value.clone()))
                .collect(),
        }
    }

    /// Convert a whole set.
    pub fn from_validation_errors(errors: &ValidationErrors) -> Vec<Self> {
        errors.iter().map(ProblemField::from_field_error).collect()
    }
}

impl Problem {
    /// A problem carrying only the members RFC 9457 requires.
    pub fn new(status: StatusCode, type_uri: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            type_uri: type_uri.into(),
            title: title.into(),
            status: status.as_u16(),
            detail: None,
            instance: None,
            errors: None,
            request_id: None,
            trace_id: None,
            extensions: BTreeMap::new(),
        }
    }

    /// Render `error` under `options`.
    ///
    /// This is the *only* place the disclosure rule is applied: a 5xx `detail`
    /// survives exactly when [`ProblemOptions::expose_internal_errors`] is set.
    ///
    /// What suppression covers, when the status is a 5xx and the flag is off:
    /// `detail`, the field errors, and the source chain. What it deliberately
    /// does *not* cover: the [`extensions`](Error::extensions), which are values
    /// the application chose to publish by calling
    /// [`with_extension`](Error::with_extension) — suppressing those would make
    /// the builder silently useless on the one class of error people most want
    /// to annotate.
    pub fn from_error(error: &Error, options: &ProblemOptions) -> Self {
        let disclose = error.kind().detail_is_client_safe() || options.expose_internal_errors;

        let mut problem = Problem::new(error.status(), error.type_uri(), error.title());

        if disclose {
            problem.detail = error.detail().map(str::to_owned);
            if let Some(fields) = error.fields().filter(|fields| !fields.is_empty()) {
                problem.errors = Some(ProblemField::from_validation_errors(fields));
            }
        }

        for (key, value) in error.extensions() {
            problem
                .extensions
                .insert(key.clone().into_owned(), value.clone());
        }

        // The chain is the single most useful thing an operator can be handed,
        // and the single most dangerous thing a client can be handed. It ships
        // only when someone deliberately turned disclosure on.
        if options.expose_internal_errors && error.is_server_error() {
            problem
                .extensions
                .insert(CHAIN_MEMBER.to_owned(), Value::String(error.chain()));
        }

        problem
    }

    /// Set `instance`, which is the request path.
    pub fn with_instance(mut self, instance: impl Into<String>) -> Self {
        self.instance = Some(instance.into());
        self
    }

    /// Set the correlation id.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Set the W3C trace id.
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }

    /// The status as a [`StatusCode`], falling back to 500 for a value that is
    /// not a valid status (only reachable through `Deserialize`).
    pub fn status_code(&self) -> StatusCode {
        StatusCode::from_u16(self.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
    }

    /// Serialise to `application/problem+json` bytes.
    ///
    /// Infallible: a serialisation failure falls back to a hand-written
    /// document, because failing to render an error is not an option.
    ///
    /// This is also where an extension that would collide with a structural
    /// member is republished under its [`extension_key`], so no document that
    /// leaves this method can carry `status` twice. The check is a scan of the
    /// extension names and copies nothing unless it finds one, so the common
    /// case — no extensions, or none reserved — costs a comparison per member.
    ///
    /// ```
    /// use moso::error::problem::Problem;
    /// use serde_json::{Value, json};
    ///
    /// let mut problem = Problem::new(http::StatusCode::CONFLICT, "urn:x", "Conflict");
    /// problem.extensions.insert("status".to_owned(), json!("sneaky"));
    ///
    /// let document: Value = serde_json::from_slice(&problem.to_bytes()).unwrap();
    /// assert_eq!(document["status"], json!(409));
    /// assert_eq!(document["x_status"], json!("sneaky"));
    /// ```
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        if self.extensions.keys().any(|key| is_reserved_member(key)) {
            let mut safe = self.clone();
            safe.extensions = self
                .extensions
                .iter()
                .map(|(key, value)| (extension_key(key).into_owned(), value.clone()))
                .collect();
            return serde_json::to_vec(&safe).unwrap_or_else(|_| FALLBACK_DOCUMENT.to_vec());
        }
        serde_json::to_vec(self).unwrap_or_else(|_| FALLBACK_DOCUMENT.to_vec())
    }
}

/// The document emitted when even serialising the problem fails.
///
/// Only an extension member holding a value `serde_json` refuses can reach
/// this, and the answer to "we cannot render the error" has to be a valid
/// problem document rather than an empty body.
const FALLBACK_DOCUMENT: &[u8] =
    br#"{"type":"https://moso.rs/errors/internal","title":"Internal Server Error","status":500}"#;

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = self.to_bytes();
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(PROBLEM_CONTENT_TYPE));
        response
    }
}

/// What the renderer is allowed to disclose, and in what shape.
///
/// Assembled once at boot from `HttpConfig` and the active [`Profile`].
#[derive(Debug, Clone)]
pub struct ProblemOptions {
    /// Whether a 5xx may carry its `detail` and source chain.
    ///
    /// `http.expose_internal_errors`. Defaults to `false` in every profile;
    /// turning it on in production is a deliberate, logged decision.
    pub expose_internal_errors: bool,
    /// The active profile. Controls how much the HTML page shows.
    pub profile: Profile,
    /// Whether to honour `Accept: text/html` with [`html_page`].
    pub html_errors: bool,
    /// The base URI prepended to a kind's slug when the error carries no
    /// explicit `type`.
    pub type_base: &'static str,
}

impl Default for ProblemOptions {
    /// The safe defaults: disclose nothing, assume production.
    fn default() -> Self {
        Self {
            expose_internal_errors: false,
            profile: Profile::Production,
            html_errors: true,
            type_base: crate::error::ERROR_TYPE_BASE,
        }
    }
}

impl ProblemOptions {
    /// Options for `profile`, with the profile's conventional defaults.
    ///
    /// [`Profile::Dev`] exposes internals and therefore renders the rich HTML
    /// page; [`Profile::Test`] and [`Profile::Production`] do not, so a test
    /// asserts against the bytes production will actually send.
    pub fn for_profile(profile: Profile) -> Self {
        Self {
            expose_internal_errors: profile.exposes_errors(),
            profile,
            html_errors: true,
            type_base: crate::error::ERROR_TYPE_BASE,
        }
    }

    /// Whether `status` may carry its detail under these options.
    pub fn discloses(&self, status: StatusCode) -> bool {
        !status.is_server_error() || self.expose_internal_errors
    }
}

// ---------------------------------------------------------------------------
// The ambient request context
// ---------------------------------------------------------------------------

/// What the error renderer knows about the request it is answering.
///
/// `IntoResponse` takes no arguments, so a bare `Error` rendered by
/// macro-generated glue would otherwise carry no correlation id and no
/// configured disclosure policy. The layer that owns the request installs one
/// of these with [`with_error_context`]; everything rendered inside the scope
/// picks it up.
///
/// [`Default`] is the conservative context: production options, no ids, JSON.
#[derive(Debug, Clone, Default)]
pub struct ErrorContext {
    /// What may be disclosed, and in what shape.
    pub options: ProblemOptions,
    /// The correlation id, echoed as `request_id`.
    pub request_id: Option<String>,
    /// The W3C trace id, echoed as `trace_id`.
    pub trace_id: Option<String>,
    /// The request path, echoed as `instance`.
    pub instance: Option<String>,
    /// Whether this client asked for HTML; see [`prefers_html`].
    pub prefers_html: bool,
}

impl ErrorContext {
    /// A context for `options` with no request identity yet.
    pub fn new(options: ProblemOptions) -> Self {
        Self {
            options,
            request_id: None,
            trace_id: None,
            instance: None,
            prefers_html: false,
        }
    }

    /// Fill in everything derivable from the request parts: the path and
    /// whether the client prefers HTML.
    pub fn with_parts(mut self, parts: &http::request::Parts) -> Self {
        self.instance = Some(parts.uri.path().to_owned());
        self.prefers_html = parts_prefer_html(parts);
        self
    }

    /// Set the correlation id.
    pub fn with_request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }

    /// Set the W3C trace id.
    pub fn with_trace_id(mut self, trace_id: impl Into<String>) -> Self {
        self.trace_id = Some(trace_id.into());
        self
    }
}

tokio::task_local! {
    /// The context every `IntoResponse for Error` inside this task reads.
    static ERROR_CONTEXT: ErrorContext;
}

/// Run `future` with `context` installed for every error rendered inside it.
///
/// A task-local rather than a thread-local: a request's future is moved between
/// worker threads freely, and a thread-local would attribute one request's id
/// to another's error.
pub async fn with_error_context<F>(context: ErrorContext, future: F) -> F::Output
where
    F: core::future::Future,
{
    ERROR_CONTEXT.scope(context, future).await
}

/// The context installed by the innermost enclosing [`with_error_context`].
///
/// `None` outside any scope, which is the conservative case: the renderer then
/// discloses nothing and emits no ids.
pub fn current_error_context() -> Option<ErrorContext> {
    ERROR_CONTEXT.try_with(Clone::clone).ok()
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render `error` as a response under `context`.
///
/// The single entry point both `IntoResponse` and the `catch_error` layer go
/// through, so the disclosure rule, the content negotiation and the error's own
/// headers are applied in exactly one place.
pub fn render(error: &Error, context: &ErrorContext) -> Response {
    let mut problem = Problem::from_error(error, &context.options);
    if let Some(instance) = &context.instance {
        problem.instance = Some(instance.clone());
    }
    if let Some(request_id) = &context.request_id {
        problem.request_id = Some(request_id.clone());
    }
    if let Some(trace_id) = &context.trace_id {
        problem.trace_id = Some(trace_id.clone());
    }

    let mut response = if context.prefers_html && context.options.html_errors {
        let status = problem.status_code();
        let body = html_page(&problem, Some(error), &context.options);
        let mut response = Response::new(axum::body::Body::from(body));
        *response.status_mut() = status;
        response
            .headers_mut()
            .insert(CONTENT_TYPE, HeaderValue::from_static(HTML_CONTENT_TYPE));
        response
    } else {
        problem.into_response()
    };

    if let Some(headers) = error.headers() {
        for (name, value) in headers {
            response.headers_mut().append(name.clone(), value.clone());
        }
    }
    response
}

impl IntoResponse for Error {
    /// The conservative rendering, described in the module header.
    ///
    /// Discloses a 5xx detail only when an [`ErrorContext`] in scope says the
    /// configuration allows it; with no context in scope it discloses nothing,
    /// because at that point the configuration is not reachable. Stores
    /// `Arc<Self>` in the response extensions so the `catch_error` layer can
    /// log it once and re-render it with the request context.
    fn into_response(self) -> Response {
        let context = current_error_context().unwrap_or_default();
        let error: ErrorRef = Arc::new(self);
        let mut response = render(&error, &context);
        response.extensions_mut().insert(error);
        response
    }
}

/// The `Arc<Error>` a rendered error response carries in its extensions.
///
/// The `catch_error` layer looks for this; middleware that wants to observe
/// errors (metrics, Sentry) should look for it too rather than parsing the body.
pub type ErrorRef = Arc<Error>;

/// Render `problem` as an HTML page.
///
/// In [`Profile::Dev`] the page includes the error chain, the backtrace with
/// the user's frames highlighted, and the matched route. In any other profile
/// it is a title, a status, a sentence and the request id — nothing an attacker
/// can learn from.
///
/// The rich page additionally requires
/// [`expose_internal_errors`](ProblemOptions::expose_internal_errors) for a
/// 5xx: acceptance criterion 2 says an internal error's source never reaches
/// the body in *any* profile without that flag, and "any" includes `dev`.
///
/// The page is self-contained: no external stylesheet, no script, no font.
pub fn html_page(problem: &Problem, error: Option<&Error>, options: &ProblemOptions) -> String {
    let verbose = options.profile == Profile::Dev && options.discloses(problem.status_code());

    let mut page = String::with_capacity(4096);
    page.push_str("<!doctype html>\n<html lang=\"en\">\n<head>\n");
    page.push_str("<meta charset=\"utf-8\">\n");
    page.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    page.push_str("<meta name=\"robots\" content=\"noindex\">\n");
    page.push_str(&format!(
        "<title>{} {}</title>\n",
        problem.status,
        escape(&problem.title)
    ));
    page.push_str("<style>\n");
    page.push_str(STYLE);
    page.push_str("</style>\n</head>\n<body>\n<main>\n");

    page.push_str(&format!(
        "<p class=\"status\">{}</p>\n<h1>{}</h1>\n",
        problem.status,
        escape(&problem.title)
    ));

    match &problem.detail {
        Some(detail) => page.push_str(&format!("<p class=\"detail\">{}</p>\n", escape(detail))),
        None => page.push_str(
            "<p class=\"detail\">The server could not complete this request. \
             Quote the request id below when reporting it.</p>\n",
        ),
    }

    page.push_str("<dl class=\"meta\">\n");
    if let Some(instance) = &problem.instance {
        push_meta(&mut page, "path", instance);
    }
    if let Some(request_id) = &problem.request_id {
        push_meta(&mut page, "request id", request_id);
    }
    if let Some(trace_id) = &problem.trace_id {
        push_meta(&mut page, "trace id", trace_id);
    }
    push_meta(&mut page, "type", &problem.type_uri);
    page.push_str("</dl>\n");

    if let Some(fields) = &problem.errors {
        page.push_str("<h2>Fields</h2>\n<table>\n");
        page.push_str("<thead><tr><th>pointer</th><th>code</th><th>message</th></tr></thead>\n");
        page.push_str("<tbody>\n");
        for field in fields {
            page.push_str(&format!(
                "<tr><td><code>{}</code></td><td><code>{}</code></td><td>{}</td></tr>\n",
                escape(&field.pointer),
                escape(&field.code),
                escape(&field.message)
            ));
        }
        page.push_str("</tbody>\n</table>\n");
    }

    if verbose {
        if let Some(error) = error {
            page.push_str("<h2>Error chain</h2>\n");
            page.push_str(&format!(
                "<pre class=\"chain\">{}</pre>\n",
                escape(&error.chain())
            ));

            if let Some(backtrace) = error.backtrace() {
                page.push_str("<h2>Backtrace</h2>\n<pre class=\"backtrace\">");
                for line in backtrace.to_string().lines() {
                    // A frame from the user's own crate is what they are
                    // looking for; framework and std frames are scenery.
                    let interesting = !is_framework_frame(line);
                    if interesting {
                        page.push_str(&format!("<b>{}</b>\n", escape(line)));
                    } else {
                        page.push_str(&format!("{}\n", escape(line)));
                    }
                }
                page.push_str("</pre>\n");
            }
        }

        page.push_str("<h2>Problem document</h2>\n");
        let json = serde_json::to_string_pretty(problem)
            .unwrap_or_else(|_| String::from_utf8_lossy(FALLBACK_DOCUMENT).into_owned());
        page.push_str(&format!("<pre class=\"json\">{}</pre>\n", escape(&json)));

        page.push_str(
            "<p class=\"footer\">This page is rendered because the profile is \
             <code>dev</code>. It is never rendered in production.</p>\n",
        );
    } else {
        page.push_str("<p class=\"footer\">moso</p>\n");
    }

    page.push_str("</main>\n</body>\n</html>\n");
    page
}

/// One `<dt>`/`<dd>` pair of the metadata list.
fn push_meta(page: &mut String, label: &str, value: &str) {
    page.push_str(&format!(
        "<dt>{}</dt><dd><code>{}</code></dd>\n",
        escape(label),
        escape(value)
    ));
}

/// Whether a backtrace line belongs to the framework, the runtime or `std`.
///
/// Used only to decide what to embolden, so a false negative costs a bold line
/// and nothing else.
fn is_framework_frame(line: &str) -> bool {
    const NOISE: &[&str] = &[
        "moso_core::",
        "moso::",
        "axum::",
        "axum_core::",
        "tower::",
        "tower_http::",
        "hyper::",
        "hyper_util::",
        "tokio::",
        "core::",
        "std::",
        "alloc::",
        "__rust",
        "rust_begin_unwind",
        "backtrace::",
    ];
    NOISE.iter().any(|needle| line.contains(needle))
}

/// The page's stylesheet, inlined because the page must render with no network.
const STYLE: &str = "\
:root{color-scheme:light dark}
*{box-sizing:border-box}
body{margin:0;padding:3rem 1.5rem;font:16px/1.55 ui-sans-serif,system-ui,-apple-system,\
'Segoe UI',Roboto,sans-serif;background:#fbfbfd;color:#1c1c22}
main{max-width:52rem;margin:0 auto}
.status{margin:0;font-size:.8rem;letter-spacing:.12em;text-transform:uppercase;color:#8a8a96}
h1{margin:.2rem 0 1rem;font-size:1.9rem;line-height:1.2;font-weight:650}
h2{margin:2rem 0 .6rem;font-size:.78rem;letter-spacing:.1em;text-transform:uppercase;color:#8a8a96}
.detail{margin:0 0 1.6rem;font-size:1.05rem}
dl.meta{display:grid;grid-template-columns:max-content 1fr;gap:.35rem 1.2rem;margin:0;\
padding:1rem 1.2rem;border:1px solid #e4e4ea;border-radius:8px;background:#fff}
dl.meta dt{color:#8a8a96;font-size:.85rem}
dl.meta dd{margin:0;overflow-wrap:anywhere}
code{font:0.86em ui-monospace,SFMono-Regular,Menlo,Consolas,monospace}
pre{margin:0;padding:1rem 1.2rem;border:1px solid #e4e4ea;border-radius:8px;background:#fff;\
overflow-x:auto;font:0.82rem/1.5 ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;\
white-space:pre-wrap;overflow-wrap:anywhere}
pre b{font-weight:650;background:#fff3c4}
table{width:100%;border-collapse:collapse;background:#fff;border:1px solid #e4e4ea;\
border-radius:8px;overflow:hidden}
th,td{padding:.5rem .8rem;text-align:left;border-bottom:1px solid #eeeef2;font-size:.9rem}
th{color:#8a8a96;font-weight:550;font-size:.78rem;text-transform:uppercase;letter-spacing:.06em}
tr:last-child td{border-bottom:none}
.footer{margin-top:2.5rem;color:#8a8a96;font-size:.85rem}
@media(prefers-color-scheme:dark){
body{background:#111116;color:#e8e8ee}
dl.meta,pre,table{background:#18181f;border-color:#2a2a34}
th,td{border-bottom-color:#24242e}
pre b{background:#4a3c00;color:#ffe9a3}
}
";

/// Escape `&`, `<`, `>`, `\"` and `'` for interpolation into HTML.
///
/// Borrows when nothing needs escaping, which is the common case. This is a
/// local copy rather than a call into [`crate::response::escape_html`] on
/// purpose: the error page is the last thing that runs before the client sees
/// a failure, and it must not depend on another module being correct.
fn escape(input: &str) -> Cow<'_, str> {
    if !input.contains(['&', '<', '>', '"', '\'']) {
        return Cow::Borrowed(input);
    }
    let mut out = String::with_capacity(input.len() + 16);
    for character in input.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    Cow::Owned(out)
}

// ---------------------------------------------------------------------------
// Content negotiation
// ---------------------------------------------------------------------------

/// Whether this request prefers HTML over JSON.
///
/// True only when `text/html` outranks `application/json` *and*
/// `application/problem+json` in the `Accept` header — an API client that sends
/// `*/*` keeps getting JSON.
pub fn prefers_html(request: &Request) -> bool {
    headers_prefer_html(request.headers())
}

/// Whether these request *parts* prefer HTML. The [`prefers_html`] variant for
/// callers that have already split the request.
pub fn parts_prefer_html(parts: &http::request::Parts) -> bool {
    headers_prefer_html(&parts.headers)
}

/// The shared body of the two negotiation entry points.
fn headers_prefer_html(headers: &HeaderMap) -> bool {
    let Some(accept) = headers
        .get(http::header::ACCEPT)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };

    let html = accept_quality(accept, "text", "html");
    let json = accept_quality(accept, "application", "json").max(accept_quality(
        accept,
        "application",
        "problem+json",
    ));

    // Strictly greater: a tie — which is what `*/*` alone produces — is JSON.
    html > json
}

/// The quality value `accept` assigns to `type_/subtype`, or `0.0`.
///
/// Follows RFC 9110 precedence: the *most specific* matching range wins even
/// when a broader range carries a higher `q`, which is what makes
/// `text/html, */*;q=0.9` mean what a browser means by it.
fn accept_quality(accept: &str, type_: &str, subtype: &str) -> f32 {
    let mut best: Option<(u8, f32)> = None;

    for entry in accept.split(',') {
        let mut parts = entry.split(';');
        let Some(range) = parts.next().map(str::trim) else {
            continue;
        };
        let Some((range_type, range_subtype)) = range.split_once('/') else {
            continue;
        };
        let (range_type, range_subtype) = (range_type.trim(), range_subtype.trim());

        let specificity = if range_type == "*" && range_subtype == "*" {
            0
        } else if range_type.eq_ignore_ascii_case(type_) && range_subtype == "*" {
            1
        } else if range_type.eq_ignore_ascii_case(type_)
            && range_subtype.eq_ignore_ascii_case(subtype)
        {
            2
        } else {
            continue;
        };

        let mut quality = 1.0_f32;
        for parameter in parts {
            let parameter = parameter.trim();
            let value = parameter
                .strip_prefix("q=")
                .or_else(|| parameter.strip_prefix("Q="));
            if let Some(value) = value {
                quality = value.trim().parse::<f32>().unwrap_or(1.0).clamp(0.0, 1.0);
            }
        }

        let better = match best {
            None => true,
            Some((best_specificity, best_quality)) => {
                specificity > best_specificity
                    || (specificity == best_specificity && quality > best_quality)
            }
        };
        if better {
            best = Some((specificity, quality));
        }
    }

    best.map_or(0.0, |(_, quality)| quality)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::{BootErrors, ErrorKind};
    use http_body_util::BodyExt;

    /// The string that must never escape a 5xx body.
    const CANARY: &str = "SECRET_TABLE_users_password_hash";

    async fn body_string(response: Response) -> String {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        String::from_utf8(bytes.to_vec()).expect("utf-8 body")
    }

    fn request_with_accept(accept: &str) -> Request {
        Request::builder()
            .header(http::header::ACCEPT, accept)
            .body(axum::body::Body::empty())
            .expect("request")
    }

    #[test]
    fn problem_content_type_is_the_rfc_value() {
        assert_eq!(PROBLEM_CONTENT_TYPE, "application/problem+json");
    }

    #[test]
    fn default_options_disclose_nothing() {
        let options = ProblemOptions::default();
        assert!(!options.expose_internal_errors);
        assert_eq!(options.profile, Profile::Production);
    }

    #[test]
    fn options_follow_the_profile() {
        assert!(ProblemOptions::for_profile(Profile::Dev).expose_internal_errors);
        assert!(!ProblemOptions::for_profile(Profile::Test).expose_internal_errors);
        assert!(!ProblemOptions::for_profile(Profile::Production).expose_internal_errors);
    }

    // ── the wire format ──────────────────────────────────────────────────

    #[test]
    fn a_problem_matches_the_documented_shape() {
        let error = Error::conflict("A user with this email already exists").with_field(
            "/email",
            "unique",
            "already taken",
        );
        let problem = Problem::from_error(&error, &ProblemOptions::default())
            .with_instance("/api/v1/users")
            .with_request_id("01J8XG7K3RQZ4B0N2Y6M9C5V1T")
            .with_trace_id("4bf92f3577b34da6a3ce929d0e0e4736");

        let json: serde_json::Value =
            serde_json::from_slice(&problem.to_bytes()).expect("valid json");
        assert_eq!(json["type"], "https://moso.rs/errors/conflict");
        assert_eq!(json["title"], "Conflict");
        assert_eq!(json["status"], 409);
        assert_eq!(json["detail"], "A user with this email already exists");
        assert_eq!(json["instance"], "/api/v1/users");
        assert_eq!(json["errors"][0]["pointer"], "/email");
        assert_eq!(json["errors"][0]["code"], "unique");
        assert_eq!(json["errors"][0]["message"], "already taken");
        assert_eq!(json["request_id"], "01J8XG7K3RQZ4B0N2Y6M9C5V1T");
        assert_eq!(json["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn a_problem_round_trips() {
        let mut problem = Problem::new(
            StatusCode::CONFLICT,
            "https://moso.rs/errors/conflict",
            "Conflict",
        )
        .with_instance("/users");
        problem
            .extensions
            .insert("sku".to_owned(), serde_json::json!("ABC-1"));
        problem.errors = Some(vec![ProblemField {
            pointer: "/email".to_owned(),
            code: "unique".to_owned(),
            message: "taken".to_owned(),
            params: BTreeMap::new(),
        }]);

        let bytes = problem.to_bytes();
        let parsed: Problem = serde_json::from_slice(&bytes).expect("round trip");
        assert_eq!(parsed, problem);
    }

    #[test]
    fn extensions_land_at_the_top_level() {
        let error = Error::too_many(std::time::Duration::from_secs(30));
        let problem = Problem::from_error(&error, &ProblemOptions::default());
        let json: serde_json::Value =
            serde_json::from_slice(&problem.to_bytes()).expect("valid json");
        assert_eq!(json["retry_after"], 30);
    }

    #[test]
    fn an_extension_can_never_overwrite_a_structural_member() {
        // The failure this prevents: a `status` extension flattened beside the
        // real one, leaving a client to pick whichever its parser kept.
        let error = Error::conflict("the order already shipped")
            .with_extension("status", "shipped")
            .with_extension("detail", "mine")
            .with_extension("order_id", 7);

        let problem = Problem::from_error(&error, &ProblemOptions::default());
        let json: serde_json::Value =
            serde_json::from_slice(&problem.to_bytes()).expect("valid json");

        assert_eq!(json["status"], 409);
        assert_eq!(json["detail"], "the order already shipped");
        assert_eq!(json["x_status"], "shipped");
        assert_eq!(json["x_detail"], "mine");
        assert_eq!(json["order_id"], 7, "an ordinary name is left alone");
    }

    #[test]
    fn a_hand_built_problem_is_republished_the_same_way() {
        // `Problem::extensions` is a public field, so the guard cannot live
        // only in `Error::with_extension`.
        let mut problem = Problem::new(StatusCode::GONE, "urn:example", "Gone");
        problem
            .extensions
            .insert("status".to_owned(), serde_json::json!(200));
        problem
            .extensions
            .insert("sku".to_owned(), serde_json::json!("ABC-1"));

        let json: serde_json::Value =
            serde_json::from_slice(&problem.to_bytes()).expect("valid json");
        assert_eq!(json["status"], 410);
        assert_eq!(json["x_status"], 200);
        assert_eq!(json["sku"], "ABC-1");
    }

    #[test]
    fn the_reserved_list_is_exactly_what_the_document_serialises() {
        // The list and the `serde` attributes are two statements of one fact,
        // so this test is what stops them drifting: every member a fully
        // populated problem emits, minus the extensions it was given, must be
        // in `RESERVED_MEMBERS`.
        let mut problem = Problem::new(StatusCode::CONFLICT, "urn:example", "Conflict")
            .with_instance("/orders/1")
            .with_request_id("01J8XG7K3RQZ4B0N2Y6M9C5V1T")
            .with_trace_id("4bf92f3577b34da6a3ce929d0e0e4736");
        problem.detail = Some("nope".to_owned());
        problem.errors = Some(Vec::new());

        let json = serde_json::to_value(&problem).expect("valid json");
        let members: Vec<&str> = json
            .as_object()
            .expect("an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(members.len(), RESERVED_MEMBERS.len());
        for member in members {
            assert!(
                is_reserved_member(member),
                "`{member}` is serialised but not reserved"
            );
        }
        assert!(
            !is_reserved_member(CHAIN_MEMBER),
            "the chain is itself an extension"
        );
    }

    #[test]
    fn field_errors_keep_their_params() {
        let field = moso_schema::FieldError::new("/name", "len", "too short")
            .with_param("min", 3)
            .with_param("max", 32);
        let error = Error::validation(ValidationErrors::from(field));
        let problem = Problem::from_error(&error, &ProblemOptions::default());
        let fields = problem.errors.expect("errors");
        assert_eq!(fields[0].params["min"], serde_json::json!(3));
        assert_eq!(fields[0].params["max"], serde_json::json!(32));
    }

    // ── the disclosure rule ──────────────────────────────────────────────

    #[tokio::test]
    async fn an_internal_error_never_leaks_its_detail_by_default() {
        let error = Error::internal(std::io::Error::other(CANARY));
        let body = body_string(error.into_response()).await;

        assert!(
            !body.contains(CANARY),
            "the canary escaped a default-rendered 500: {body}"
        );
        let json: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(json["status"], 500);
        assert_eq!(json["title"], "Internal Server Error");
        assert!(json.get("detail").is_none());
        assert!(json.get(CHAIN_MEMBER).is_none());
    }

    #[tokio::test]
    async fn an_internal_error_leaks_nothing_in_any_profile_without_the_flag() {
        for profile in [Profile::Dev, Profile::Test, Profile::Production] {
            let error = Error::internal(std::io::Error::other(CANARY));
            let mut options = ProblemOptions::for_profile(profile);
            options.expose_internal_errors = false;

            let context = ErrorContext::new(options);
            let body = body_string(render(&error, &context)).await;
            assert!(
                !body.contains(CANARY),
                "the canary escaped in profile {profile}: {body}"
            );
        }
    }

    #[tokio::test]
    async fn the_html_page_suppresses_the_canary_too() {
        let error = Error::internal(std::io::Error::other(CANARY));
        let mut options = ProblemOptions::for_profile(Profile::Dev);
        options.expose_internal_errors = false;

        let mut context = ErrorContext::new(options);
        context.prefers_html = true;

        let body = body_string(render(&error, &context)).await;
        assert!(body.contains("<!doctype html>"));
        assert!(
            !body.contains(CANARY),
            "the canary escaped the dev HTML page: {body}"
        );
    }

    #[tokio::test]
    async fn exposing_internal_errors_reveals_the_detail_and_the_chain() {
        let error = Error::internal(std::io::Error::other(CANARY));
        let options = ProblemOptions {
            expose_internal_errors: true,
            ..ProblemOptions::default()
        };

        let body = body_string(render(&error, &ErrorContext::new(options))).await;
        let json: serde_json::Value = serde_json::from_str(&body).expect("valid json");
        assert_eq!(json["detail"], CANARY);
        assert!(json[CHAIN_MEMBER].as_str().expect("chain").contains(CANARY));
    }

    #[test]
    fn field_errors_are_suppressed_on_a_5xx() {
        let error = Error::new(ErrorKind::Internal).with_field("/secret", "custom:x", CANARY);
        let problem = Problem::from_error(&error, &ProblemOptions::default());
        assert!(problem.errors.is_none());
    }

    #[test]
    fn a_4xx_always_discloses() {
        let error = Error::bad_request("missing `email`");
        let problem = Problem::from_error(&error, &ProblemOptions::default());
        assert_eq!(problem.detail.as_deref(), Some("missing `email`"));
    }

    // ── responses ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn the_response_carries_the_status_content_type_and_error_ref() {
        let error = Error::not_found("user");
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some(PROBLEM_CONTENT_TYPE)
        );
        let reference = response
            .extensions()
            .get::<ErrorRef>()
            .expect("Arc<Error> in extensions");
        assert_eq!(reference.status(), StatusCode::NOT_FOUND);

        let body = body_string(response).await;
        assert!(body.contains("User not found"));
    }

    #[tokio::test]
    async fn the_errors_own_headers_reach_the_response() {
        let error = Error::too_many(std::time::Duration::from_secs(30));
        let response = error.into_response();
        assert_eq!(
            response
                .headers()
                .get(http::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok()),
            Some("30")
        );
    }

    #[tokio::test]
    async fn the_ambient_context_supplies_the_ids() {
        let context = ErrorContext::new(ProblemOptions::default())
            .with_request_id("01J8XG7K3RQZ4B0N2Y6M9C5V1T")
            .with_trace_id("4bf92f3577b34da6a3ce929d0e0e4736");

        let response =
            with_error_context(context, async { Error::not_found("user").into_response() }).await;

        let json: serde_json::Value =
            serde_json::from_str(&body_string(response).await).expect("valid json");
        assert_eq!(json["request_id"], "01J8XG7K3RQZ4B0N2Y6M9C5V1T");
        assert_eq!(json["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
    }

    #[test]
    fn there_is_no_ambient_context_outside_a_scope() {
        assert!(current_error_context().is_none());
    }

    #[tokio::test]
    async fn a_boot_error_still_renders_a_500_problem() {
        let error = Error::boot(BootErrors::new());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    // ── the HTML page ────────────────────────────────────────────────────

    #[test]
    fn the_dev_page_shows_the_chain_and_the_production_page_does_not() {
        let error = Error::internal(std::io::Error::other("connection refused"));
        let dev = ProblemOptions::for_profile(Profile::Dev);
        let production = ProblemOptions::for_profile(Profile::Production);

        let dev_problem = Problem::from_error(&error, &dev);
        let dev_page = html_page(&dev_problem, Some(&error), &dev);
        assert!(dev_page.contains("Error chain"));
        assert!(dev_page.contains("connection refused"));
        assert!(dev_page.contains("Problem document"));

        let production_problem = Problem::from_error(&error, &production);
        let production_page = html_page(&production_problem, Some(&error), &production);
        assert!(!production_page.contains("Error chain"));
        assert!(!production_page.contains("connection refused"));
    }

    #[test]
    fn the_page_is_self_contained() {
        let error = Error::not_found("user");
        let options = ProblemOptions::for_profile(Profile::Dev);
        let page = html_page(
            &Problem::from_error(&error, &options),
            Some(&error),
            &options,
        );

        assert!(!page.contains("http://"));
        assert!(!page.contains("//cdn"));
        assert!(!page.contains("<script"));
        assert!(!page.contains("<link"));
        assert!(page.contains("<style>"));
    }

    #[test]
    fn the_page_escapes_every_interpolated_value() {
        let error = Error::bad_request("<script>alert('xss')</script>")
            .with_title("<b>Bad</b>")
            .with_field("/a<b", "type", "\"quoted\" & <angled>");
        let options = ProblemOptions::for_profile(Profile::Dev);
        let problem = Problem::from_error(&error, &options)
            .with_instance("/users?q=<script>")
            .with_request_id("id\"'<>&");
        let page = html_page(&problem, Some(&error), &options);

        assert!(!page.contains("<script>alert"));
        assert!(page.contains("&lt;script&gt;alert(&#39;xss&#39;)&lt;/script&gt;"));
        assert!(page.contains("&lt;b&gt;Bad&lt;/b&gt;"));
        assert!(page.contains("/a&lt;b"));
        assert!(page.contains("&quot;quoted&quot; &amp; &lt;angled&gt;"));
        // Exactly one `<script` would be a bug; there should be none at all.
        assert!(!page.contains("<script"));
    }

    #[test]
    fn escape_borrows_when_it_can() {
        assert!(matches!(escape("nothing to do"), Cow::Borrowed(_)));
        assert!(matches!(escape("a<b"), Cow::Owned(_)));
        assert_eq!(escape("&"), "&amp;");
    }

    #[test]
    fn the_page_lists_field_errors() {
        let mut errors = ValidationErrors::new();
        errors.add("/email", "format", "must be an email address");
        let error = Error::validation(errors);
        let options = ProblemOptions::for_profile(Profile::Production);
        let page = html_page(
            &Problem::from_error(&error, &options),
            Some(&error),
            &options,
        );
        assert!(page.contains("/email"));
        assert!(page.contains("must be an email address"));
    }

    // ── content negotiation ──────────────────────────────────────────────

    #[test]
    fn a_browser_gets_html() {
        assert!(prefers_html(&request_with_accept(
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,*/*;q=0.8"
        )));
        assert!(prefers_html(&request_with_accept("text/html")));
        assert!(prefers_html(&request_with_accept("text/html, */*;q=0.9")));
    }

    #[test]
    fn an_api_client_gets_json() {
        assert!(!prefers_html(&request_with_accept("*/*")));
        assert!(!prefers_html(&request_with_accept("application/json")));
        assert!(!prefers_html(&request_with_accept(
            "application/problem+json"
        )));
        assert!(!prefers_html(&request_with_accept(
            "application/json, text/html;q=0.5"
        )));
        // A tie goes to JSON.
        assert!(!prefers_html(&request_with_accept(
            "text/html;q=0.8, application/json;q=0.8"
        )));
    }

    #[test]
    fn a_missing_or_unreadable_accept_gets_json() {
        let bare = Request::builder()
            .body(axum::body::Body::empty())
            .expect("request");
        assert!(!prefers_html(&bare));
        assert!(!prefers_html(&request_with_accept("garbage")));
        assert!(!prefers_html(&request_with_accept("")));
    }

    #[test]
    fn specificity_beats_a_higher_quality_on_a_broader_range() {
        // `text/*` is more specific than `*/*`, so its q is the one that counts
        // even though `*/*` claims a higher one.
        assert!((accept_quality("text/*;q=0.4, */*;q=0.9", "text", "html") - 0.4).abs() < 1e-6);
        assert!((accept_quality("*/*;q=0.9", "text", "html") - 0.9).abs() < 1e-6);
        assert_eq!(accept_quality("application/json", "text", "html"), 0.0);
    }

    #[test]
    fn a_malformed_q_value_is_treated_as_one() {
        assert!((accept_quality("text/html;q=banana", "text", "html") - 1.0).abs() < 1e-6);
        assert!((accept_quality("text/html;q=9", "text", "html") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parts_and_requests_negotiate_identically() {
        let request = request_with_accept("text/html");
        let (parts, _) = request.into_parts();
        assert!(parts_prefer_html(&parts));
    }

    #[test]
    fn a_context_built_from_parts_carries_the_path() {
        let request = Request::builder()
            .uri("/api/v1/users?q=1")
            .header(http::header::ACCEPT, "text/html")
            .body(axum::body::Body::empty())
            .expect("request");
        let (parts, _) = request.into_parts();
        let context = ErrorContext::new(ProblemOptions::default()).with_parts(&parts);
        assert_eq!(context.instance.as_deref(), Some("/api/v1/users"));
        assert!(context.prefers_html);
    }
}
