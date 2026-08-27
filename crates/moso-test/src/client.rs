//! Driving the application under test.
//!
//! # Two transports, one API
//!
//! [`TestClient`] speaks to the application either **in process** — calling the
//! composed `tower::Service` directly, with no socket, no TCP handshake and no
//! second runtime — or over a **real bound port** with `reqwest`. The builder
//! chooses; the assertions do not change either way.
//!
//! In-process is the default because it is roughly an order of magnitude faster
//! and because it removes an entire class of flake: there is no port to be
//! taken, no accept queue to be full, no connection to be reset. The socket
//! transport exists for the tests that genuinely need the wire — HTTP/2, a
//! client library under test, `Connection: close` behaviour, an external process
//! calling in — and it exercises exactly the same middleware stack.
//!
//! # The chain
//!
//! ```
//! use moso_test::prelude::*;
//! # /// A user, as the API accepts one.
//! # #[derive(moso::Schema)] pub struct CreateUser {
//! #     /// Public handle.
//! #     #[schema(len = 3..=32)] pub username: String,
//! #     /// Contact address.
//! #     pub email: moso::schema::Email }
//! # /// A user, as the API returns one.
//! # #[derive(moso::Schema)] pub struct UserOut {
//! #     /// Stable identifier.
//! #     pub id: u64,
//! #     /// Public handle.
//! #     pub username: String }
//! # /// Everything this application reads from its environment.
//! # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
//! #     /// Service name.
//! #     #[config(default = "users")] pub name: String }
//! # /// Create a user.
//! # #[moso::endpoint]
//! # async fn create(moso::extract::Json(body): moso::extract::Json<CreateUser>)
//! #     -> moso::Result<moso::response::Created<UserOut>>
//! # {
//! #     Ok(moso::response::Created::at(
//! #         "/users/1",
//! #         UserOut { id: 1, username: body.username },
//! #     ))
//! # }
//! # /// The composition root every Moso application exposes.
//! # fn app() -> moso::AppBuilder {
//! #     moso::App::new(AppConfig { name: "users".to_owned() })
//! #         .mount(moso::routes! { POST "/users" => create })
//! # }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() -> moso::Result<()> {
//! # let app = TestApp::builder().app(app()).spawn().await?;
//! let response = app.client()
//!     .post("/users")
//!     .json(&CreateUser {
//!         username: "ada".to_owned(),
//!         email: "ada@example.com".parse().unwrap(),
//!     })
//!     .bearer("test-token")
//!     .send()
//!     .await;
//!
//! response.assert_status(201);
//! # Ok(())
//! # }
//! ```
//!
//! [`RequestBuilder::send`] does not return a `Result`. A transport failure in a
//! test is a test failure, and it is reported the same way an assertion failure
//! is: with the request, the response and the server's own log lines. Use
//! [`RequestBuilder::try_send`] when the failure is what the test is about.
//!
//! # Redirects are never followed
//!
//! Neither transport follows a `3xx`, and there is no `follow_redirects()`.
//! Following one would mean the two transports disagreeing — the in-process path
//! has no client library to do it — and it would mean a test that meant to
//! assert "this returns a 302 to `/login`" quietly asserting something about
//! `/login` instead. Assert on the status and the `Location` header, then issue
//! the second request yourself.
//!
//! # Streaming bodies
//!
//! Every response is buffered before it is handed back, which is what lets a
//! failure five lines later still print it. Server-sent events and WebSocket
//! upgrades therefore have no client here; a test for either should drive
//! [`TestApp::service`](crate::TestApp::service) or the bound port directly.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http::header::{HeaderName, HeaderValue};
use http::{HeaderMap, Method, StatusCode};
use moso::AppState;
use moso::openapi::Document;
use moso::response::sse::Event;
use serde::Serialize;
use url::Url;

use crate::contract::Options as ContractOptions;
use crate::logs::LogAssertions;
use crate::report::RequestRecord;
use crate::response::TestResponse;

/// How a [`TestClient`] reaches the application.
#[derive(Clone)]
pub(crate) enum Transport {
    /// The composed `tower::Service`, called directly.
    InProcess(Box<moso::deps::axum::Router<()>>),
    /// A real socket on `127.0.0.1`.
    #[cfg(feature = "server")]
    Socket(reqwest::Client),
}

impl Transport {
    /// The word the failure report uses for this transport.
    pub(crate) fn label(&self) -> &'static str {
        match self {
            Transport::InProcess(_) => "in-process",
            #[cfg(feature = "server")]
            Transport::Socket(_) => "socket",
        }
    }
}

impl core::fmt::Debug for Transport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// Everything a client needs, shared by every clone and every request.
pub(crate) struct ClientInner {
    pub(crate) transport: Transport,
    pub(crate) base_url: Url,
    pub(crate) state: Arc<AppState>,
    pub(crate) logs: LogAssertions,
    pub(crate) app: u64,
    /// Headers added to every request unless the request overrides them.
    pub(crate) headers: HeaderMap,
    /// Cookies added to every request, merged with the request's own.
    pub(crate) cookies: Vec<(String, String)>,
    /// When set, every response is checked against the OpenAPI document.
    pub(crate) contract: Option<ContractOptions>,
    /// A per-request deadline, off by default.
    pub(crate) timeout: Option<Duration>,
}

/// An HTTP client bound to one [`TestApp`](crate::TestApp).
///
/// Cheap to clone, and every "give me a client with X" method returns a *new*
/// client rather than mutating this one — so `app.client()` is always the
/// unauthenticated baseline no matter what a previous line did.
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
/// let client = app.client();
///
/// // Each "give me a client with X" returns a new one, so the baseline is intact.
/// let authed = client.with_bearer("test-token");
/// let _ = &client;
///
/// authed
///     .post("/users")
///     .json(&serde_json::json!({ "username": "ada" }))
///     .send()
///     .await
///     .assert_status(201);
/// # Ok(())
/// # }
/// ```
///
/// Speaks either in-process (the default: no socket, no second runtime) or over a
/// real bound port. The assertions do not change either way.
#[derive(Clone)]
pub struct TestClient {
    inner: Arc<ClientInner>,
}

impl TestClient {
    pub(crate) fn new(inner: ClientInner) -> Self {
        Self {
            inner: Arc::new(inner),
        }
    }

    /// The URL requests are addressed to.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.inner.base_url
    }

    /// The headers this client adds to every request.
    #[must_use]
    pub fn headers(&self) -> &HeaderMap {
        &self.inner.headers
    }

    /// The OpenAPI document the application published.
    #[must_use]
    pub fn openapi(&self) -> &Document {
        self.inner.state.document()
    }

    /// The frozen application state.
    #[must_use]
    pub fn state(&self) -> &Arc<AppState> {
        &self.inner.state
    }

    /// The captured server logs, so an assertion can quote them.
    #[must_use]
    pub fn logs(&self) -> &LogAssertions {
        &self.inner.logs
    }

    // ── derived clients ───────────────────────────────────────────────────

    /// A copy of this client with `name: value` added to every request.
    #[must_use]
    pub fn with_header(&self, name: &str, value: &str) -> Self {
        let mut inner = self.fork();
        insert_header(&mut inner.headers, name, value);
        Self::new(inner)
    }

    /// A copy of this client sending `Authorization: Bearer <token>`.
    #[must_use]
    pub fn with_bearer(&self, token: &str) -> Self {
        self.with_header("authorization", &format!("Bearer {token}"))
    }

    /// A copy of this client sending an extra cookie.
    #[must_use]
    pub fn with_cookie(&self, name: &str, value: &str) -> Self {
        let mut inner = self.fork();
        inner.cookies.push((name.to_owned(), value.to_owned()));
        Self::new(inner)
    }

    /// A copy of this client with no credentials at all.
    ///
    /// Drops `Authorization`, `Cookie` and every cookie this client accumulated
    /// — the "and now as a stranger" half of an authorisation test.
    #[must_use]
    pub fn anonymous(&self) -> Self {
        let mut inner = self.fork();
        inner.headers.remove(http::header::AUTHORIZATION);
        inner.headers.remove(http::header::COOKIE);
        inner.cookies.clear();
        Self::new(inner)
    }

    /// A copy of this client that checks every response against the document.
    ///
    /// The global switch `43-testing.md` describes as
    /// `[test] assert_openapi = true`: it makes every test a contract test.
    #[must_use]
    pub fn asserting_openapi(&self, options: Option<ContractOptions>) -> Self {
        let mut inner = self.fork();
        inner.contract = options;
        Self::new(inner)
    }

    /// A copy of this client with a deadline on every request.
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        let mut inner = self.fork();
        inner.timeout = Some(timeout);
        Self::new(inner)
    }

    fn fork(&self) -> ClientInner {
        ClientInner {
            transport: self.inner.transport.clone(),
            base_url: self.inner.base_url.clone(),
            state: Arc::clone(&self.inner.state),
            logs: self.inner.logs.clone(),
            app: self.inner.app,
            headers: self.inner.headers.clone(),
            cookies: self.inner.cookies.clone(),
            contract: self.inner.contract,
            timeout: self.inner.timeout,
        }
    }

    // ── requests ──────────────────────────────────────────────────────────

    /// Start a request with an arbitrary method.
    #[must_use]
    pub fn request(&self, method: Method, path: &str) -> RequestBuilder {
        RequestBuilder::new(self.clone(), method, path)
    }

    /// `GET path`.
    #[must_use]
    pub fn get(&self, path: &str) -> RequestBuilder {
        self.request(Method::GET, path)
    }

    /// `POST path`.
    #[must_use]
    pub fn post(&self, path: &str) -> RequestBuilder {
        self.request(Method::POST, path)
    }

    /// `PUT path`.
    #[must_use]
    pub fn put(&self, path: &str) -> RequestBuilder {
        self.request(Method::PUT, path)
    }

    /// `PATCH path`.
    #[must_use]
    pub fn patch(&self, path: &str) -> RequestBuilder {
        self.request(Method::PATCH, path)
    }

    /// `DELETE path`.
    #[must_use]
    pub fn delete(&self, path: &str) -> RequestBuilder {
        self.request(Method::DELETE, path)
    }

    /// `HEAD path`.
    #[must_use]
    pub fn head(&self, path: &str) -> RequestBuilder {
        self.request(Method::HEAD, path)
    }

    /// `OPTIONS path`.
    #[must_use]
    pub fn options(&self, path: &str) -> RequestBuilder {
        self.request(Method::OPTIONS, path)
    }
}

impl core::fmt::Debug for TestClient {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestClient")
            .field("base_url", &self.inner.base_url.as_str())
            .field("transport", &self.inner.transport)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// RequestBuilder
// ---------------------------------------------------------------------------

/// One request under construction.
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
/// let response = app.client()
///     .post("/users")
///     .header("x-request-id", "test-1")
///     .json(&serde_json::json!({ "username": "ada" }))
///     .send()
///     .await;
///
/// response.assert_status(201);
/// # Ok(())
/// # }
/// ```
///
/// [`RequestBuilder::send`] returns a [`TestResponse`], not a
/// `Result`: a transport failure in a test is a test failure, and it is reported
/// with the request, the response and the server's own log lines. Use
/// [`RequestBuilder::try_send`] when the failure is the thing being tested.
pub struct RequestBuilder {
    client: TestClient,
    method: Method,
    path: String,
    query: Vec<(String, String)>,
    headers: HeaderMap,
    cookies: Vec<(String, String)>,
    body: Option<Bytes>,
    timeout: Option<Duration>,
    contract: Option<ContractOptions>,
    request_id: String,
}

impl RequestBuilder {
    fn new(client: TestClient, method: Method, path: &str) -> Self {
        let contract = client.inner.contract;
        let timeout = client.inner.timeout;
        Self {
            client,
            method,
            path: path.to_owned(),
            query: Vec::new(),
            headers: HeaderMap::new(),
            cookies: Vec::new(),
            body: None,
            timeout,
            contract,
            request_id: next_request_id(),
        }
    }

    /// Set a header, replacing any the client supplies.
    #[must_use]
    pub fn header(mut self, name: &str, value: &str) -> Self {
        insert_header(&mut self.headers, name, value);
        self
    }

    /// Send `Authorization: Bearer <token>`.
    #[must_use]
    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }

    /// Add one cookie, merged with the client's.
    #[must_use]
    pub fn cookie(mut self, name: &str, value: &str) -> Self {
        self.cookies.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Add one query-string pair.
    #[must_use]
    pub fn query_pair(mut self, name: &str, value: &str) -> Self {
        self.query.push((name.to_owned(), value.to_owned()));
        self
    }

    /// Add every pair of a serialisable value to the query string.
    ///
    /// Panics if the value cannot be form-encoded — a struct with a nested map,
    /// say. That is a mistake in the test, not a condition to handle.
    #[must_use]
    pub fn query<T: Serialize + ?Sized>(mut self, params: &T) -> Self {
        let encoded = serde_urlencoded::to_string(params).unwrap_or_else(|error| {
            panic!("moso-test: the query parameters do not form-encode: {error}")
        });
        for (name, value) in form_urlencoded_pairs(&encoded) {
            self.query.push((name, value));
        }
        self
    }

    /// Send `value` as a JSON body, setting `Content-Type: application/json`.
    #[must_use]
    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Self {
        let bytes = serde_json::to_vec(value).unwrap_or_else(|error| {
            panic!("moso-test: the request body does not serialise: {error}")
        });
        self.body = Some(Bytes::from(bytes));
        insert_header(&mut self.headers, "content-type", "application/json");
        self
    }

    /// Send a `serde_json::Value` body verbatim.
    ///
    /// The way to test what happens to a body a typed struct could not express:
    /// a missing field, an extra one, the wrong type.
    #[must_use]
    pub fn json_value(self, value: serde_json::Value) -> Self {
        self.json(&value)
    }

    /// Send `value` form-encoded, as `application/x-www-form-urlencoded`.
    #[must_use]
    pub fn form<T: Serialize + ?Sized>(mut self, value: &T) -> Self {
        let encoded = serde_urlencoded::to_string(value)
            .unwrap_or_else(|error| panic!("moso-test: the form body does not encode: {error}"));
        self.body = Some(Bytes::from(encoded));
        insert_header(
            &mut self.headers,
            "content-type",
            "application/x-www-form-urlencoded",
        );
        self
    }

    /// Send a multipart body.
    #[must_use]
    pub fn multipart(mut self, form: Multipart) -> Self {
        insert_header(&mut self.headers, "content-type", &form.content_type());
        self.body = Some(form.into_body());
        self
    }

    /// Send raw bytes, with no `Content-Type` unless one was set.
    #[must_use]
    pub fn body(mut self, bytes: impl Into<Bytes>) -> Self {
        self.body = Some(bytes.into());
        self
    }

    /// Send a text body as `text/plain; charset=utf-8`.
    #[must_use]
    pub fn text(mut self, text: impl Into<String>) -> Self {
        self.body = Some(Bytes::from(text.into()));
        insert_header(
            &mut self.headers,
            "content-type",
            "text/plain; charset=utf-8",
        );
        self
    }

    /// Fail the request if it takes longer than `timeout`.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Override the correlation id this request is sent with.
    ///
    /// The harness generates one per request and uses it to pick out the server
    /// log lines belonging to this request; set it only when the *value* is what
    /// the test is about.
    #[must_use]
    pub fn request_id(mut self, id: impl Into<String>) -> Self {
        self.request_id = id.into();
        self
    }

    /// Check (or stop checking) this response against the OpenAPI document.
    #[must_use]
    pub fn asserting_openapi(mut self, options: Option<ContractOptions>) -> Self {
        self.contract = options;
        self
    }

    /// The URL this request will be sent to.
    ///
    /// Available before sending so a test can assert on how a path and its query
    /// parameters were assembled.
    #[must_use]
    pub fn url(&self) -> Url {
        let mut url = self
            .client
            .inner
            .base_url
            .join(&self.path)
            .unwrap_or_else(|error| {
                panic!(
                    "moso-test: {:?} is not a path this client can address ({error})",
                    self.path
                )
            });
        if !self.query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (name, value) in &self.query {
                pairs.append_pair(name, value);
            }
        }
        url
    }

    /// Send the request, failing the test if the transport cannot.
    pub async fn send(self) -> TestResponse {
        let logs = self.client.inner.logs.clone();
        match self.try_send().await {
            Ok(response) => response,
            Err(failure) => panic!("{}", failure.render(&logs)),
        }
    }

    /// Send the request, returning the transport error instead of panicking.
    ///
    /// # Errors
    ///
    /// A connection failure, a timeout, or a response the harness could not read.
    pub async fn try_send(self) -> Result<TestResponse, Box<SendFailure>> {
        let url = self.url();
        let mut headers = self.client.inner.headers.clone();
        for (name, value) in self.headers.iter() {
            headers.insert(name.clone(), value.clone());
        }
        let cookies: Vec<(String, String)> = self
            .client
            .inner
            .cookies
            .iter()
            .cloned()
            .chain(self.cookies.iter().cloned())
            .collect();
        if !cookies.is_empty() {
            let jar = cookies
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join("; ");
            insert_header(&mut headers, "cookie", &jar);
        }
        insert_header(&mut headers, moso::REQUEST_ID_HEADER, &self.request_id);
        if let Some(body) = &self.body {
            headers.insert(
                http::header::CONTENT_LENGTH,
                HeaderValue::from(body.len() as u64),
            );
        }

        let record = RequestRecord {
            method: self.method.as_str().to_owned(),
            url: url.to_string(),
            headers: headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().to_owned(),
                        value.to_str().unwrap_or("<non-ascii>").to_owned(),
                    )
                })
                .collect(),
            body: self.body.clone(),
            transport: self.client.inner.transport.label(),
        };

        // Claimed *before* the request goes out, so a log line produced on a
        // server task is attributable the instant it is emitted.
        crate::logs::claim_request(self.client.inner.app, &self.request_id);

        let started = Instant::now();
        let sent = self.dispatch(&url, headers).await;
        let elapsed = started.elapsed();

        match sent {
            Ok((status, headers, body)) => {
                let response = TestResponse::new(
                    record,
                    status,
                    headers,
                    body,
                    elapsed,
                    self.request_id.clone(),
                    Arc::clone(&self.client.inner.state),
                    self.client.inner.logs.clone(),
                );
                if let Some(options) = self.contract {
                    response.assert_matches_openapi_with(options);
                }
                Ok(response)
            }
            Err(message) => Err(Box::new(SendFailure {
                request: record,
                message,
                elapsed,
                request_id: self.request_id.clone(),
            })),
        }
    }

    /// Send the request and read the response as a server-sent event stream.
    ///
    /// The request is sent like any other and its `text/event-stream` body is
    /// parsed into [`Event`]s — one per dispatched frame, keep-alive comment
    /// frames skipped — so a test can assert on what the handler streamed.
    ///
    /// # The stream must end
    ///
    /// The whole body is read before it is parsed, so this waits for the stream
    /// to *close*. A production SSE endpoint often streams forever; a test one
    /// must terminate — a bounded stream, or one that selects on
    /// `Inject<Signal>` — or set [`timeout`](Self::timeout) so a stuck stream
    /// fails the test instead of hanging it.
    ///
    /// # Panics
    ///
    /// If the request could not be sent, with the same report
    /// [`send`](Self::send) prints.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::response::sse::{Event, Sse};
    /// use moso_test::prelude::*;
    /// use futures_util::stream;
    /// use std::pin::Pin;
    /// # /// Everything this application reads from its environment.
    /// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Service name.
    /// #     #[config(default = "clock")] pub name: String }
    ///
    /// /// A concretely-named stream, so it can appear in a signature.
    /// pub type Events = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;
    ///
    /// /// Stream two ticks, then close.
    /// #[moso::endpoint]
    /// async fn ticks() -> Result<Sse<Events>> {
    ///     let events = stream::iter([
    ///         Ok(Event::data("one").named("tick")),
    ///         Ok(Event::data("two").named("tick")),
    ///     ]);
    ///     Ok(Sse::new(Box::pin(events) as Events))
    /// }
    ///
    /// /// The composition root every Moso application exposes.
    /// fn app() -> moso::AppBuilder {
    ///     moso::App::new(AppConfig { name: "clock".to_owned() })
    ///         .mount(moso::routes! { GET "/ticks" => ticks })
    /// }
    ///
    /// # use futures_util::Stream;
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso::Result<()> {
    /// let app = TestApp::builder().app(app()).spawn().await?;
    ///
    /// let stream = app.client().get("/ticks").sse().await;
    /// stream
    ///     .assert_status(200)
    ///     .assert_event_count(2)
    ///     .assert_data_contains("two");
    /// assert_eq!(stream.events()[0].name.as_deref(), Some("tick"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn sse(self) -> SseResponse {
        let response = self.send().await;
        let events = parse_event_stream(response.body());
        SseResponse { response, events }
    }

    /// Hand the assembled request to whichever transport this client has.
    async fn dispatch(
        &self,
        url: &Url,
        headers: HeaderMap,
    ) -> Result<(http::StatusCode, HeaderMap, Bytes), String> {
        let work = self.dispatch_inner(url, headers);
        match self.timeout {
            Some(limit) => match tokio::time::timeout(limit, work).await {
                Ok(result) => result,
                Err(_) => Err(format!(
                    "the request did not complete within {}",
                    humanise(limit)
                )),
            },
            None => work.await,
        }
    }

    async fn dispatch_inner(
        &self,
        url: &Url,
        headers: HeaderMap,
    ) -> Result<(http::StatusCode, HeaderMap, Bytes), String> {
        match &self.client.inner.transport {
            Transport::InProcess(router) => {
                use tower::ServiceExt as _;

                let target = match url.query() {
                    Some(query) => format!("{}?{query}", url.path()),
                    None => url.path().to_owned(),
                };
                let mut request = http::Request::builder()
                    .method(self.method.clone())
                    .uri(target);
                if let Some(host) = url.host_str() {
                    let authority = match url.port() {
                        Some(port) => format!("{host}:{port}"),
                        None => host.to_owned(),
                    };
                    request = request.header(http::header::HOST, authority);
                }
                {
                    let slot = request.headers_mut().expect("the builder is well formed");
                    for (name, value) in headers.iter() {
                        slot.insert(name.clone(), value.clone());
                    }
                }
                let body = self.body.clone().unwrap_or_default();
                let request = request
                    .body(moso::deps::axum::body::Body::from(body))
                    .map_err(|error| format!("the request could not be built: {error}"))?;

                let response = router
                    .as_ref()
                    .clone()
                    .oneshot(request)
                    .await
                    .map_err(|error| format!("the service failed: {error}"))?;
                let status = response.status();
                let headers = response.headers().clone();
                let bytes = moso::deps::axum::body::to_bytes(response.into_body(), usize::MAX)
                    .await
                    .map_err(|error| format!("the response body could not be read: {error}"))?;
                Ok((status, headers, bytes))
            }
            #[cfg(feature = "server")]
            Transport::Socket(client) => {
                let mut request = client
                    .request(self.method.clone(), url.clone())
                    .headers(headers);
                if let Some(body) = self.body.clone() {
                    request = request.body(body);
                }
                let response = request
                    .send()
                    .await
                    .map_err(|error| format!("the request failed: {error}"))?;
                let status = response.status();
                let headers = response.headers().clone();
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|error| format!("the response body could not be read: {error}"))?;
                Ok((status, headers, bytes))
            }
        }
    }
}

impl core::fmt::Debug for RequestBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RequestBuilder")
            .field("method", &self.method.as_str())
            .field("path", &self.path)
            .field("request_id", &self.request_id)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// SendFailure
// ---------------------------------------------------------------------------

/// A request that never produced a response.
#[derive(Clone, Debug)]
pub struct SendFailure {
    /// What was being sent.
    pub request: RequestRecord,
    /// Why it failed, in the transport's own words.
    pub message: String,
    /// How long it took to fail.
    pub elapsed: Duration,
    /// The correlation id the request carried.
    pub request_id: String,
}

impl SendFailure {
    /// The full report, log lines included.
    #[must_use]
    pub fn render(&self, logs: &LogAssertions) -> String {
        let mut out = crate::report::rule("moso-test: the request could not be sent");
        out.push_str(&format!("  {}\n\n", self.message));
        out.push_str(&self.request.render());
        let lines = logs.for_request(&self.request_id);
        out.push_str(&crate::report::section(
            &format!("server logs for request_id {}", self.request_id),
            &crate::logs::render_records(&lines),
        ));
        out.push_str(&crate::report::rule_end());
        out
    }
}

impl core::fmt::Display for SendFailure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.request.method, self.request.url, self.message
        )
    }
}

impl std::error::Error for SendFailure {}

// ---------------------------------------------------------------------------
// Server-sent events
// ---------------------------------------------------------------------------

/// A consumed `text/event-stream` response, and assertions over its events.
///
/// Returned by [`RequestBuilder::sse`]. It holds the whole underlying
/// [`TestResponse`] as well as the parsed [`Event`]s, so a test can assert on
/// the status and headers the same way it would for any response and on the
/// stream on top.
#[derive(Debug)]
pub struct SseResponse {
    response: TestResponse,
    events: Vec<Event>,
}

impl SseResponse {
    /// The parsed events, in the order the server sent them.
    #[must_use]
    pub fn events(&self) -> &[Event] {
        &self.events
    }

    /// Every event's `data`, joined nowhere — one `String` per event.
    #[must_use]
    pub fn data(&self) -> Vec<String> {
        self.events.iter().map(|event| event.data.clone()).collect()
    }

    /// The underlying response, for status, header and body assertions.
    #[must_use]
    pub fn response(&self) -> &TestResponse {
        &self.response
    }

    /// The response status.
    #[must_use]
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }

    /// Assert the response status, the way [`TestResponse::assert_status`] does.
    pub fn assert_status(&self, expected: impl crate::response::IntoStatus) -> &Self {
        self.response.assert_status(expected);
        self
    }

    /// Assert that exactly `expected` events were streamed.
    ///
    /// # Panics
    ///
    /// If the count differs, printing every event's data.
    pub fn assert_event_count(&self, expected: usize) -> &Self {
        if self.events.len() != expected {
            panic!(
                "{}",
                self.report(&format!(
                    "expected {expected} event(s), the stream carried {}",
                    self.events.len()
                ))
            );
        }
        self
    }

    /// Assert that some event's `data` contains `needle`.
    ///
    /// # Panics
    ///
    /// If no event's data contains it, printing what was streamed.
    pub fn assert_data_contains(&self, needle: &str) -> &Self {
        if !self.events.iter().any(|event| event.data.contains(needle)) {
            panic!(
                "{}",
                self.report(&format!("no event's data contained {needle:?}"))
            );
        }
        self
    }

    /// Assert that some event carried the listener name `name` (its `event:`
    /// field).
    ///
    /// # Panics
    ///
    /// If no event was named that, printing what was streamed.
    pub fn assert_named(&self, name: &str) -> &Self {
        if !self
            .events
            .iter()
            .any(|event| event.name.as_deref() == Some(name))
        {
            panic!("{}", self.report(&format!("no event was named {name:?}")));
        }
        self
    }

    /// A failure report headed `headline`, listing the streamed events.
    fn report(&self, headline: &str) -> String {
        let mut out = crate::report::rule("moso-test: sse assertion failed");
        out.push_str(&format!("  {headline}\n\n"));
        let body = if self.events.is_empty() {
            "(no events)".to_owned()
        } else {
            self.events
                .iter()
                .enumerate()
                .map(|(index, event)| {
                    let name = event.name.as_deref().unwrap_or("-");
                    format!("{index}  event={name}  data={:?}", event.data)
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        out.push_str(&crate::report::section("events", &body));
        out.push_str(&crate::report::rule_end());
        out
    }
}

/// Parse a `text/event-stream` body into dispatched [`Event`]s.
///
/// Frames are separated by a blank line; a frame that carries only comments (a
/// keep-alive) dispatches nothing and is skipped, matching what a browser
/// `EventSource` does. `data` lines are rejoined with `\n`.
fn parse_event_stream(bytes: &Bytes) -> Vec<Event> {
    let text = String::from_utf8_lossy(bytes);
    let normalised = text.replace("\r\n", "\n").replace('\r', "\n");

    let mut events = Vec::new();
    for frame in normalised.split("\n\n") {
        if frame.is_empty() {
            continue;
        }

        let mut event = Event::default();
        let mut data_lines: Vec<&str> = Vec::new();
        let mut dispatched = false;

        for line in frame.split('\n') {
            if line.is_empty() {
                continue;
            }
            if let Some(comment) = line.strip_prefix(':') {
                let text = comment.strip_prefix(' ').unwrap_or(comment);
                event.comment = Some(match event.comment.take() {
                    Some(existing) => format!("{existing}\n{text}"),
                    None => text.to_owned(),
                });
                continue;
            }

            let (field, value) = match line.split_once(':') {
                Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
                None => (line, ""),
            };
            dispatched = true;
            match field {
                "data" => data_lines.push(value),
                "event" => event.name = Some(value.to_owned()),
                "id" => event.id = Some(value.to_owned()),
                "retry" => event.retry = value.parse().ok(),
                // An unknown field is ignored by the SSE spec.
                _ => {}
            }
        }

        if !data_lines.is_empty() {
            event.data = data_lines.join("\n");
        }
        // A frame with only a comment is a keep-alive; it dispatches nothing.
        if dispatched {
            events.push(event);
        }
    }
    events
}

// ---------------------------------------------------------------------------
// Multipart
// ---------------------------------------------------------------------------

/// One part of a multipart body.
#[derive(Clone, Debug)]
pub struct Part {
    /// The form field name.
    pub name: String,
    /// The `filename` parameter, which is what makes a part a *file* part.
    pub filename: Option<String>,
    /// The part's own `Content-Type`.
    pub content_type: Option<String>,
    /// The bytes.
    pub data: Bytes,
}

/// A `multipart/form-data` body, assembled by hand.
///
/// Built here rather than with `reqwest::multipart` so that the in-process and
/// socket transports send byte-identical bodies — a test that passes over one
/// and fails over the other would otherwise be a mystery about the *client*.
/// The boundary is derived from a counter rather than randomness, so a captured
/// failure body is reproducible.
///
/// ```
/// use moso_test::Multipart;
///
/// let form = Multipart::new()
///     .text("title", "Holiday")
///     .file("photo", "beach.jpg", "image/jpeg", b"\xff\xd8\xff".to_vec());
///
/// // The boundary comes from a counter, not randomness, so a captured failure
/// // body is reproducible.
/// assert!(form.content_type().starts_with("multipart/form-data; boundary="));
/// ```
///
/// Attach it with `app.client().post("/upload").multipart(form)`.
#[derive(Clone, Debug, Default)]
pub struct Multipart {
    parts: Vec<Part>,
    boundary: Option<String>,
}

impl Multipart {
    /// An empty form.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add a plain text field.
    #[must_use]
    pub fn text(mut self, name: &str, value: &str) -> Self {
        self.parts.push(Part {
            name: name.to_owned(),
            filename: None,
            content_type: None,
            data: Bytes::from(value.to_owned()),
        });
        self
    }

    /// Add a file field.
    #[must_use]
    pub fn file(
        mut self,
        name: &str,
        filename: &str,
        content_type: &str,
        data: impl Into<Bytes>,
    ) -> Self {
        self.parts.push(Part {
            name: name.to_owned(),
            filename: Some(filename.to_owned()),
            content_type: Some(content_type.to_owned()),
            data: data.into(),
        });
        self
    }

    /// Add a fully specified part.
    #[must_use]
    pub fn part(mut self, part: Part) -> Self {
        self.parts.push(part);
        self
    }

    /// Use a fixed boundary rather than the generated one.
    #[must_use]
    pub fn boundary(mut self, boundary: impl Into<String>) -> Self {
        self.boundary = Some(boundary.into());
        self
    }

    /// The `Content-Type` header this form must be sent with.
    #[must_use]
    pub fn content_type(&self) -> String {
        format!("multipart/form-data; boundary={}", self.resolved_boundary())
    }

    /// The encoded body.
    #[must_use]
    pub fn into_body(self) -> Bytes {
        let boundary = self.resolved_boundary();
        let mut out: Vec<u8> = Vec::new();
        for part in &self.parts {
            out.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
            let mut disposition = format!("Content-Disposition: form-data; name=\"{}\"", part.name);
            if let Some(filename) = &part.filename {
                disposition.push_str(&format!("; filename=\"{filename}\""));
            }
            disposition.push_str("\r\n");
            out.extend_from_slice(disposition.as_bytes());
            if let Some(content_type) = &part.content_type {
                out.extend_from_slice(format!("Content-Type: {content_type}\r\n").as_bytes());
            }
            out.extend_from_slice(b"\r\n");
            out.extend_from_slice(&part.data);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
        Bytes::from(out)
    }

    /// How many parts the form has.
    #[must_use]
    pub fn len(&self) -> usize {
        self.parts.len()
    }

    /// Whether the form has no parts.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    fn resolved_boundary(&self) -> String {
        self.boundary
            .clone()
            .unwrap_or_else(|| format!("moso-test-boundary-{}", self.parts.len()))
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Set a header, failing the test on a name or value HTTP cannot carry.
fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) {
    let name = HeaderName::try_from(name)
        .unwrap_or_else(|error| panic!("moso-test: {name:?} is not a header name ({error})"));
    let value = HeaderValue::from_str(value).unwrap_or_else(|error| {
        panic!("moso-test: {value:?} is not a value for header `{name}` ({error})")
    });
    headers.insert(name, value);
}

/// Split an already-encoded query string back into decoded pairs.
fn form_urlencoded_pairs(encoded: &str) -> Vec<(String, String)> {
    if encoded.is_empty() {
        return Vec::new();
    }
    serde_urlencoded::from_str::<Vec<(String, String)>>(encoded).unwrap_or_default()
}

/// Crockford's base32 alphabet, which is the one ULIDs are written in.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// A fresh correlation id.
///
/// # Why it is shaped like a ULID
///
/// Moso's request-id middleware adopts a client-supplied id only when it parses
/// as a ULID, and generates a fresh one otherwise. An id the middleware
/// discards is an id the server's log lines are filed under a *different* key
/// from the one the client remembers — which silently empties the most useful
/// section of the failure report. So the harness issues ids the middleware will
/// keep.
///
/// The value is `process id << 64 | counter`, so it is unique across the
/// processes `cargo test` runs in parallel and *deterministic* within one: the
/// same test always produces the same ids, which is what makes a captured
/// failure report reproducible. The top three bits are always zero, so the
/// 130-bit encoding always fits the 128 bits a ULID has.
fn next_request_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    encode_crockford((u128::from(std::process::id()) << 64) | u128::from(counter))
}

/// Encode 128 bits as the 26 Crockford base32 characters a ULID is written in.
fn encode_crockford(value: u128) -> String {
    let mut out = [0_u8; 26];
    let mut remaining = value;
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(remaining & 0x1f) as usize];
        remaining >>= 5;
    }
    String::from_utf8(out.to_vec()).expect("every byte comes from an ASCII table")
}

/// A duration as a test reader would say it.
fn humanise(duration: Duration) -> String {
    if duration < Duration::from_secs(1) {
        format!("{} ms", duration.as_millis())
    } else {
        format!("{:.1} s", duration.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_event_stream_parser_reads_fields_and_rejoins_data() {
        let wire = "event: tick\nid: 7\ndata: one\ndata: two\n\ndata: {\"n\":1}\n\n";
        let events = parse_event_stream(&Bytes::from_static(wire.as_bytes()));

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].name.as_deref(), Some("tick"));
        assert_eq!(events[0].id.as_deref(), Some("7"));
        assert_eq!(events[0].data, "one\ntwo");
        assert_eq!(events[1].data, "{\"n\":1}");
    }

    #[test]
    fn a_keep_alive_comment_frame_dispatches_nothing() {
        let wire = ": keep-alive\n\ndata: real\n\n";
        let events = parse_event_stream(&Bytes::from_static(wire.as_bytes()));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn crlf_line_endings_parse_the_same_as_lf() {
        let wire = "data: hello\r\n\r\n";
        let events = parse_event_stream(&Bytes::from_static(wire.as_bytes()));

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "hello");
    }

    #[test]
    fn request_ids_are_unique_and_acceptable_to_the_middleware() {
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, b);
        // The three things `moso_core::middleware::request_id` checks.
        assert_eq!(a.len(), 26);
        assert!(a.bytes().all(|byte| byte.is_ascii_graphic()), "{a}");
        assert!(
            a.bytes().all(|byte| CROCKFORD.contains(&byte)),
            "{a} is not Crockford base32"
        );
    }

    /// The invariant the whole log-attribution story rests on: Moso's own ULID
    /// parser must accept what the harness sends, or the middleware replaces it.
    #[test]
    fn a_generated_id_round_trips_through_a_real_ulid_parser() {
        let id = next_request_id();
        let parsed = ulid::Ulid::from_string(&id).expect("moso's parser accepts it");
        assert_eq!(parsed.to_string(), id);
    }

    #[test]
    fn the_encoder_covers_the_ends_of_the_range() {
        assert_eq!(encode_crockford(0), "0".repeat(26));
        assert_eq!(encode_crockford(1), format!("{}1", "0".repeat(25)));
        // 128 bits of ones is the largest ULID, `7ZZZ…`.
        let max = encode_crockford(u128::MAX);
        assert_eq!(max.len(), 26);
        assert!(max.starts_with('7'), "{max}");
        assert!(max.ends_with('Z'), "{max}");
    }

    #[test]
    fn a_multipart_body_has_the_shape_a_parser_expects() {
        let form = Multipart::new()
            .boundary("BOUND")
            .text("title", "hello")
            .file(
                "avatar",
                "a.png",
                "image/png",
                Bytes::from_static(b"\x89PNG"),
            );
        assert_eq!(form.content_type(), "multipart/form-data; boundary=BOUND");
        let body = form.into_body();
        let text = String::from_utf8_lossy(&body);
        assert!(text.starts_with("--BOUND\r\n"), "{text}");
        assert!(text.contains("Content-Disposition: form-data; name=\"title\"\r\n\r\nhello\r\n"));
        assert!(text.contains("name=\"avatar\"; filename=\"a.png\""));
        assert!(text.contains("Content-Type: image/png"));
        assert!(text.ends_with("--BOUND--\r\n"), "{text}");
    }

    #[test]
    fn an_empty_multipart_body_is_just_the_closing_boundary() {
        let body = Multipart::new().boundary("B").into_body();
        assert_eq!(&body[..], b"--B--\r\n");
    }

    #[test]
    fn multipart_reports_its_size() {
        assert!(Multipart::new().is_empty());
        assert_eq!(Multipart::new().text("a", "1").len(), 1);
    }

    #[test]
    fn query_pairs_round_trip_through_the_encoder() {
        let pairs = form_urlencoded_pairs("a=1&b=hello+world");
        assert_eq!(
            pairs,
            [
                ("a".to_owned(), "1".to_owned()),
                ("b".to_owned(), "hello world".to_owned())
            ]
        );
        assert!(form_urlencoded_pairs("").is_empty());
    }

    #[test]
    fn inserting_a_header_replaces_rather_than_appends() {
        let mut headers = HeaderMap::new();
        insert_header(&mut headers, "x-a", "1");
        insert_header(&mut headers, "x-a", "2");
        assert_eq!(headers.get_all("x-a").iter().count(), 1);
        assert_eq!(headers["x-a"], "2");
    }

    #[test]
    fn durations_read_as_a_person_would_say_them() {
        assert_eq!(humanise(Duration::from_millis(250)), "250 ms");
        assert_eq!(humanise(Duration::from_millis(1500)), "1.5 s");
    }
}
