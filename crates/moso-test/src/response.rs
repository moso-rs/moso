//! The response, and the assertions that make a failure readable.
//!
//! Every assertion on [`TestResponse`] follows the same rule: it either returns
//! `&self` so the next one can be chained, or it panics with the whole story —
//! the request that was sent, the response that came back, a JSON diff when one
//! applies, and the server's own log lines for that request id.
//!
//! ```text
//! ── moso-test: assertion failed ────────────────────────────────────────
//!   expected status 201, got 422
//!
//!   request:
//!     POST http://localhost/users
//!   request body:
//!     { "email": "a@b.com", "password": "short" }
//!
//!   response:
//!     422 Unprocessable Entity  (1.1 ms, in-process)
//!   response body:
//!     { "type": "https://moso.rs/errors/validation", "status": 422,
//!       "errors": [ { "pointer": "/password", "code": "len", … } ] }
//!
//!   server logs for request_id moso-test-0000abc1-000000000003:
//!     INFO  moso::http  422 POST /users  1.1ms
//! ──────────────────────────────────────────────────────────────────────
//! ```
//!
//! The body is buffered before the response is handed back, which is what makes
//! all of this possible: an assertion five lines later can still print it, and
//! [`json`](TestResponse::json) does not need to be `async`.

use std::fmt::Write as _;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use moso::openapi::{Document, HttpMethod, MediaType, Operation};
use moso::{AppState, Problem};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::contract::Options as ContractOptions;
use crate::logs::LogAssertions;
use crate::report::{self, RequestRecord};

/// Anything that names an HTTP status.
///
/// Exists so `assert_status(201)` and `assert_status(StatusCode::CREATED)` are
/// both spelled the obvious way. Implemented for [`StatusCode`] and for `u16`;
/// there is nothing to implement yourself.
///
/// ```
/// use moso::deps::http::StatusCode;
/// use moso_test::IntoStatus;
///
/// assert_eq!(201.into_status(), StatusCode::CREATED);
/// assert_eq!(StatusCode::CREATED.into_status(), StatusCode::CREATED);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` does not name an HTTP status",
    label = "not a status",
    note = "`assert_status` takes a `u16` or a `StatusCode`, and there is nothing to implement \
            yourself",
    note = "help: pass the number — `.assert_status(201)`",
    note = "help: or the constant — `.assert_status(StatusCode::CREATED)`, from \
            `moso::deps::http`"
)]
pub trait IntoStatus {
    /// The status this value names.
    fn into_status(self) -> StatusCode;
}

impl IntoStatus for StatusCode {
    fn into_status(self) -> StatusCode {
        self
    }
}

impl IntoStatus for u16 {
    fn into_status(self) -> StatusCode {
        StatusCode::from_u16(self)
            .unwrap_or_else(|_| panic!("moso-test: {self} is not an HTTP status code"))
    }
}

/// A buffered response, with assertions.
///
/// ```
/// use moso_test::prelude::*;
/// # /// A user, as the API accepts one.
/// # #[derive(moso::Schema)] pub struct CreateUser {
/// #     /// Public handle.
/// #     #[schema(len = 3..=32)] pub username: String }
/// # /// A user, as the API returns one.
/// # #[derive(moso::Schema)] pub struct UserOut {
/// #     /// Stable identifier.
/// #     pub id: u64,
/// #     /// Public handle.
/// #     pub username: String }
/// # /// Everything this application reads from its environment.
/// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "users")] pub name: String }
/// # /// Create a user.
/// # #[moso::endpoint]
/// # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
/// #     -> moso::Result<moso::response::Created<UserOut>>
/// # {
/// #     Ok(moso::response::Created::at("/users/1", UserOut { id: 1, username: body.username }))
/// # }
/// # /// The composition root every Moso application exposes.
/// # fn app() -> moso::AppBuilder {
/// #     moso::App::new(AppConfig { name: "users".to_owned() })
/// #         .mount(moso::routes! { POST "/users" => create })
/// # }
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso::Result<()> {
/// # let app = TestApp::builder().app(app()).spawn().await?;
/// app.client()
///     .post("/users")
///     .json(&serde_json::json!({ "username": "ada" }))
///     .send()
///     .await
///     .assert_status(201)
///     .assert_header_present("location")
///     .assert_json_path("/username", "ada")
///     .assert_matches_openapi();
///
/// // A rejected body is a problem document, checked the same way.
/// app.client()
///     .post("/users")
///     .json(&serde_json::json!({ "username": "ab" }))
///     .send()
///     .await
///     .assert_status(422)
///     .assert_json_path("/errors/0/pointer", "/username");
/// # Ok(())
/// # }
/// ```
///
/// Every assertion returns `&Self`, so they chain; a failure prints the request, the
/// response and the captured log lines rather than `left == right`.
pub struct TestResponse {
    request: RequestRecord,
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
    elapsed: Duration,
    request_id: String,
    state: Arc<AppState>,
    logs: LogAssertions,
}

impl TestResponse {
    #[allow(
        clippy::too_many_arguments,
        reason = "a private constructor for a value with eight fields; a parameter struct would be the same eight names one indirection away"
    )]
    pub(crate) fn new(
        request: RequestRecord,
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes,
        elapsed: Duration,
        request_id: String,
        state: Arc<AppState>,
        logs: LogAssertions,
    ) -> Self {
        Self {
            request,
            status,
            headers,
            body,
            elapsed,
            request_id,
            state,
            logs,
        }
    }

    /// The OpenAPI document this response is judged against.
    #[must_use]
    pub fn openapi(&self) -> &Document {
        self.state.document()
    }

    // ── accessors ─────────────────────────────────────────────────────────

    /// The status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.status
    }

    /// The response headers.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    /// One header's value, if it is present and ASCII.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
    }

    /// The raw body.
    #[must_use]
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// The body as text, replacing anything that is not UTF-8.
    #[must_use]
    pub fn text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    /// The request that produced this response.
    #[must_use]
    pub fn request(&self) -> &RequestRecord {
        &self.request
    }

    /// How long the round trip took.
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    /// The correlation id the request carried, which is the key the server logs
    /// are filed under.
    #[must_use]
    pub fn request_id(&self) -> &str {
        &self.request_id
    }

    /// The server-side log lines emitted while serving this request.
    #[must_use]
    pub fn logs(&self) -> Vec<crate::LogRecord> {
        self.logs.for_request(&self.request_id)
    }

    // ── typed bodies ──────────────────────────────────────────────────────

    /// Deserialise the body, failing the test with the full report on error.
    #[must_use]
    pub fn json<T: DeserializeOwned>(&self) -> T {
        match serde_json::from_slice(&self.body) {
            Ok(value) => value,
            Err(error) => self.fail(
                &format!(
                    "the body does not deserialise into {}",
                    core::any::type_name::<T>()
                ),
                &[("serde error", error.to_string())],
            ),
        }
    }

    /// Deserialise the body, returning the error instead of failing.
    ///
    /// # Errors
    ///
    /// Whatever `serde_json` says about the body.
    pub fn try_json<T: DeserializeOwned>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }

    /// The body as a `serde_json::Value`, failing the test if it is not JSON.
    #[must_use]
    pub fn json_value(&self) -> Value {
        self.json()
    }

    /// The body parsed as an RFC 9457 problem document.
    #[must_use]
    pub fn problem(&self) -> Problem {
        self.json()
    }

    // ── status and headers ────────────────────────────────────────────────

    /// Assert the exact status.
    pub fn assert_status(&self, expected: impl IntoStatus) -> &Self {
        let expected = expected.into_status();
        if self.status == expected {
            return self;
        }
        self.fail(
            &format!(
                "expected status {} {}, got {} {}",
                expected.as_u16(),
                expected.canonical_reason().unwrap_or(""),
                self.status.as_u16(),
                self.status.canonical_reason().unwrap_or(""),
            ),
            &[],
        )
    }

    /// Assert the status is a 2xx.
    pub fn assert_ok(&self) -> &Self {
        if self.status.is_success() {
            return self;
        }
        self.fail(
            &format!("expected a 2xx status, got {}", self.status.as_u16()),
            &[],
        )
    }

    /// Assert a header is present with exactly this value.
    pub fn assert_header(&self, name: &str, expected: &str) -> &Self {
        match self.header(name) {
            Some(actual) if actual == expected => self,
            Some(actual) => self.fail(
                &format!("expected header `{name}` to be {expected:?}, got {actual:?}"),
                &[],
            ),
            None => self.fail(
                &format!("expected header `{name}` to be {expected:?}, but it is absent"),
                &[],
            ),
        }
    }

    /// Assert a header is present, whatever its value.
    pub fn assert_header_present(&self, name: &str) -> &Self {
        if self.headers.contains_key(name) {
            return self;
        }
        self.fail(&format!("expected header `{name}` to be present"), &[])
    }

    /// Assert a header is absent.
    pub fn assert_no_header(&self, name: &str) -> &Self {
        if !self.headers.contains_key(name) {
            return self;
        }
        self.fail(&format!("expected header `{name}` to be absent"), &[])
    }

    /// Assert a header is present and contains `needle`.
    ///
    /// The right assertion for `Content-Type`, where
    /// `application/json; charset=utf-8` and `application/json` are the same
    /// answer.
    pub fn assert_header_contains(&self, name: &str, needle: &str) -> &Self {
        match self.header(name) {
            Some(actual) if actual.contains(needle) => self,
            Some(actual) => self.fail(
                &format!("expected header `{name}` to contain {needle:?}, got {actual:?}"),
                &[],
            ),
            None => self.fail(
                &format!("expected header `{name}` to contain {needle:?}, but it is absent"),
                &[],
            ),
        }
    }

    /// Assert the body is empty.
    pub fn assert_empty_body(&self) -> &Self {
        if self.body.is_empty() {
            return self;
        }
        self.fail(
            &format!("expected an empty body, got {} bytes", self.body.len()),
            &[],
        )
    }

    /// Assert the body, as text, contains `needle`.
    pub fn assert_text_contains(&self, needle: &str) -> &Self {
        if self.text().contains(needle) {
            return self;
        }
        self.fail(&format!("expected the body to contain {needle:?}"), &[])
    }

    // ── JSON ──────────────────────────────────────────────────────────────

    /// Assert the value at an RFC 6901 JSON Pointer.
    ///
    /// ```
    /// use moso_test::prelude::*;
    /// # /// A user, as the API accepts one.
    /// # #[derive(moso::Schema)] pub struct CreateUser {
    /// #     /// Public handle.
    /// #     #[schema(len = 3..=32)] pub username: String,
    /// #     /// Contact address.
    /// #     pub email: moso::schema::Email }
    /// # /// A user, as the API returns one.
    /// # #[derive(moso::Schema)] pub struct UserOut {
    /// #     /// Stable identifier.
    /// #     pub id: u64,
    /// #     /// Public handle.
    /// #     pub username: String }
    /// # /// Everything this application reads from its environment.
    /// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Service name.
    /// #     #[config(default = "users")] pub name: String }
    /// # /// Create a user.
    /// # #[moso::endpoint]
    /// # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
    /// #     -> moso::Result<moso::response::Created<UserOut>>
    /// # {
    /// #     Ok(moso::response::Created::at(
    /// #         "/users/1",
    /// #         UserOut { id: 1, username: body.username },
    /// #     ))
    /// # }
    /// # /// The composition root every Moso application exposes.
    /// # fn app() -> moso::AppBuilder {
    /// #     moso::App::new(AppConfig { name: "users".to_owned() })
    /// #         .mount(moso::routes! { POST "/users" => create })
    /// # }
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso::Result<()> {
    /// # let app = TestApp::builder().app(app()).spawn().await?;
    /// let response = app.client().post("/users")
    ///     .json(&serde_json::json!({ "username": "ada", "email": "a@b.example" }))
    ///     .send().await;
    ///
    /// response.assert_json_path("/id", 1)
    ///         .assert_json_path("/username", "ada");
    /// # Ok(())
    /// # }
    /// ```
    pub fn assert_json_path(&self, pointer: &str, expected: impl Into<Value>) -> &Self {
        let expected = expected.into();
        let body = self.json_value();
        match body.pointer(pointer) {
            Some(actual) if *actual == expected => self,
            Some(actual) => self.fail(
                &format!("the value at `{pointer}` is not what the test expected"),
                &[(
                    "json diff",
                    crate::diff::render(&crate::diff::exact(&expected, actual)),
                )],
            ),
            None => self.fail(
                &format!("the body has nothing at `{pointer}`"),
                &[("expected", expected.to_string())],
            ),
        }
    }

    /// Assert there is nothing at an RFC 6901 JSON Pointer.
    pub fn assert_no_json_path(&self, pointer: &str) -> &Self {
        let body = self.json_value();
        match body.pointer(pointer) {
            None => self,
            Some(actual) => self.fail(
                &format!("expected nothing at `{pointer}`"),
                &[("found", actual.to_string())],
            ),
        }
    }

    /// Assert the body contains everything in `expected`.
    ///
    /// A **subset** match: members the response has and `expected` does not are
    /// fine. That is what makes an assertion survive the day a field is added,
    /// which it should — a test that breaks when the API gains a member is a
    /// test that will be deleted.
    pub fn assert_json_matches(&self, expected: impl Into<Value>) -> &Self {
        let expected = expected.into();
        let actual = self.json_value();
        let differences = crate::diff::subset(&expected, &actual);
        if differences.is_empty() {
            return self;
        }
        self.fail(
            "the body does not match the expected document",
            &[("json diff", crate::diff::render(&differences))],
        )
    }

    /// Assert the body equals `expected`, member for member.
    pub fn assert_json_eq(&self, expected: impl Into<Value>) -> &Self {
        let expected = expected.into();
        let actual = self.json_value();
        let differences = crate::diff::exact(&expected, &actual);
        if differences.is_empty() {
            return self;
        }
        self.fail(
            "the body is not the expected document",
            &[("json diff", crate::diff::render(&differences))],
        )
    }

    // ── problems ──────────────────────────────────────────────────────────

    /// Assert the body is an RFC 9457 problem of a given class.
    ///
    /// `code` may be the full `type` URI — `https://moso.rs/errors/validation` —
    /// or just its last segment, `validation`, which is what
    /// [`ErrorKind::slug`](moso::ErrorKind::slug) returns and what a test
    /// actually wants to write.
    pub fn assert_problem(&self, code: &str) -> &Self {
        let problem = match self.try_json::<Problem>() {
            Ok(problem) => problem,
            Err(error) => self.fail(
                &format!("expected an RFC 9457 problem of type `{code}`"),
                &[("serde error", error.to_string())],
            ),
        };
        let slug = problem.type_uri.rsplit('/').next().unwrap_or_default();
        if problem.type_uri == code || slug == code {
            return self;
        }
        self.fail(
            &format!(
                "expected a problem of type `{code}`, got `{}`",
                problem.type_uri
            ),
            &[("problem title", problem.title)],
        )
    }

    /// Assert the problem carries a field error at `pointer` with code `code`.
    ///
    /// The assertion a validation test is actually making: not "it was a 422"
    /// but "it was a 422 *about the password being too short*".
    pub fn assert_field_error(&self, pointer: &str, code: &str) -> &Self {
        let problem = match self.try_json::<Problem>() {
            Ok(problem) => problem,
            Err(error) => self.fail(
                &format!("expected a field error at `{pointer}` with code `{code}`"),
                &[("serde error", error.to_string())],
            ),
        };
        let fields = problem.errors.unwrap_or_default();
        if fields
            .iter()
            .any(|field| field.pointer == pointer && field.code == code)
        {
            return self;
        }
        let listing = if fields.is_empty() {
            "(the problem carries no field errors)".to_owned()
        } else {
            fields
                .iter()
                .map(|field| format!("{} {} — {}", field.pointer, field.code, field.message))
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.fail(
            &format!("expected a field error at `{pointer}` with code `{code}`"),
            &[("field errors present", listing)],
        )
    }

    // ── contract ──────────────────────────────────────────────────────────

    /// Assert the body matches the OpenAPI schema documented for this operation.
    ///
    /// This is the contract test. It catches the class of bug no other assertion
    /// does: a handler and a document that have drifted apart, which produces a
    /// green suite and a broken client.
    ///
    /// Strict by default — a property the schema does not describe is drift. See
    /// [`crate::contract`] for why, and use
    /// [`assert_matches_openapi_with`](Self::assert_matches_openapi_with) with
    /// [`ContractOptions::lax`] for literal JSON Schema semantics.
    pub fn assert_matches_openapi(&self) -> &Self {
        self.assert_matches_openapi_with(ContractOptions::strict())
    }

    /// [`assert_matches_openapi`](Self::assert_matches_openapi) with explicit
    /// strictness.
    pub fn assert_matches_openapi_with(&self, options: ContractOptions) -> &Self {
        let path = self.request_path();
        let method: HttpMethod = match self.request.method.parse() {
            Ok(method) => method,
            Err(_) => self.fail(
                &format!(
                    "`{}` is not a method the OpenAPI document can describe",
                    self.request.method
                ),
                &[],
            ),
        };

        let Some(template) = match_template(self.openapi(), &path) else {
            self.fail(
                &format!("the OpenAPI document has no path matching `{path}`"),
                &[("documented paths", self.documented_paths())],
            )
        };
        let Some(operation) = self.openapi().operation(method, template) else {
            self.fail(
                &format!(
                    "the OpenAPI document describes `{template}` but not `{} {template}`",
                    self.request.method
                ),
                &[("documented methods", self.documented_methods(template))],
            )
        };

        let status = self.status.as_u16();
        let Some((key, spec)) = response_for(operation, status) else {
            self.fail(
                &format!(
                    "the operation `{} {template}` does not document a {status} response",
                    self.request.method
                ),
                &[(
                    "documented responses",
                    operation
                        .responses
                        .keys()
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                )],
            )
        };

        let content_type = self.header("content-type").unwrap_or_default();
        if spec.content.is_empty() {
            if self.body.is_empty() {
                return self;
            }
            self.fail(
                &format!(
                    "the operation documents a {key} response with no content, but the body has {} bytes",
                    self.body.len()
                ),
                &[],
            );
        }
        let Some((media_key, media)) = media_type_for(spec, content_type) else {
            self.fail(
                &format!("the {key} response does not document the content type {content_type:?}"),
                &[(
                    "documented content types",
                    spec.content.keys().cloned().collect::<Vec<_>>().join(", "),
                )],
            )
        };

        // A documented non-JSON body is a contract we can check the *type* of
        // and nothing more; asserting a JSON Schema over a PNG would be theatre.
        if !report::is_json_like(media_key) {
            return self;
        }
        let Some(schema) = &media.schema else {
            return self;
        };
        let value = match self.try_json::<Value>() {
            Ok(value) => value,
            Err(error) => self.fail(
                &format!(
                    "the {key} response is documented as {media_key} but the body is not JSON"
                ),
                &[("serde error", error.to_string())],
            ),
        };

        let violations = crate::contract::validate(self.openapi(), schema, &value, options);
        if violations.is_empty() {
            return self;
        }
        self.fail(
            &format!(
                "the body does not match the schema documented for {} {template} → {key}",
                self.request.method
            ),
            &[
                ("contract violations", crate::contract::render(&violations)),
                (
                    "documented schema",
                    serde_json::to_string_pretty(schema).unwrap_or_default(),
                ),
            ],
        )
    }

    // ── failure rendering ─────────────────────────────────────────────────

    /// The full report, without panicking. Exposed so a test that wants to
    /// assert *about* the harness can read what it would have printed.
    #[must_use]
    pub fn report(&self, headline: &str, extra: &[(&str, String)]) -> String {
        let mut out = report::rule("moso-test: assertion failed");
        let _ = writeln!(out, "  {headline}");
        out.push('\n');
        out.push_str(&self.request.render());

        let summary = format!(
            "{} {}  ({}, {})",
            self.status.as_u16(),
            self.status.canonical_reason().unwrap_or(""),
            humanise(self.elapsed),
            self.request.transport,
        );
        out.push_str(&report::section("response", &summary));
        out.push_str(&report::section(
            "response headers",
            &report::pairs(
                &self
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_owned(),
                            value.to_str().unwrap_or("<non-ascii>").to_owned(),
                        )
                    })
                    .collect::<Vec<_>>(),
            ),
        ));
        out.push_str(&report::section(
            "response body",
            &report::body(&self.body, self.header("content-type")),
        ));

        for (title, body) in extra {
            out.push_str(&report::section(title, body));
        }

        let lines = self.logs.for_request(&self.request_id);
        let rendered = if self.logs.is_capturing() {
            crate::logs::render_records(&lines)
        } else {
            "log capture is unavailable: another global `tracing` subscriber was \
             installed before the first TestApp was spawned."
                .to_owned()
        };
        out.push_str(&report::section(
            &format!("server logs for request_id {}", self.request_id),
            &rendered,
        ));
        out.push_str(&report::rule_end());
        out
    }

    fn fail(&self, headline: &str, extra: &[(&str, String)]) -> ! {
        panic!("{}", self.report(headline, extra));
    }

    /// The path the request was sent to, without the query string.
    fn request_path(&self) -> String {
        let path = self
            .request
            .url
            .split_once('?')
            .map_or(self.request.url.as_str(), |(head, _)| head);
        // Strip scheme and authority; what remains starts at the first `/`.
        match path.find("://") {
            Some(index) => match path[index + 3..].find('/') {
                Some(offset) => path[index + 3 + offset..].to_owned(),
                None => "/".to_owned(),
            },
            None => path.to_owned(),
        }
    }

    fn documented_paths(&self) -> String {
        self.openapi()
            .paths
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn documented_methods(&self, template: &str) -> String {
        self.openapi()
            .path_item(template)
            .map(|item| {
                item.operations()
                    .map(|(method, _)| method.as_upper_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    }
}

impl core::fmt::Debug for TestResponse {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestResponse")
            .field("status", &self.status.as_u16())
            .field("bytes", &self.body.len())
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Document lookup
// ---------------------------------------------------------------------------

/// The documented path template matching a concrete request path.
///
/// Exact match first, so a literal `/users/me` wins over `/users/{id}` — which
/// is also the order the router resolves them in.
#[must_use]
pub fn match_template<'a>(document: &'a Document, path: &str) -> Option<&'a str> {
    let path = normalise(path);
    if let Some((template, _)) = document.paths.get_key_value(path.as_str()) {
        return Some(template.as_str());
    }
    document
        .paths
        .keys()
        .find(|template| template_matches(template, &path))
        .map(String::as_str)
}

/// Whether an OpenAPI path template matches a concrete path.
#[must_use]
pub fn template_matches(template: &str, path: &str) -> bool {
    let template_segments: Vec<&str> = template.trim_matches('/').split('/').collect();
    let path_segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    for (index, expected) in template_segments.iter().enumerate() {
        // `{*rest}` is Axum 0.8's wildcard: it swallows everything left.
        if expected.starts_with("{*") && expected.ends_with('}') {
            return path_segments.len() > index;
        }
        let Some(actual) = path_segments.get(index) else {
            return false;
        };
        let is_parameter = expected.starts_with('{') && expected.ends_with('}');
        if is_parameter {
            // A parameter matches one non-empty segment.
            if actual.is_empty() {
                return false;
            }
            continue;
        }
        if expected != actual {
            return false;
        }
    }
    template_segments.len() == path_segments.len()
}

/// The documented response for a status: exact, then `4XX`, then `default`.
#[must_use]
pub fn response_for(
    operation: &Operation,
    status: u16,
) -> Option<(&str, &moso::openapi::Response)> {
    let exact = status.to_string();
    let wildcard = format!("{}XX", status / 100);
    for key in [exact.as_str(), wildcard.as_str(), "default"] {
        if let Some((key, spec)) = operation.responses.get_key_value(key) {
            return Some((key.as_str(), spec));
        }
    }
    // Some generators write `4xx` in lower case.
    let lower = wildcard.to_ascii_lowercase();
    operation
        .responses
        .get_key_value(lower.as_str())
        .map(|(key, spec)| (key.as_str(), spec))
}

/// The documented media type for a response's `Content-Type`.
///
/// Matches on the essence — `application/json; charset=utf-8` is
/// `application/json` — then falls back to a lone documented entry, which is the
/// common case for a handler whose response carries no content type at all.
#[must_use]
pub fn media_type_for<'a>(
    response: &'a moso::openapi::Response,
    content_type: &str,
) -> Option<(&'a str, &'a MediaType)> {
    let essence = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    if let Some((key, media)) = response.content.get_key_value(essence.as_str()) {
        return Some((key.as_str(), media));
    }
    if let Some((key, media)) = response
        .content
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(&essence))
    {
        return Some((key.as_str(), media));
    }
    if response.content.len() == 1 {
        return response
            .content
            .iter()
            .next()
            .map(|(key, media)| (key.as_str(), media));
    }
    None
}

/// A path with its trailing slash removed, except at the root.
fn normalise(path: &str) -> String {
    if path.len() > 1 && path.ends_with('/') {
        return path.trim_end_matches('/').to_owned();
    }
    if path.is_empty() {
        "/".to_owned()
    } else {
        path.to_owned()
    }
}

/// A duration as a test reader would say it.
fn humanise(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{:.1} ms", duration.as_secs_f64() * 1000.0)
    } else {
        format!("{:.2} s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso::openapi::{Document, Info, Operation, PathItem, Response as ApiResponse};

    fn document_with(paths: &[&str]) -> Document {
        let mut document = Document::new(Info::new("Test", "1"));
        for path in paths {
            document
                .paths
                .insert((*path).to_owned(), PathItem::default());
        }
        document
    }

    #[test]
    fn an_exact_path_wins_over_a_template() {
        let document = document_with(&["/users/{id}", "/users/me"]);
        assert_eq!(match_template(&document, "/users/me"), Some("/users/me"));
        assert_eq!(match_template(&document, "/users/7"), Some("/users/{id}"));
    }

    #[test]
    fn a_trailing_slash_still_matches() {
        let document = document_with(&["/users"]);
        assert_eq!(match_template(&document, "/users/"), Some("/users"));
    }

    #[test]
    fn an_unmatched_path_is_none() {
        let document = document_with(&["/users/{id}"]);
        assert_eq!(match_template(&document, "/posts/7"), None);
        assert_eq!(match_template(&document, "/users/7/posts"), None);
    }

    #[test]
    fn a_template_needs_the_same_segment_count() {
        assert!(template_matches("/users/{id}", "/users/7"));
        assert!(!template_matches("/users/{id}", "/users"));
        assert!(!template_matches("/users/{id}", "/users/7/posts"));
    }

    #[test]
    fn a_wildcard_segment_swallows_the_rest() {
        assert!(template_matches("/assets/{*path}", "/assets/css/app.css"));
        assert!(template_matches("/assets/{*path}", "/assets/logo.png"));
        assert!(!template_matches("/assets/{*path}", "/assets"));
    }

    #[test]
    fn the_root_path_matches_itself() {
        let document = document_with(&["/"]);
        assert_eq!(match_template(&document, "/"), Some("/"));
    }

    #[test]
    fn a_response_falls_back_from_exact_to_wildcard_to_default() {
        let mut operation = Operation::default();
        operation
            .responses
            .insert("200".to_owned(), ApiResponse::new("ok"));
        operation
            .responses
            .insert("4XX".to_owned(), ApiResponse::new("client error"));
        operation
            .responses
            .insert("default".to_owned(), ApiResponse::new("anything"));

        assert_eq!(
            response_for(&operation, 200).map(|(key, _)| key),
            Some("200")
        );
        assert_eq!(
            response_for(&operation, 404).map(|(key, _)| key),
            Some("4XX")
        );
        assert_eq!(
            response_for(&operation, 500).map(|(key, _)| key),
            Some("default")
        );
    }

    #[test]
    fn a_lower_case_wildcard_key_is_still_found() {
        let mut operation = Operation::default();
        operation
            .responses
            .insert("4xx".to_owned(), ApiResponse::new("client error"));
        assert_eq!(
            response_for(&operation, 422).map(|(key, _)| key),
            Some("4xx")
        );
    }

    #[test]
    fn an_undocumented_status_is_none() {
        let mut operation = Operation::default();
        operation
            .responses
            .insert("200".to_owned(), ApiResponse::new("ok"));
        assert!(response_for(&operation, 404).is_none());
    }

    #[test]
    fn a_media_type_matches_on_its_essence() {
        let mut response = ApiResponse::new("ok");
        response
            .content
            .insert("application/json".to_owned(), MediaType::default());
        assert_eq!(
            media_type_for(&response, "application/json; charset=utf-8").map(|(key, _)| key),
            Some("application/json")
        );
    }

    #[test]
    fn a_lone_documented_media_type_is_used_when_the_header_is_missing() {
        let mut response = ApiResponse::new("ok");
        response
            .content
            .insert("application/problem+json".to_owned(), MediaType::default());
        assert_eq!(
            media_type_for(&response, "").map(|(key, _)| key),
            Some("application/problem+json")
        );
    }

    #[test]
    fn an_ambiguous_content_type_is_not_guessed() {
        let mut response = ApiResponse::new("ok");
        response
            .content
            .insert("application/json".to_owned(), MediaType::default());
        response
            .content
            .insert("text/csv".to_owned(), MediaType::default());
        assert!(media_type_for(&response, "application/xml").is_none());
    }

    #[test]
    fn a_status_is_named_by_a_number_or_by_its_constant() {
        assert_eq!(201u16.into_status(), StatusCode::CREATED);
        assert_eq!(StatusCode::CREATED.into_status(), StatusCode::CREATED);
    }

    #[test]
    fn normalising_leaves_the_root_alone() {
        assert_eq!(normalise("/"), "/");
        assert_eq!(normalise(""), "/");
        assert_eq!(normalise("/a/"), "/a");
        assert_eq!(normalise("/a"), "/a");
    }

    #[test]
    fn durations_are_rendered_with_a_unit() {
        assert!(humanise(Duration::from_micros(1500)).ends_with(" ms"));
        assert!(humanise(Duration::from_secs(2)).ends_with(" s"));
    }
}
