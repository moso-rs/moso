//! Booting the real application for a test.
//!
//! # The principle, restated
//!
//! A harness that constructs a parallel, simplified application tests the
//! harness. [`TestApp`] boots the **real** [`moso::App`]: the real provider
//! map, the real middleware stack, the real boot-time validation, the real
//! OpenAPI document. If a provider is missing, [`TestAppBuilder::spawn`] returns
//! the same grouped boot report `main` would have printed — which means a test
//! catches the misconfiguration that would otherwise be found in staging.
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
//! # async fn main() -> moso::Result<()> { creating_a_user_returns_201().await }
//! // #[tokio::test]
//! async fn creating_a_user_returns_201() -> moso::Result<()> {
//!     let app = TestApp::builder().app(app()).spawn().await?;
//!
//!     app.client()
//!         .post("/users")
//!         .json(&serde_json::json!({ "username": "ada", "email": "ada@example.com" }))
//!         .send()
//!         .await
//!         .assert_status(201)
//!         .assert_json_path("/username", "ada")
//!         .assert_matches_openapi();
//!
//!     app.logs().assert_no_errors();
//!     Ok(())
//! }
//! ```
//!
//! # Two ways to reach the application
//!
//! By default the client calls the composed `tower::Service` in process: no
//! socket, no accept loop, no second runtime, and a whole class of flake that
//! cannot happen. [`TestAppBuilder::bind`] instead binds an ephemeral port on
//! `127.0.0.1` and serves on it, for the tests that genuinely need the wire.
//!
//! The two differ in one further way, and it is worth knowing: `bind()` goes
//! through [`App::serve_on`](moso::App::serve_on), so **`on_startup` hooks and
//! lifespan guards run**. The in-process path uses
//! [`App::into_service`](moso::App::into_service), which by Moso's own
//! definition does not run them. A test whose application depends on a startup
//! hook must call `bind()`.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use http::HeaderMap;
use moso::openapi::Document;
use moso::{App, AppBuilder, AppState, Resolver, Result, Router, Signal};
use moso_core::di::DependencyOverrides;
use tokio::task::JoinHandle;
use url::Url;

use crate::client::{ClientInner, TestClient, Transport};
use crate::clock::TestClock;
use crate::contract::Options as ContractOptions;
use crate::logs::{self, LogAssertions, LogBuffer};

/// How long [`TestAppBuilder::bind`] waits for the server task to start serving.
#[cfg(feature = "server")]
const READY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often it re-probes while waiting.
#[cfg(feature = "server")]
const READY_INTERVAL: Duration = Duration::from_millis(10);

/// The host the in-process transport addresses requests to.
///
/// Nothing resolves it — the request never leaves the process — but a request
/// needs an absolute URL for `Host` and for the failure report, and
/// `http://localhost/` is the one every reader parses without thinking.
const IN_PROCESS_BASE: &str = "http://localhost/";

// ---------------------------------------------------------------------------
// TestApp
// ---------------------------------------------------------------------------

/// A booted application, and everything a test needs to drive and inspect it.
///
/// Dropping it stops the server task, if there is one, and releases the log
/// buffer. Nothing leaks between tests, so a hundred of these can run in
/// parallel in one binary.
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
/// let app = TestApp::builder().app(app()).spawn().await?;
///
/// // The real document, from the real routes.
/// assert!(app.openapi().paths.contains_key("/users"));
///
/// app.client()
///     .post("/users")
///     .json(&serde_json::json!({ "username": "ada" }))
///     .send()
///     .await
///     .assert_status(201)
///     .assert_matches_openapi();
///
/// app.logs().assert_no_errors();
/// # Ok(())
/// # }
/// ```
///
/// Boots the **real** `App`, including boot-time validation: a missing provider
/// fails the test with the same grouped report `main` would have printed, rather
/// than surfacing in staging.
pub struct TestApp {
    inner: Arc<TestAppInner>,
    client: TestClient,
}

struct TestAppInner {
    id: u64,
    base_url: Url,
    state: Arc<AppState>,
    resolver: Resolver,
    logs: LogAssertions,
    clock: TestClock,
    paused_time: bool,
    server: Option<ServerHandle>,
    /// Kept so `service()` can hand back a clone for a test that wants to drive
    /// the stack itself. `None` in bound mode, where the `App` was consumed by
    /// `serve_on`.
    service: Option<moso::deps::axum::Router<()>>,
}

/// The spawned server task, in bound mode.
struct ServerHandle {
    addr: SocketAddr,
    signal: Signal,
    task: JoinHandle<Result<()>>,
}

impl TestApp {
    /// Start a builder.
    #[must_use]
    pub fn builder() -> TestAppBuilder {
        TestAppBuilder::new()
    }

    /// Boot `app` with the harness defaults and no further configuration.
    ///
    /// # Deviation from `43-testing.md`
    ///
    /// The design document writes this as `TestApp::spawn()`, taking nothing and
    /// finding the application itself. It cannot: with no link-time registry
    /// (decision D11) and no `#[moso::test]` macro in this build, nothing can
    /// tell the harness where the composition root is. Passing the
    /// [`AppBuilder`] is one word longer and is the only spelling that is
    /// honest about it. [`test_app!`](crate::test_app) wraps it.
    ///
    /// # Errors
    ///
    /// The application's own boot report, unchanged.
    pub async fn spawn(app: AppBuilder) -> Result<Self> {
        TestAppBuilder::new().app(app).spawn().await
    }

    /// The client every request should go through.
    ///
    /// Always the unauthenticated baseline: the derived clients
    /// ([`TestClient::with_bearer`] and friends) return new values rather than
    /// mutating this one.
    #[must_use]
    pub fn client(&self) -> &TestClient {
        &self.client
    }

    /// A client with no credentials at all.
    #[must_use]
    pub fn as_anonymous(&self) -> TestClient {
        self.client.anonymous()
    }

    /// A client sending `Authorization: Bearer <token>` on every request.
    ///
    /// The stand-in for `43-testing.md`'s `as_user(&User)`, which needs an auth
    /// battery this build does not have.
    #[must_use]
    pub fn as_bearer(&self, token: &str) -> TestClient {
        self.client.with_bearer(token)
    }

    /// Assertions over the application's own log output.
    #[must_use]
    pub fn logs(&self) -> &LogAssertions {
        &self.inner.logs
    }

    /// The URL requests are addressed to.
    ///
    /// In bound mode this is a real `http://127.0.0.1:<port>/`. In process it is
    /// `http://localhost/`, which nothing resolves.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.inner.base_url
    }

    /// The bound address, when the application is serving on a real socket.
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.inner.server.as_ref().map(|server| server.addr)
    }

    /// The generated OpenAPI document.
    #[must_use]
    pub fn openapi(&self) -> &Document {
        self.inner.state.document()
    }

    /// The frozen application state: providers, limits, profile, health checks.
    #[must_use]
    pub fn state(&self) -> &Arc<AppState> {
        &self.inner.state
    }

    /// A resolver over the application's providers, for a test that wants to
    /// reach into the graph rather than through HTTP.
    #[must_use]
    pub fn resolver(&self) -> &Resolver {
        &self.inner.resolver
    }

    /// The clock the harness registered as a provider.
    #[must_use]
    pub fn clock(&self) -> &TestClock {
        &self.inner.clock
    }

    /// The composed service, for a test that wants to drive `tower` itself.
    ///
    /// `None` in bound mode, where [`App::serve_on`](moso::App::serve_on)
    /// consumed the application.
    #[must_use]
    pub fn service(&self) -> Option<moso::deps::axum::Router<()>> {
        self.inner.service.clone()
    }

    /// Move the application's clock forward.
    ///
    /// # What this actually moves
    ///
    /// The [`TestClock`] this app provides, which application code reads through
    /// `Inject<TestClock>`. It does **not** move framework internals: `moso-core`
    /// reads `Instant::now()` and `SystemTime::now()` directly rather than
    /// through an indirection a harness can replace, so timeouts, `Retry-After`
    /// and the shutdown grace are unaffected. That gap is noted in
    /// [`crate::clock`] and is a `moso-core` change, not a harness one.
    ///
    /// Tokio's own clock is advanced too, but only when the test opted in with
    /// [`TestAppBuilder::paused_time`] — `tokio::time::advance` panics on a
    /// runtime whose time is not paused, and a harness must not turn a missing
    /// annotation into a panic inside an unrelated assertion.
    pub async fn advance_time(&self, by: Duration) {
        self.inner.clock.advance(by);
        if self.inner.paused_time {
            tokio::time::advance(by).await;
        }
        // Give anything waiting on the new time a chance to observe it before
        // the caller's next assertion.
        tokio::task::yield_now().await;
    }

    /// Stop the application and wait for it to finish draining.
    ///
    /// Called automatically on drop; call it explicitly when the test wants to
    /// assert on what shutdown logged.
    pub async fn shutdown(mut self) {
        let Some(server) = Arc::get_mut(&mut self.inner).and_then(|inner| inner.server.take())
        else {
            // Another handle is still alive, or there is no server task; the
            // `Drop` impl will trigger the signal.
            return;
        };
        server.signal.trigger();
        let _ = server.task.await;
    }
}

impl Drop for TestAppInner {
    fn drop(&mut self) {
        if let Some(server) = &self.server {
            server.signal.trigger();
            server.task.abort();
        }
        logs::deregister(self.id);
    }
}

impl core::fmt::Debug for TestApp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestApp")
            .field("base_url", &self.inner.base_url.as_str())
            .field("bound", &self.inner.server.is_some())
            .field("logs", &self.inner.logs.len())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// TestAppBuilder
// ---------------------------------------------------------------------------

/// One edit applied to the application builder before it is built.
type Edit = Box<dyn FnOnce(AppBuilder) -> AppBuilder + Send>;

/// One adjustment to the framework's own HTTP section.
type HttpEdit = Box<dyn FnOnce(&mut moso::http_config::HttpConfig) + Send>;

/// One entry written into the request-scoped [`DependencyOverrides`] table.
type OverrideEdit = Box<dyn FnOnce(&mut DependencyOverrides) + Send>;

/// Assembles a [`TestApp`].
///
/// Everything registered here is applied **after** the application's own
/// composition root, and provider registration is last-write-wins, so
/// [`override_provider`](Self::override_provider) really does override.
pub struct TestAppBuilder {
    app: Option<AppBuilder>,
    edits: Vec<Edit>,
    /// Replaces the framework's HTTP section outright, when set.
    http: Option<moso::http_config::HttpConfig>,
    /// Adjustments applied on top, in registration order.
    http_edits: Vec<HttpEdit>,
    /// Request-scoped dependency fixtures, collapsed into one table at boot.
    dependency_overrides: Vec<OverrideEdit>,
    profile: Option<moso::config::Profile>,
    bind: bool,
    paused_time: bool,
    contract: Option<ContractOptions>,
    clock: TestClock,
    log_limit: usize,
    headers: HeaderMap,
}

impl TestAppBuilder {
    /// A builder with the harness defaults and no application yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            app: None,
            edits: Vec::new(),
            http: None,
            http_edits: Vec::new(),
            dependency_overrides: Vec::new(),
            profile: Some(moso::config::Profile::Test),
            bind: false,
            paused_time: false,
            contract: None,
            clock: TestClock::new(),
            log_limit: logs::DEFAULT_LOG_LIMIT,
            headers: HeaderMap::new(),
        }
    }

    /// The application under test.
    ///
    /// This should be the crate's real composition root — `my_crate::app()` —
    /// not one assembled for the test. A harness that builds its own application
    /// tests the harness.
    #[must_use]
    pub fn app(mut self, app: AppBuilder) -> Self {
        self.app = Some(app);
        self
    }

    /// Mount a router, in addition to whatever the application mounts.
    #[must_use]
    pub fn mount(mut self, router: Router) -> Self {
        self.edits.push(Box::new(move |app| app.mount(router)));
        self
    }

    /// Mount a router under a prefix.
    #[must_use]
    pub fn mount_at(mut self, prefix: &'static str, router: Router) -> Self {
        self.edits
            .push(Box::new(move |app| app.mount_at(prefix, router)));
        self
    }

    /// Register a provider, or replace the application's.
    ///
    /// This is the lever every test pulls: production wires an SMTP mailer, the
    /// test wires a capturing one, and no handler changes.
    #[must_use]
    pub fn override_provider<T: Send + Sync + 'static>(mut self, value: T) -> Self {
        self.edits.push(Box::new(move |app| app.provide(value)));
        self
    }

    /// Register a trait-object provider, or replace the application's.
    #[must_use]
    pub fn override_provider_dyn<T: ?Sized + Send + Sync + 'static>(
        mut self,
        value: Arc<T>,
    ) -> Self {
        self.edits.push(Box::new(move |app| app.provide_dyn(value)));
        self
    }

    /// Replace one request-scoped [`Dependency`](moso::Dependency) with a
    /// fixture — the FastAPI `dependency_overrides` table, spelled in Rust.
    ///
    /// [`override_provider`](Self::override_provider) swaps an *app-lifetime*
    /// value; this swaps a *per-request* one. The closure is handed the same
    /// [`RequestCtx`](moso::RequestCtx) the real `resolve` would get — so a
    /// fixture can still read the request it stands in for — and its result is
    /// memoised for the request exactly as the real resolution is. Everything
    /// not named here still resolves the real way.
    ///
    /// Calls accumulate; overriding two different dependencies takes two calls,
    /// and overriding the same one twice keeps the last. The table is installed
    /// as an ordinary provider, so it is compiled out of a release build with
    /// the rest of the `test` feature and can never replace a dependency in
    /// production.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso_test::prelude::*;
    /// # /// Everything this application reads from its environment.
    /// # #[derive(moso::Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Service name.
    /// #     #[config(default = "users")] pub name: String }
    ///
    /// /// Who the request acts as. In production this reads a session cookie.
    /// #[derive(Clone, Debug)]
    /// pub struct CurrentUser {
    ///     /// Whether the caller may act as an administrator.
    ///     pub is_admin: bool,
    /// }
    ///
    /// impl Dependency for CurrentUser {
    ///     async fn resolve(_: &RequestCtx) -> Result<Self> {
    ///         // The real path: anonymous unless a session says otherwise.
    ///         Ok(CurrentUser { is_admin: false })
    ///     }
    /// }
    ///
    /// /// Report whether the caller is an admin.
    /// #[moso::endpoint]
    /// async fn whoami(Depends(user): Depends<CurrentUser>) -> Result<moso::response::Json<bool>> {
    ///     Ok(moso::response::Json(user.is_admin))
    /// }
    ///
    /// /// The composition root every Moso application exposes.
    /// fn app() -> moso::AppBuilder {
    ///     moso::App::new(AppConfig { name: "users".to_owned() })
    ///         .mount(moso::routes! { GET "/whoami" => whoami })
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")]
    /// # async fn main() -> moso::Result<()> {
    /// let app = TestApp::builder()
    ///     .app(app())
    ///     // Stand in an admin without touching the login flow.
    ///     .override_dependency(|_ctx| async { Ok(CurrentUser { is_admin: true }) })
    ///     .spawn()
    ///     .await?;
    ///
    /// app.client().get("/whoami").send().await.assert_json_matches(true);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn override_dependency<D, F, Fut>(mut self, resolve: F) -> Self
    where
        D: moso::Dependency,
        F: Fn(moso::RequestCtx) -> Fut + Send + Sync + 'static,
        Fut: core::future::Future<Output = Result<D>> + Send + 'static,
    {
        self.dependency_overrides.push(Box::new(move |table| {
            table.insert::<D, _, _>(resolve);
        }));
        self
    }

    /// Apply an arbitrary edit to the application builder.
    ///
    /// The escape hatch for everything this builder does not name:
    /// `on_startup`, `with_middleware`, `openapi`, `health_check`.
    #[must_use]
    pub fn customise(
        mut self,
        edit: impl FnOnce(AppBuilder) -> AppBuilder + Send + 'static,
    ) -> Self {
        self.edits.push(Box::new(edit));
        self
    }

    /// Replace the HTTP limits and disclosure policy wholesale.
    ///
    /// # Deviation from `43-testing.md`
    ///
    /// The document writes `config(|c: &mut AppConfig| …)`. The application's own
    /// configuration is a type the harness has never heard of and which was
    /// already constructed before `App::new` saw it, so it cannot be edited from
    /// here — pass an edited value to your own `app()` instead, or override it
    /// with [`override_provider`](Self::override_provider). What *is* editable
    /// from here is the framework's own HTTP section, which is this method and
    /// [`http_config_with`](Self::http_config_with).
    #[must_use]
    pub fn http_config(mut self, config: moso::http_config::HttpConfig) -> Self {
        self.http = Some(config);
        self
    }

    /// Adjust the HTTP section rather than replacing it.
    ///
    /// Edits are applied in registration order to whatever
    /// [`http_config`](Self::http_config) left, so two calls compose instead of
    /// the second silently undoing the first.
    #[must_use]
    pub fn http_config_with(
        mut self,
        edit: impl FnOnce(&mut moso::http_config::HttpConfig) + Send + 'static,
    ) -> Self {
        self.http_edits.push(Box::new(edit));
        self
    }

    /// Replace the listener and shutdown settings.
    #[must_use]
    pub fn server_config(mut self, config: moso::http_config::ServerConfig) -> Self {
        self.edits
            .push(Box::new(move |app| app.server_config(config)));
        self
    }

    /// Let 5xx responses carry their detail and source chain.
    ///
    /// Off by default, deliberately: `Profile::Test` renders the bytes
    /// production will actually send, so a test asserting on an error body is
    /// asserting on the real one. Turn this on while debugging a 500 — or read
    /// the captured logs, which carry the detail either way.
    #[must_use]
    pub fn expose_internal_errors(self) -> Self {
        self.http_config_with(|http| http.expose_internal_errors = true)
    }

    /// Run under a specific profile. `Profile::Test` by default.
    #[must_use]
    pub fn profile(mut self, profile: moso::config::Profile) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Leave the application's own profile alone.
    #[must_use]
    pub fn inherit_profile(mut self) -> Self {
        self.profile = None;
        self
    }

    /// Serve on a real ephemeral port instead of calling the service in process.
    ///
    /// Also the only mode in which `on_startup` hooks and lifespan guards run —
    /// see the module header.
    #[must_use]
    pub fn bind(mut self) -> Self {
        self.bind = true;
        self
    }

    /// Declare that this test runs with Tokio's clock paused.
    ///
    /// Requires `#[tokio::test(start_paused = true)]`. With it,
    /// [`TestApp::advance_time`] also advances Tokio's timer, so a
    /// `tokio::time::sleep` inside the application returns immediately.
    #[must_use]
    pub fn paused_time(mut self) -> Self {
        self.paused_time = true;
        self
    }

    /// Pin the provided [`TestClock`] to a fixed instant.
    #[must_use]
    pub fn clock_at(mut self, at: SystemTime) -> Self {
        self.clock = TestClock::at(at);
        self
    }

    /// Check every response against the OpenAPI document.
    ///
    /// `43-testing.md`'s `[test] assert_openapi = true`: it makes every test in
    /// the suite a contract test, at the cost of one schema walk per response.
    #[must_use]
    pub fn assert_openapi(mut self, options: ContractOptions) -> Self {
        self.contract = Some(options);
        self
    }

    /// Send a header on every request this app's client makes.
    #[must_use]
    pub fn default_header(mut self, name: &str, value: &str) -> Self {
        let name = http::header::HeaderName::try_from(name)
            .unwrap_or_else(|error| panic!("moso-test: {name:?} is not a header name ({error})"));
        let value = http::header::HeaderValue::from_str(value)
            .unwrap_or_else(|error| panic!("moso-test: {value:?} is not a header value ({error})"));
        self.headers.insert(name, value);
        self
    }

    /// How many log records to keep before dropping the oldest.
    #[must_use]
    pub fn log_limit(mut self, limit: usize) -> Self {
        self.log_limit = limit;
        self
    }

    /// Build and start the application.
    ///
    /// # Errors
    ///
    /// The application's boot report if the graph does not validate, or an
    /// [`Error`](struct@moso::Error) if the listener cannot be bound or the server task
    /// fails to start.
    pub async fn spawn(self) -> Result<TestApp> {
        let capturing = logs::install();
        let id = logs::next_app_id();
        let buffer = Arc::new(LogBuffer::new(self.log_limit));
        logs::register(id, &buffer);
        let log_assertions = LogAssertions::new(buffer, capturing);

        // Everything from here on — including the boot report — is attributed to
        // this test app, so a boot failure's own log lines end up in its buffer.
        let boot_span = tracing::info_span!(
            target: "moso_test",
            "moso_test::app",
            moso_test_app = id,
        );

        let mut builder = self.app.unwrap_or_default();
        for edit in self.edits {
            builder = edit(builder);
        }
        if let Some(profile) = self.profile {
            builder = builder.profile(profile);
        }
        // The HTTP section is applied last so that an edit made here wins over
        // whatever the application's own composition root set, and so that two
        // edits compose rather than the second undoing the first.
        if self.http.is_some() || !self.http_edits.is_empty() {
            let mut http = self.http.unwrap_or_default();
            for edit in self.http_edits {
                edit(&mut http);
            }
            builder = builder.http_config(http);
        }
        // Collapse every `override_dependency` call into one table and register
        // it as a provider, which is how `RequestCtx::depends` finds it. An
        // application that never overrides a dependency registers no table, so
        // the `depends` fast path stays a single miss.
        if !self.dependency_overrides.is_empty() {
            let mut table = DependencyOverrides::new();
            for edit in self.dependency_overrides {
                edit(&mut table);
            }
            builder = builder.provide(table);
        }
        // Last, so it wins over an application that happens to provide one.
        builder = builder.provide(self.clock.clone());

        let app = boot_span.in_scope(|| builder.build())?;

        let state = Arc::clone(app.state());
        let resolver = app.resolver();

        let (base_url, transport, server, service) = if self.bind {
            let (base_url, transport, handle) = start_bound(app, boot_span).await?;
            (base_url, transport, Some(handle), None)
        } else {
            let service = app.into_service().layer(crate::logs::CaptureLayer::new(id));
            let base_url = Url::parse(IN_PROCESS_BASE).expect("a constant, valid URL");
            (
                base_url,
                Transport::InProcess(Box::new(service.clone())),
                None,
                Some(service),
            )
        };

        let client = TestClient::new(ClientInner {
            transport,
            base_url: base_url.clone(),
            state: Arc::clone(&state),
            logs: log_assertions.clone(),
            app: id,
            headers: self.headers,
            cookies: Vec::new(),
            contract: self.contract,
            timeout: None,
        });

        Ok(TestApp {
            inner: Arc::new(TestAppInner {
                id,
                base_url,
                state,
                resolver,
                logs: log_assertions,
                clock: self.clock,
                paused_time: self.paused_time,
                server,
                service,
            }),
            client,
        })
    }
}

impl Default for TestAppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for TestAppBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("TestAppBuilder")
            .field("has_app", &self.app.is_some())
            .field("edits", &self.edits.len())
            .field("http_edits", &self.http_edits.len())
            .field("dependency_overrides", &self.dependency_overrides.len())
            .field("bind", &self.bind)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// The bound-port path
// ---------------------------------------------------------------------------

/// Bind `127.0.0.1:0`, serve the application on it, and wait until it answers.
///
/// No TLS, no proxy, no redirect following, no cookie store on the client: a
/// test that wants any of those should be asserting on the `Location` header or
/// the `Set-Cookie` header, not on what a client library did with them behind
/// its back.
#[cfg(feature = "server")]
async fn start_bound(app: App, span: tracing::Span) -> Result<(Url, Transport, ServerHandle)> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|error| {
            moso::Error::internal(error).with_detail("could not bind an ephemeral test port")
        })?;
    let addr = listener.local_addr().map_err(|error| {
        moso::Error::internal(error).with_detail("the bound listener has no local address")
    })?;
    let signal = app.shutdown_signal();
    let task = tokio::spawn(tracing::Instrument::instrument(
        app.serve_on(listener),
        span,
    ));
    let handle = ServerHandle { addr, signal, task };

    let base_url = Url::parse(&format!("http://{addr}/")).map_err(|error| {
        moso::Error::internal(error).with_detail("the bound address is not a URL")
    })?;
    // The harness speaks plaintext to 127.0.0.1 and asks for no TLS, but cargo
    // unifies features: in a workspace that also contains `moso-mail` or
    // `moso-storage`, reqwest is built with `rustls-no-provider` and constructs
    // a rustls connector regardless — panicking on `build()` if no process
    // default provider was installed. ring, not aws-lc-rs, because sqlx has
    // already chosen ring and two providers in one process is a runtime panic.
    // `install_default` fails only when somebody got here first, which is the
    // outcome we wanted anyway, so the result is deliberately discarded.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|error| {
            moso::Error::internal(error).with_detail("the test HTTP client could not be built")
        })?;

    wait_until_ready(&base_url, &client, &handle).await?;
    Ok((base_url, Transport::Socket(client), handle))
}

/// Without the `server` feature there is no client to bind a port for.
#[cfg(not(feature = "server"))]
#[allow(
    clippy::unused_async,
    reason = "the signature must match the `server` variant"
)]
async fn start_bound(_app: App, _span: tracing::Span) -> Result<(Url, Transport, ServerHandle)> {
    Err(moso::Error::internal_msg(
        "TestAppBuilder::bind() needs moso-test's `server` feature, which is what \
         brings in the HTTP client that would talk to the port",
    ))
}

/// Poll the listener until it answers, or the server task fails.
///
/// The listener is bound before `serve_on` runs the startup hooks, so a TCP
/// connect succeeds long before the application is actually serving. Only an
/// HTTP round trip proves readiness — and if a startup hook failed, the task has
/// already finished with the error that says why.
#[cfg(feature = "server")]
async fn wait_until_ready(
    base_url: &Url,
    client: &reqwest::Client,
    handle: &ServerHandle,
) -> Result<()> {
    let probe = base_url.join("healthz").map_err(|error| {
        moso::Error::internal(error).with_detail("the probe URL could not be built")
    })?;
    let deadline = std::time::Instant::now() + READY_TIMEOUT;

    loop {
        if handle.task.is_finished() {
            return Err(moso::Error::internal_msg(
                "the application stopped before it began serving; a startup hook failed, \
                 and its own error was logged",
            ));
        }
        // Any HTTP response at all means the stack is up.
        if client.get(probe.clone()).send().await.is_ok() {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(moso::Error::internal_msg(format!(
                "the application did not start serving on {} within {} s",
                handle.addr,
                READY_TIMEOUT.as_secs()
            )));
        }
        tokio::time::sleep(READY_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_in_process_base_url_parses() {
        let url = Url::parse(IN_PROCESS_BASE).expect("valid");
        assert_eq!(url.path(), "/");
        assert_eq!(url.host_str(), Some("localhost"));
    }

    #[test]
    fn a_fresh_builder_defaults_to_the_test_profile_and_in_process() {
        let builder = TestAppBuilder::new();
        assert_eq!(builder.profile, Some(moso::config::Profile::Test));
        assert!(!builder.bind);
        assert!(!builder.paused_time);
        assert!(builder.contract.is_none());
    }

    #[test]
    fn inheriting_the_profile_leaves_it_unset() {
        let builder = TestAppBuilder::new().inherit_profile();
        assert!(builder.profile.is_none());
    }

    #[test]
    fn edits_accumulate_in_registration_order() {
        let builder = TestAppBuilder::new()
            .override_provider(1u8)
            .override_provider(2u16);
        assert_eq!(builder.edits.len(), 2);
    }

    #[test]
    fn a_default_header_is_validated_eagerly() {
        let builder = TestAppBuilder::new().default_header("x-test", "1");
        assert_eq!(builder.headers["x-test"], "1");
    }
}
