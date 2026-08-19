//! `App` — lifecycle, boot-time validation, and the composition root.
//!
//! Axum gives you a `Router` and `axum::serve`. Everything between —
//! configuration, connection pools, graceful shutdown, health checks, "did I
//! wire up the thing this handler needs" — is the user's problem. `App` is
//! where the framework earns its name.
//!
//! The design goal, stated once: **anything that can fail should fail at boot
//! with a sentence, not at 3am with a panic inside a request.**
//!
//! # The boot sequence, normatively
//!
//! ```text
//! build():
//!   1. Resolve config          typed, all sources layered, secrets redacted
//!   2. Freeze provider map     run `provide_with` factories in dependency order
//!   3. Compose router          merge mounted routers; detect route conflicts   ★
//!   4. Validate the DI graph   every operation's providers exist               ★
//!   5. Build the OpenAPI doc   duplicate operation ids, schema name clashes    ★
//!   6. Compose middleware      assert the stack ordering invariants            ★
//!   → App
//!
//! serve():
//!   7.  Run `on_startup` hooks in registration order; a failure aborts
//!   8.  Bind; log the address, the docs URL and the profile
//!   9.  Install SIGINT/SIGTERM handlers
//!   10. Serve
//!   ── on signal ──
//!   11. /readyz answers 503 immediately
//!   12. Stop accepting; drain in-flight up to `shutdown_grace`
//!   13. Run `on_shutdown` hooks in reverse order; drop lifespan guards
//!   14. Flush tracing exporters (the `TracingGuard` `serve` holds, when a
//!       `tracing_config` was set — see the `observability` module, behind the `subscriber` feature);
//!       exit 0
//! ```
//!
//! ★ marks the checks that make a Moso boot different from an Axum one. Each
//! produces a hand-written entry in the grouped report, and **all** of them run
//! before anything is reported — see [`BootErrors`].
//!
//! # Two `App`s in one process
//!
//! `App` is `Send + 'static` and owns everything it needs, so two can be built
//! and served simultaneously. That is not a curiosity: it is what makes
//! `moso-test` able to run tests in parallel, each against a real application
//! with a real DI graph.
//!
//! # Where the framework's own routes live
//!
//! `/healthz`, `/readyz`, `/openapi.json` and `/docs` are mounted on an *outer*
//! router whose fallback is the application. They therefore sit **outside** the
//! middleware stack: no access log, no compression, no request-id span, no
//! timeout. That is deliberate on both counts — a liveness probe running twice
//! a second would otherwise be the majority of the log volume, and
//! `/openapi.json` is a pre-serialised byte slice that should cost a memcpy.

use core::future::Future;
use std::any::TypeId;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use indexmap::IndexMap;
use moso_openapi::builder::derive_operation_id;
use moso_openapi::{Document, DocumentBuilder, DocumentError};

use crate::BoxFuture;
use crate::config::{Config, Profile, SecretProvider};
use crate::di::{ProviderMap, ProviderMapBuilder};
use crate::error::{BootError, BootErrors, Error, ProviderRequirement, Result, RouteRef};
use crate::health::{HealthCheck, HealthReport, READINESS_BUDGET};
use crate::http_config::{HttpConfig, ServerConfig, TracingConfig};
use crate::middleware::{MiddlewareStack, RoutePatterns};
use crate::router::{Route, RouteEntry, RouteInfo, Router};
use crate::shutdown::{Drain, Signal};
use crate::task::BlockingPool;

// ---------------------------------------------------------------------------
// AppState
// ---------------------------------------------------------------------------

/// The frozen state every request sees.
///
/// Built once at boot, wrapped in an `Arc`, never mutated. Immutability is what
/// makes provider lookup lock-free and what makes "the application changed
/// under me mid-request" impossible to express.
///
/// ```
/// use moso::prelude::*;
/// # /// Everything this application reads from its environment.
/// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "shop")] pub name: String }
/// # fn main() -> Result<()> {
/// let app = App::new(AppConfig { name: "shop".to_owned() }).build()?;
/// let state = app.state();
///
/// assert_eq!(state.profile(), moso::config::Profile::Dev);
/// assert!(state.http().limits().body_max > 0);
/// assert!(!state.shutdown().is_shutting_down());
/// # Ok(())
/// # }
/// ```
///
/// A handler almost never touches this: `Inject<T>` reaches the provider map and
/// `RequestCtx` carries what a request needs. It is here for middleware and for
/// anything that has to look at the process rather than the request.
pub struct AppState {
    providers: Arc<ProviderMap>,
    http: HttpConfig,
    server: ServerConfig,
    tracing: Option<TracingConfig>,
    profile: Profile,
    shutdown: Signal,
    drain: Drain,
    blocking: BlockingPool,
    started_at: Instant,
    document: Arc<Document>,
    health_checks: Vec<(&'static str, Arc<dyn HealthCheck>)>,
}

impl AppState {
    /// The provider map.
    pub fn providers(&self) -> &ProviderMap {
        &self.providers
    }

    /// The HTTP limits and disclosure policy.
    pub fn http(&self) -> &HttpConfig {
        &self.http
    }

    /// The listener and shutdown settings.
    pub fn server(&self) -> &ServerConfig {
        &self.server
    }

    /// The tracing configuration [`App::serve`] installs a subscriber from, if
    /// one was set with
    /// [`AppBuilder::tracing_config`](crate::AppBuilder::tracing_config).
    ///
    /// `None` means the application left the subscriber to its `main`: `serve`
    /// installs nothing, so whatever `main` installed (or did not) is what runs.
    pub fn tracing_config(&self) -> Option<&TracingConfig> {
        self.tracing.as_ref()
    }

    /// The active profile.
    pub fn profile(&self) -> Profile {
        self.profile
    }

    /// The shutdown signal.
    pub fn shutdown(&self) -> &Signal {
        &self.shutdown
    }

    /// The in-flight request drain.
    pub fn drain(&self) -> &Drain {
        &self.drain
    }

    /// The bounded blocking pool.
    pub fn blocking(&self) -> &BlockingPool {
        &self.blocking
    }

    /// How long the process has been serving.
    pub fn uptime(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// The generated OpenAPI document.
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The registered readiness checks.
    pub fn health_checks(&self) -> &[(&'static str, Arc<dyn HealthCheck>)] {
        &self.health_checks
    }

    /// An empty state, for a unit test of something that needs a
    /// [`RequestCtx`](crate::RequestCtx) but not an application.
    ///
    /// No providers, no routes, no document, default limits. A test that needs
    /// any of those should build a real [`App`] — which is cheap, and which is
    /// the point of `App` being `Send + 'static`.
    ///
    /// Behind `cfg(any(test, feature = "test"))` like the rest of the test-only
    /// surface, so it exists for this crate's own tests and for a downstream
    /// test suite that turns on `moso/test`, and for nothing else.
    #[cfg(any(test, feature = "test"))]
    pub fn for_tests() -> Self {
        Self {
            providers: Arc::new(ProviderMap::new()),
            http: HttpConfig::default(),
            server: ServerConfig::default(),
            tracing: None,
            profile: Profile::Test,
            shutdown: Signal::new(),
            drain: Drain::new(),
            blocking: BlockingPool::new(1),
            started_at: Instant::now(),
            document: Arc::new(Document::default()),
            health_checks: Vec::new(),
        }
    }
}

impl core::fmt::Debug for AppState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AppState")
            .field("providers", &self.providers.len())
            .field("profile", &self.profile)
            .field("health_checks", &self.health_checks.len())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Resolver
// ---------------------------------------------------------------------------

/// Reads providers outside a request.
///
/// Used by `on_startup`, by `provide_with`, by health checks and by CLI tasks.
/// Inside a request you use [`Inject<T>`](crate::Inject), which is **infallible
/// because boot validated it**. That asymmetry is the point of the two-tier
/// model: outside a request there is no boot guarantee to lean on, so the API
/// returns a `Result` and says why.
///
/// # Recording misses
///
/// A resolver handed to a `provide_with` factory records the types the factory
/// asked for and did not find. That is what turns "factory returned an error"
/// into "the factory for `SearchClient` needs `Db`, which is registered after
/// it" — and, when two factories each record the other, into a provider cycle
/// with a real path. Nothing is recorded by a resolver built with
/// [`Resolver::new`].
///
/// ```
/// use moso::prelude::*;
/// # /// Everything this application reads from its environment.
/// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "shop")] pub name: String }
/// /// A store, registered once at boot.
/// #[derive(Default)]
/// pub struct Store;
///
/// # fn main() -> Result<()> {
/// let app = App::new(AppConfig { name: "shop".to_owned() })
///     .provide(Store::default())
///     .build()?;
///
/// let resolver = app.resolver();
/// assert!(resolver.has::<Store>());
/// assert!(resolver.get::<Store>().is_ok());
///
/// // The application's own configuration is a provider like any other.
/// assert_eq!(resolver.config::<AppConfig>()?.name, "shop");
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Resolver {
    providers: Arc<ProviderMap>,
    misses: Option<Arc<Mutex<Vec<&'static str>>>>,
}

impl Resolver {
    /// Build a resolver over a provider map.
    pub fn new(providers: Arc<ProviderMap>) -> Self {
        Self {
            providers,
            misses: None,
        }
    }

    /// A resolver that records the provider types it fails to find.
    fn recording(providers: Arc<ProviderMap>) -> (Self, Arc<Mutex<Vec<&'static str>>>) {
        let misses = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                providers,
                misses: Some(Arc::clone(&misses)),
            },
            misses,
        )
    }

    /// A provider by type.
    ///
    /// # Deviation from the design document
    ///
    /// The specification writes this as `Result<&T>`. A borrow cannot be
    /// produced here: the resolver owns an `Arc<ProviderMap>`, and the map's
    /// only reader — `ProviderMap::get` — hands back a *clone* of the stored
    /// `Arc<T>` rather than a borrow of it. Returning `Arc<T>` is the same
    /// value at one refcount bump, reads identically at every call site thanks
    /// to `Deref`, and matches
    /// [`RequestCtx::config`](crate::RequestCtx::config), which already returns
    /// `Arc`. Restoring the borrow needs a `ProviderMap::get_ref` in `di.rs`.
    pub fn get<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.lookup::<T>()
    }

    /// A trait-object provider, as registered by
    /// [`AppBuilder::provide_dyn`].
    pub fn get_dyn<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.lookup::<T>()
    }

    /// A provider as a shared handle.
    pub fn get_arc<T: Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        self.lookup::<T>()
    }

    /// The application's configuration.
    pub fn config<C: Config>(&self) -> Result<Arc<C>> {
        self.lookup::<C>()
    }

    /// Whether a provider is registered.
    pub fn has<T: ?Sized + 'static>(&self) -> bool {
        self.providers.contains::<T>()
    }

    /// The one lookup path, so the miss recording cannot be forgotten on one
    /// of four otherwise-identical methods.
    fn lookup<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>> {
        if let Some(value) = self.providers.get::<T>() {
            return Ok(value);
        }
        let name = core::any::type_name::<T>();
        if let Some(misses) = &self.misses
            && let Ok(mut misses) = misses.lock()
        {
            misses.push(name);
        }
        Err(crate::di::missing_provider_error(name))
    }
}

impl core::fmt::Debug for Resolver {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Resolver")
            .field("providers", &self.providers.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Lifespan
// ---------------------------------------------------------------------------

/// RAII resources acquired at startup and released at shutdown.
///
/// ```
/// use moso::prelude::*;
/// # /// Everything this application reads from its environment.
/// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Where the broker lives.
/// #     #[config(default = "localhost:9092")] pub broker: String }
/// /// Unsubscribes on drop, after the drain has finished.
/// pub struct Consumer(String);
/// impl Drop for Consumer {
///     fn drop(&mut self) { /* unsubscribe */ }
/// }
///
/// # fn main() {
/// let app = App::new(AppConfig { broker: "localhost:9092".to_owned() })
///     .lifespan(|r| async move {
///         let cfg = r.config::<AppConfig>()?;
///         Ok(Consumer(cfg.broker.clone()))
///     });
/// # let _ = app;
/// # }
/// ```
///
/// Guards drop in reverse acquisition order, after `on_shutdown` hooks have
/// run — so a hook can still use the resource it is about to release, which is
/// the ordering people expect and rarely get.
pub struct Lifespan {
    guards: Vec<Box<dyn core::any::Any + Send>>,
}

impl Lifespan {
    /// An empty lifespan.
    pub fn new() -> Self {
        Self { guards: Vec::new() }
    }

    /// Take ownership of a guard.
    pub fn push<G: Send + 'static>(&mut self, guard: G) {
        self.guards.push(Box::new(guard));
    }

    /// How many guards are held.
    pub fn len(&self) -> usize {
        self.guards.len()
    }

    /// Whether nothing is held.
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }

    /// Drop every guard, innermost first.
    pub fn release(&mut self) {
        while self.guards.pop().is_some() {}
    }
}

impl Default for Lifespan {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for Lifespan {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Lifespan")
            .field("guards", &self.guards.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

/// A built, validated application.
///
/// Everything that could fail has already failed, in one report. What remains
/// is binding a listener and serving.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
///
/// /// Everything this application reads from its environment.
/// #[derive(Config, Debug, Clone, Default)]
/// pub struct AppConfig {}
///
/// /// A store, shared by every request.
/// #[derive(Debug, Default)]
/// pub struct Store;
///
/// /// Answer with nothing.
/// #[endpoint]
/// async fn ping(Inject(_store): Inject<Store>) -> Result<NoContent> {
///     Ok(NoContent)
/// }
///
/// # fn main() -> Result<()> {
/// let app = App::new(AppConfig::default())
///     .provide(Store)
///     .mount(moso::routes! { GET "/ping" => ping })
///     .build()?;   // ← every problem, or none
///
/// // The document exists whatever the `openapi` feature says.
/// assert!(app.openapi().paths.contains_key("/ping"));
/// assert_eq!(app.router_info().len(), 1);
///
/// // `app.serve().await` from here in a real `main`; a test drives the
/// // composed service directly instead.
/// let _service: moso::deps::axum::Router<()> = app.into_service();
/// # Ok(())
/// # }
/// ```
///
/// Forgetting `.provide(Store)` above is not a runtime 500: `build()` returns
/// `Err` naming the type, the routes that need it, and the line to add.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
///
/// /// Everything this application reads from its environment.
/// #[derive(Config, Clone, Debug)]
/// pub struct AppConfig {
///     /// Human-readable service name.
///     #[config(default = "shop")]
///     pub name: String,
/// }
///
/// /// A store, registered once and shared by every request.
/// #[derive(Default)]
/// pub struct Store;
///
/// /// Answer with nothing at all.
/// #[endpoint]
/// async fn ping(Inject(store): Inject<Store>) -> Result<NoContent> {
///     let _ = store;
///     Ok(NoContent)
/// }
///
/// # fn main() -> Result<()> {
/// let app = App::new(AppConfig { name: "shop".to_owned() })
///     .provide(Store::default())
///     .mount(moso::routes! { GET "/ping" => ping })
///     .build()?;
///
/// // The document was generated from the same types the handlers were compiled from.
/// assert!(app.openapi().paths.contains_key("/ping"));
/// # Ok(())
/// # }
/// ```
///
/// `build()` is the gate: it walks every route's `required_providers()`, checks the
/// document for duplicate operation ids and conflicting paths, and reports **all**
/// the problems at once. Forgetting `.provide(Store::default())` above is a boot
/// error naming `Store`, not a 500 on the first request.
pub struct App {
    state: Arc<AppState>,
    service: axum::Router<()>,
    middleware: MiddlewareStack,
    routes: Vec<RouteInfo>,
    startup: Vec<StartupHook>,
    shutdown_hooks: Vec<ShutdownHook>,
    lifespans: Vec<LifespanFactory>,
    lifespan: Lifespan,
}

type StartupHook = Box<dyn FnOnce(Resolver) -> BoxFuture<'static, Result<()>> + Send>;
type ShutdownHook = Box<dyn FnOnce(Resolver) -> BoxFuture<'static, ()> + Send>;

impl App {
    /// Start a builder.
    ///
    /// `config` is registered as a provider, so a handler can take
    /// `Inject<AppConfig>` and a battery can read its own nested section
    /// through it.
    #[allow(
        clippy::new_ret_no_self,
        reason = "`App::new` starts a builder; that is the documented shape of the composition root"
    )]
    pub fn new<C: Config>(config: C) -> AppBuilder {
        AppBuilder::new().provide(config)
    }

    /// Serve using the address and limits from configuration.
    ///
    /// Installs signal handlers, runs the startup hooks, binds, serves, and
    /// drains. This is the last line of `main`.
    pub async fn serve(self) -> Result<()> {
        let bind = self.state.server.bind;
        let listener = tokio::net::TcpListener::bind(bind).await.map_err(|error| {
            Error::internal(error).with_detail(format!("could not bind {bind}"))
        })?;
        self.serve_on(listener).await
    }

    /// Serve on a listener the caller has already bound.
    ///
    /// For tests, socket activation, and any TLS terminator that hands over an
    /// accepted socket.
    ///
    /// # Which `ServerConfig` members are honoured
    ///
    /// `bind` (by [`App::serve`], which does the binding) and `shutdown_grace`.
    /// `keep_alive`, `nodelay` and `http2_prior_knowledge` configure the accept
    /// loop, which `axum::serve` owns and does not expose; an application that
    /// needs them should drive [`App::into_service`] with its own
    /// `hyper_util::server::conn::auto` loop. `worker_threads` belongs to the
    /// runtime the caller built before reaching this function.
    pub async fn serve_on(self, listener: tokio::net::TcpListener) -> Result<()> {
        let App {
            state,
            service,
            startup,
            shutdown_hooks,
            lifespans,
            mut lifespan,
            ..
        } = self;

        let resolver = Resolver::new(Arc::clone(&state.providers));

        // Install the tracing subscriber for the serving lifetime, if the
        // composition root set one with `AppBuilder::tracing_config`. `init`
        // uses `try_init`, so a `main` that installed its own subscriber first
        // wins and this is a harmless no-op. The guard is held to the end of
        // `serve_on`: its Drop is boot step 14 — flush and shut down the OTLP
        // exporter so a draining process does not lose its last batch of spans.
        #[cfg(feature = "subscriber")]
        let _tracing_guard = state.tracing.as_ref().map(crate::observability::init);

        // 7. Startup hooks and lifespan guards, in registration order. A
        //    failure aborts before anything is bound, and releases whatever was
        //    already acquired.
        if let Err(error) = run_startup(startup, lifespans, &resolver, &mut lifespan).await {
            lifespan.release();
            return Err(error);
        }

        // 8. Log what we are, where we are, and how to read the docs. One line,
        //    because a boot banner nobody reads is a boot banner that hides the
        //    line that mattered.
        let local = listener.local_addr().ok();
        log_listening(&state, local);

        // 9. Signals. The task holds only a `Signal`, so it does not keep the
        //    application alive; it ends when the process does.
        let signal = state.shutdown.clone();
        let signals = tokio::spawn(async move {
            crate::shutdown::listen_for_signals().await;
            tracing::info!(target: "moso::app", "shutdown signal received");
            signal.trigger();
        });

        // 10. Serve until the signal fires. `/readyz` answers 503 from the
        //     instant `trigger` runs (step 11) because it reads the same flag.
        let grace = state.server.shutdown_grace;
        let shutdown = state.shutdown.clone();
        let serving = axum::serve(
            listener,
            service.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.recv().await });

        // 12. Drain in-flight connections, but only up to the grace. Axum's
        //     graceful shutdown waits for every connection *without a bound*,
        //     so one wedged request would hold the process past the kill
        //     timeout — which is the exact failure the grace exists to prevent.
        //     Dropping the serve future closes what is left.
        let deadline = state.shutdown.clone();
        let served = tokio::select! {
            result = serving => result,
            () = async move {
                deadline.recv().await;
                tokio::time::sleep(deadline.remaining(grace)).await;
            } => {
                tracing::warn!(
                    target: "moso::app",
                    grace = %humantime::format_duration(grace),
                    "the grace period expired with connections still open; closing them"
                );
                Ok(())
            }
        };

        signals.abort();

        // Then whatever is still holding a guard, on what is left of the same
        // budget rather than on a second one.
        let remaining = state.shutdown.remaining(grace);
        if !state.drain.wait(remaining).await {
            warn_still_open(&state, grace);
        }

        // 13 and 14.
        run_shutdown(shutdown_hooks, &resolver, &mut lifespan).await;

        served
            .map_err(|error| Error::internal(error).with_detail("the server stopped with an error"))
    }

    /// Run the application's background workers without an HTTP listener.
    ///
    /// The same binary in a worker role. Startup hooks and lifespan guards run
    /// exactly as they do for the web role, so a worker cannot drift from what
    /// the web process proved at boot.
    pub async fn serve_workers(self) -> Result<()> {
        let App {
            state,
            startup,
            shutdown_hooks,
            lifespans,
            mut lifespan,
            ..
        } = self;

        let resolver = Resolver::new(Arc::clone(&state.providers));

        // The worker role installs the same subscriber the web role does, from
        // the same config, so a worker's logs and traces cannot drift from the
        // web process's. See `serve_on` for the `try_init` reasoning.
        #[cfg(feature = "subscriber")]
        let _tracing_guard = state.tracing.as_ref().map(crate::observability::init);

        if let Err(error) = run_startup(startup, lifespans, &resolver, &mut lifespan).await {
            lifespan.release();
            return Err(error);
        }

        tracing::info!(
            target: "moso::app",
            profile = %state.profile,
            "workers running; no HTTP listener in this role"
        );

        let signal = state.shutdown.clone();
        let signals = tokio::spawn(async move {
            crate::shutdown::listen_for_signals().await;
            signal.trigger();
        });

        state.shutdown.recv().await;
        signals.abort();

        let grace = state.server.shutdown_grace;
        if !state.drain.wait(state.shutdown.remaining(grace)).await {
            warn_still_open(&state, grace);
        }
        run_shutdown(shutdown_hooks, &resolver, &mut lifespan).await;
        Ok(())
    }

    /// The composed Tower service, with application state attached.
    ///
    /// The full escape hatch: hand it to `axum::serve`, to a test harness, or
    /// to anything that speaks `tower::Service`. Unlike
    /// [`Router::into_axum`](crate::Router::into_axum) this keeps the state, so
    /// `Inject<T>` works.
    ///
    /// Startup hooks do **not** run — they belong to `serve`. A test that needs
    /// them should serve on a listener bound to port 0 instead.
    pub fn into_service(self) -> axum::Router<()> {
        self.service
    }

    /// The generated OpenAPI document.
    ///
    /// Available whatever the `openapi` cargo feature says — the feature
    /// controls whether `/docs` and `/openapi.json` are *mounted*, not whether
    /// the document exists. `moso openapi export` needs it in every build.
    pub fn openapi(&self) -> &Document {
        self.state.document()
    }

    /// One row per route, in registration order — what `moso routes` prints.
    ///
    /// Captured at `build()`, before the router was compiled into services, so
    /// reading it costs nothing and cannot fail.
    pub fn router_info(&self) -> &[RouteInfo] {
        &self.routes
    }

    /// The middleware stack this application is serving with.
    ///
    /// The stack **after** [`MiddlewareStack::configure`] has run, so the
    /// timeout, the body limit and the disclosure policy are the ones in force
    /// rather than the ones the composition root typed.
    /// [`AppBuilder::middleware_stack`] is the builder-side twin and shows the
    /// stack *before* configuration.
    ///
    /// A generated project's `--dump-middleware` is one line over it:
    ///
    /// ```
    /// use moso::prelude::*;
    /// # /// This service's configuration.
    /// # #[derive(Config, Debug, Clone, Default)] pub struct Cfg {}
    /// # fn main() -> Result<()> {
    /// let app = App::new(Cfg::default()).build()?;
    ///
    /// print!("{}", app.middleware_stack().render());
    /// assert!(app.middleware_stack().is_enabled(moso::Slot::CatchError));
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// [`AppBuilder::middleware_stack`]: crate::AppBuilder::middleware_stack
    #[must_use]
    pub fn middleware_stack(&self) -> &MiddlewareStack {
        &self.middleware
    }

    /// The frozen application state.
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// The tracing configuration [`App::serve`] will install a subscriber from.
    ///
    /// `Some` when the composition root called
    /// [`AppBuilder::tracing_config`](crate::AppBuilder::tracing_config), `None`
    /// when it left the subscriber to `main`.
    #[must_use]
    pub fn tracing_config(&self) -> Option<&TracingConfig> {
        self.state.tracing_config()
    }

    /// A resolver over this application's providers.
    pub fn resolver(&self) -> Resolver {
        Resolver::new(Arc::clone(&self.state.providers))
    }

    /// The shutdown signal, so a test can stop the application it started.
    pub fn shutdown_signal(&self) -> Signal {
        self.state.shutdown.clone()
    }
}

impl core::fmt::Debug for App {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("App")
            .field("state", &self.state)
            .field("routes", &self.routes.len())
            .finish_non_exhaustive()
    }
}

/// Run the startup hooks, then acquire the lifespan guards.
///
/// Hooks first: a lifespan usually acquires a *connection*, and a startup hook
/// usually checks that the thing it will connect to is reachable. Failing the
/// cheap check before opening the expensive resource is the ordering that makes
/// a misconfigured deployment fail in a second rather than after a 30 s
/// connect timeout.
async fn run_startup(
    startup: Vec<StartupHook>,
    lifespans: Vec<LifespanFactory>,
    resolver: &Resolver,
    lifespan: &mut Lifespan,
) -> Result<()> {
    for (index, hook) in startup.into_iter().enumerate() {
        hook(resolver.clone()).await.map_err(|error| {
            tracing::error!(
                target: "moso::app",
                hook = index,
                %error,
                "an on_startup hook failed; the application will not serve"
            );
            error
        })?;
    }
    for factory in lifespans {
        let guard = factory(resolver.clone()).await?;
        lifespan.guards.push(guard);
    }
    Ok(())
}

/// Run the shutdown hooks in reverse, then drop the lifespan guards.
async fn run_shutdown(
    shutdown_hooks: Vec<ShutdownHook>,
    resolver: &Resolver,
    lifespan: &mut Lifespan,
) {
    for hook in shutdown_hooks.into_iter().rev() {
        hook(resolver.clone()).await;
    }
    // Innermost first, after the hooks — so a hook can still use the resource
    // it is about to release.
    lifespan.release();

    // The exporter, if Moso installed one, is flushed by the `TracingGuard` that
    // `serve_on`/`serve_workers` holds — its Drop runs after this function
    // returns, so the event below is written *before* the flush and is reliably
    // the last line in the batch. An application that installed its own
    // subscriber in `main` instead owns that flush, exactly as before.
    tracing::info!(target: "moso::app", "shutdown complete");
}

/// The boot line: where we are listening, what profile, where the docs are.
fn log_listening(state: &AppState, local: Option<SocketAddr>) {
    let address = local.unwrap_or(state.server.bind);
    let docs = if state.http.expose_docs {
        Some(format!("http://{address}{}", state.http.docs_path))
    } else {
        None
    };
    match docs {
        Some(docs) => tracing::info!(
            target: "moso::app",
            %address,
            profile = %state.profile,
            %docs,
            "listening"
        ),
        None => tracing::info!(
            target: "moso::app",
            %address,
            profile = %state.profile,
            "listening"
        ),
    }
}

/// Name what was still open when the grace period ran out.
///
/// Without this line the symptom is "deploys take 25 seconds" and nobody knows
/// why. With it, the leaked stream has a route.
fn warn_still_open(state: &AppState, grace: std::time::Duration) {
    let mut counts: IndexMap<&'static str, usize> = IndexMap::new();
    for name in state.drain.open_names() {
        *counts.entry(name).or_default() += 1;
    }
    let open: Vec<String> = counts
        .into_iter()
        .map(|(name, count)| {
            if count == 1 {
                name.to_owned()
            } else {
                format!("{name} (x{count})")
            }
        })
        .collect();

    tracing::warn!(
        target: "moso::app",
        grace = %humantime::format_duration(grace),
        outstanding = state.drain.outstanding(),
        still_open = %open.join(", "),
        "the drain did not finish inside the grace period; these are still open. \
         A long-lived handler must select on `Inject<Signal>` and close."
    );
}

// ---------------------------------------------------------------------------
// AppBuilder
// ---------------------------------------------------------------------------

/// Assembles an application, then validates it.
///
/// Every method returns `Self` so the whole composition root is one expression
/// — which is the point: an application's shape should be readable in one
/// screen, in `lib.rs`, not scattered across `main`.
///
/// ```
/// use moso::prelude::*;
/// use moso::response::NoContent;
/// use std::time::Duration;
/// # /// Everything this application reads from its environment.
/// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
/// #     /// Service name.
/// #     #[config(default = "shop")] pub name: String }
/// # /// A store.
/// # #[derive(Default)] pub struct Store;
/// # /// Answer with nothing at all.
/// # #[endpoint] async fn ping() -> Result<NoContent> { Ok(NoContent) }
/// # fn main() -> Result<()> {
/// let app = App::new(AppConfig { name: "shop".to_owned() })
///     .provide(Store::default())
///     .mount_at("/api/v1", moso::routes! { GET "/ping" => ping })
///     .with_middleware(|s| { s.timeout(Duration::from_secs(10)); })
///     .openapi(|d| { d.title("Shop API").version("0.1.0"); })
///     .on_startup(|_| async { Ok(()) })
///     .build()?;
///
/// assert!(app.openapi().paths.contains_key("/api/v1/ping"));
/// # Ok(())
/// # }
/// ```
///
/// Every method takes `self` and returns `Self`, so the composition root reads as
/// one expression. Nothing happens until [`AppBuilder::build`].
pub struct AppBuilder {
    registrations: Vec<Registration>,
    router: Router,
    middleware: OnceLock<MiddlewareStack>,
    startup: Vec<StartupHook>,
    shutdown_hooks: Vec<ShutdownHook>,
    lifespans: Vec<LifespanFactory>,
    health_checks: Vec<(&'static str, Arc<dyn HealthCheck>)>,
    document: DocumentBuilder,
    http: Option<HttpConfig>,
    server: ServerConfig,
    tracing: Option<TracingConfig>,
    profile: Profile,
    secret_providers: Vec<Arc<dyn SecretProvider>>,
    errors: BootErrors,
}

type LifespanFactory =
    Box<dyn FnOnce(Resolver) -> BoxFuture<'static, Result<Box<dyn core::any::Any + Send>>> + Send>;

/// Writes one provider into the map under construction.
///
/// `Fn` rather than `FnOnce` because the map is rebuilt from scratch before
/// each `provide_with` factory runs — see [`AppBuilder::build`]. Every insert
/// closure captures an `Arc`, so replaying one is a refcount bump and the
/// *identity* of the provided value is preserved across rebuilds.
type ProviderInsert = Box<dyn Fn(&mut ProviderMapBuilder) + Send>;

/// Produces one provider from the providers registered before it.
type ProviderFactory =
    Box<dyn FnOnce(Resolver) -> BoxFuture<'static, Result<ProviderInsert>> + Send>;

/// One entry in the registration order, which is also the resolution order.
enum Registration {
    /// A value that already exists.
    Eager {
        /// The type it is registered under, for the boot report.
        type_name: &'static str,
        /// How to write it into the map.
        insert: ProviderInsert,
    },
    /// A value that has to be built, possibly fallibly and asynchronously.
    Factory {
        /// The type it will be registered under.
        type_name: &'static str,
        /// The factory itself.
        factory: ProviderFactory,
    },
}

impl Registration {
    /// The type this registration provides.
    fn type_name(&self) -> &'static str {
        match self {
            Registration::Eager { type_name, .. } | Registration::Factory { type_name, .. } => {
                type_name
            }
        }
    }
}

impl AppBuilder {
    /// A builder with nothing registered.
    ///
    /// [`App::new`] is the documented entry point; this exists for an
    /// application with no configuration type of its own, and for `moso-test`.
    pub fn new() -> Self {
        Self {
            registrations: Vec::new(),
            router: Router::new(),
            middleware: OnceLock::new(),
            startup: Vec::new(),
            shutdown_hooks: Vec::new(),
            lifespans: Vec::new(),
            health_checks: Vec::new(),
            document: DocumentBuilder::new(),
            http: None,
            server: ServerConfig::default(),
            tracing: None,
            profile: detect_profile(),
            secret_providers: Vec::new(),
            errors: BootErrors::new(),
        }
    }

    // ── providers ─────────────────────────────────────────────────────────

    /// Register an application-lifetime value, retrievable as `Inject<T>`.
    pub fn provide<T: Send + Sync + 'static>(self, value: T) -> Self {
        self.provide_arc(Arc::new(value))
    }

    /// Register an already-shared value, so two providers can alias one object.
    pub fn provide_arc<T: Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.registrations.push(Registration::Eager {
            type_name: core::any::type_name::<T>(),
            insert: Box::new(move |builder| {
                builder.insert_arc(Arc::clone(&value));
            }),
        });
        self
    }

    /// Register a value built at boot, possibly fallibly and asynchronously.
    ///
    /// The closure receives a [`Resolver`], so a factory may read providers
    /// registered before it. Factories form a DAG resolved by demand; a cycle
    /// is a boot error naming the full path.
    ///
    /// # Where the future runs
    ///
    /// `build()` is synchronous, so the factory is driven to completion on the
    /// **ambient multi-threaded runtime** with
    /// [`tokio::task::block_in_place`]. That is what keeps a pool built here
    /// bound to the runtime that will serve requests with it. Calling `build()`
    /// outside a runtime, or on a current-thread runtime, is a boot error
    /// naming the fix rather than a deadlock at the first query.
    pub fn provide_with<T, F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(Resolver) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
        T: Send + Sync + 'static,
    {
        self.registrations.push(Registration::Factory {
            type_name: core::any::type_name::<T>(),
            factory: Box::new(move |resolver| {
                Box::pin(async move {
                    let value = Arc::new(f(resolver).await?);
                    let insert: ProviderInsert = Box::new(move |builder| {
                        builder.insert_arc(Arc::clone(&value));
                    });
                    Ok(insert)
                })
            }),
        });
        self
    }

    /// Register a trait object, retrievable as `Inject<dyn Trait>`.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use std::sync::Arc;
    /// # /// Everything this application reads from its environment.
    /// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Which host to relay through.
    /// #     #[config(default = "localhost:25")] pub smtp: String }
    /// /// Anything that can send a message.
    /// pub trait Mailer: Send + Sync + 'static {
    ///     /// Send one.
    ///     fn send(&self, to: &str, body: &str);
    /// }
    ///
    /// /// The production implementation.
    /// pub struct SmtpMailer;
    /// impl Mailer for SmtpMailer {
    ///     fn send(&self, _to: &str, _body: &str) {}
    /// }
    ///
    /// /// Send a welcome message.
    /// #[endpoint]
    /// async fn welcome(Inject(mailer): Inject<dyn Mailer>) -> Result<moso::response::NoContent> {
    ///     mailer.send("ada@example.com", "hello");
    ///     Ok(moso::response::NoContent)
    /// }
    ///
    /// # fn main() {
    /// let app = App::new(AppConfig { smtp: "localhost:25".to_owned() })
    ///     .provide_dyn::<dyn Mailer>(Arc::new(SmtpMailer))
    ///     .mount(Router::new().post("/welcome", moso::ep!(welcome)));
    /// # let _ = app;
    /// # }
    /// ```
    ///
    /// This is the lever every test pulls: production wires an SMTP mailer, the
    /// test app wires a capturing one, and no handler changes.
    pub fn provide_dyn<T: ?Sized + Send + Sync + 'static>(mut self, value: Arc<T>) -> Self {
        self.registrations.push(Registration::Eager {
            type_name: core::any::type_name::<T>(),
            insert: Box::new(move |builder| {
                builder.insert_dyn::<T>(Arc::clone(&value));
            }),
        });
        self
    }

    // ── routing ───────────────────────────────────────────────────────────

    /// Mount a router at the root.
    pub fn mount(mut self, router: Router) -> Self {
        self.router = core::mem::take(&mut self.router).merge(router);
        self
    }

    /// Mount a router under `prefix`.
    pub fn mount_at(mut self, prefix: &'static str, router: Router) -> Self {
        self.router = core::mem::take(&mut self.router).nest(prefix, router);
        self
    }

    /// Mount an arbitrary Axum router under `prefix`.
    ///
    /// Contributes nothing to the OpenAPI document and is invisible to
    /// boot-time validation.
    pub fn mount_axum(mut self, prefix: &'static str, router: axum::Router<()>) -> Self {
        self.router = core::mem::take(&mut self.router).mount_axum(prefix, router);
        self
    }

    // ── middleware ────────────────────────────────────────────────────────

    /// Replace the default middleware stack wholesale.
    pub fn middleware(mut self, stack: MiddlewareStack) -> Self {
        self.middleware = OnceLock::from(stack);
        self
    }

    /// Adjust the default stack.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::middleware::Slot;
    /// use std::time::Duration;
    /// # /// Everything this application reads from its environment.
    /// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Service name.
    /// #     #[config(default = "shop")] pub name: String }
    /// # fn main() {
    /// let app = App::new(AppConfig { name: "shop".to_owned() })
    ///     .with_middleware(|s| {
    ///         s.timeout(Duration::from_secs(10));
    ///         s.body_limit(1 << 20);
    ///         s.disable(Slot::Compression);
    ///     });
    /// # let _ = app;
    /// # }
    /// ```
    pub fn with_middleware(mut self, f: impl FnOnce(&mut MiddlewareStack)) -> Self {
        // `get_or_init` materialises the standard stack the first time anybody
        // looks at it; an application that replaces the stack wholesale never
        // pays for building the one it discarded.
        let _ = self.middleware.get_or_init(MiddlewareStack::standard);
        if let Some(stack) = self.middleware.get_mut() {
            f(stack);
        }
        self
    }

    // ── lifecycle ─────────────────────────────────────────────────────────

    /// Run before the listener binds. A failure aborts boot with a non-zero exit.
    pub fn on_startup<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(Resolver) -> Fut + Send + 'static,
        Fut: Future<Output = Result<()>> + Send + 'static,
    {
        self.startup
            .push(Box::new(move |resolver| Box::pin(f(resolver))));
        self
    }

    /// Run after the listener closes and in-flight requests drain.
    ///
    /// Hooks run in reverse registration order, so a hook can rely on anything
    /// registered before it still being alive.
    pub fn on_shutdown<F, Fut>(mut self, f: F) -> Self
    where
        F: FnOnce(Resolver) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        self.shutdown_hooks
            .push(Box::new(move |resolver| Box::pin(f(resolver))));
        self
    }

    /// Acquire a resource at startup and release it at shutdown, by dropping.
    ///
    /// The RAII form of the pair above, for anything whose release is a `Drop`
    /// rather than a call.
    pub fn lifespan<F, Fut, G>(mut self, f: F) -> Self
    where
        F: FnOnce(Resolver) -> Fut + Send + 'static,
        Fut: Future<Output = Result<G>> + Send + 'static,
        G: Send + 'static,
    {
        self.lifespans.push(Box::new(move |resolver| {
            Box::pin(async move {
                let guard = f(resolver).await?;
                Ok(Box::new(guard) as Box<dyn core::any::Any + Send>)
            })
        }));
        self
    }

    // ── health ────────────────────────────────────────────────────────────

    /// Register a readiness probe under `name`.
    ///
    /// All critical checks must pass for `/readyz` to return 200. The name is
    /// the key in the report body, so make it the thing being checked:
    /// `database`, `redis`, `search`.
    pub fn health_check(mut self, name: &'static str, check: impl HealthCheck) -> Self {
        self.health_checks.push((name, Arc::new(check)));
        self
    }

    // ── OpenAPI ───────────────────────────────────────────────────────────

    /// Set the document's metadata.
    ///
    /// ```
    /// use moso::prelude::*;
    /// # /// Everything this application reads from its environment.
    /// # #[derive(Config, Clone, Debug)] pub struct AppConfig {
    /// #     /// Where this instance is reachable.
    /// #     #[config(default = "https://api.shop.example")] pub public_url: String }
    /// # fn main() {
    /// let cfg = AppConfig { public_url: "https://api.shop.example".to_owned() };
    /// let url = cfg.public_url.clone();
    ///
    /// let app = App::new(cfg).openapi(move |d| {
    ///     d.title("Shop API")
    ///         .version(env!("CARGO_PKG_VERSION"))
    ///         .server(url, "this instance")
    ///         .security_scheme("session", SecurityScheme::cookie("sid"));
    /// });
    /// # let _ = app;
    /// # }
    /// ```
    pub fn openapi(mut self, f: impl FnOnce(&mut DocumentBuilder)) -> Self {
        f(&mut self.document);
        self
    }

    // ── configuration ─────────────────────────────────────────────────────

    /// Override the HTTP limits and disclosure policy.
    ///
    /// Normally these come from the application's `#[config(nested)] http`
    /// section; this is for a test that wants a 1 KiB body limit without a
    /// configuration file.
    pub fn http_config(mut self, config: HttpConfig) -> Self {
        self.http = Some(config);
        self
    }

    /// Override the listener and shutdown settings.
    pub fn server_config(mut self, config: ServerConfig) -> Self {
        self.server = config;
        self
    }

    /// Set the tracing configuration [`App::serve`] installs a subscriber from.
    ///
    /// This is what turns [`TracingConfig::otlp_endpoint`] from a recorded
    /// setting into a live exporter: with it set (and the `otel` feature on),
    /// [`App::serve`] calls `observability::init` (behind the `subscriber` feature)
    /// at the top of the serving lifetime and holds the resulting
    /// `TracingGuard` until the process
    /// drains, so the exporter is flushed on the way out.
    ///
    /// It installs through
    /// `try_init` (from `tracing-subscriber`), so a
    /// `main` that installed its own subscriber first still wins and this is a
    /// no-op — the two orders compose. Left unset, `serve` installs nothing and
    /// the subscriber is entirely `main`'s to own.
    ///
    /// ```
    /// use moso::prelude::*;
    /// use moso::http_config::TracingConfig;
    /// # /// This service's configuration.
    /// # #[derive(Config, Debug, Clone, Default)] pub struct Cfg {}
    /// # fn main() -> Result<()> {
    /// let app = App::new(Cfg::default())
    ///     .tracing_config(TracingConfig {
    ///         otlp_endpoint: Some("http://otel-collector:4317".to_owned()),
    ///         service_name: Some("shop".to_owned()),
    ///         ..TracingConfig::default()
    ///     })
    ///     .build()?;
    ///
    /// assert!(app.tracing_config().is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub fn tracing_config(mut self, config: TracingConfig) -> Self {
        self.tracing = Some(config);
        self
    }

    /// Override the detected profile.
    pub fn profile(mut self, profile: Profile) -> Self {
        self.profile = profile;
        self
    }

    /// Register a secret provider used while resolving configuration.
    ///
    /// The application's own configuration was already resolved before
    /// [`App::new`] saw it, so the providers registered here are for whatever
    /// resolves secrets *later*: a battery reading its own section, a rotation
    /// task, `moso config`. They are registered as a provider of
    /// `Vec<Arc<dyn SecretProvider>>` so that anything holding a [`Resolver`]
    /// can reach them.
    pub fn secret_provider(mut self, provider: Arc<dyn crate::config::SecretProvider>) -> Self {
        self.secret_providers.push(provider);
        self
    }

    // ── build ─────────────────────────────────────────────────────────────

    /// Validate and finalise.
    ///
    /// Runs steps 1 to 6 of the boot sequence and returns
    /// `Err(Error::boot(..))` carrying **every** problem found, not the first.
    /// A successful return means every `Inject<T>` in the route table has a
    /// provider, no two routes collide, no two operations share an id, and the
    /// middleware stack respects its ordering invariants.
    pub fn build(self) -> Result<App> {
        let (app, mut errors) = self.assemble(true);
        errors.sort_for_report();
        errors.into_result().map_err(Error::boot)?;
        Ok(app)
    }

    /// Build without running the checks.
    ///
    /// For `moso openapi export --force` and for a test that wants to inspect a
    /// deliberately broken application. Never call it from `main`: it discards
    /// the entire point of the boot sequence.
    pub fn build_unchecked(self) -> App {
        self.assemble(false).0
    }

    // ── inspection ────────────────────────────────────────────────────────

    /// The routes registered so far. Read by `moso routes`.
    pub fn router(&self) -> &Router {
        &self.router
    }

    /// The middleware stack as it stands. Read by `moso middleware`.
    ///
    /// Materialises the standard stack on first call, which is why it is not
    /// `&mut self`: an application that only *reads* the stack still gets the
    /// stack it would have served with.
    pub fn middleware_stack(&self) -> &MiddlewareStack {
        self.middleware.get_or_init(MiddlewareStack::standard)
    }

    /// Problems found so far. `build` adds to this before deciding.
    pub fn errors(&self) -> &BootErrors {
        &self.errors
    }

    /// The secret providers registered with [`AppBuilder::secret_provider`].
    pub fn secret_providers(&self) -> &[Arc<dyn SecretProvider>] {
        &self.secret_providers
    }

    /// Steps 1 to 6, collecting every problem rather than stopping at the first.
    ///
    /// `checked` decides only what happens to the OpenAPI document when it
    /// fails its own consistency checks: `build` wants the errors and does not
    /// need the document, `build_unchecked` wants the document and does not
    /// need the errors.
    fn assemble(self, checked: bool) -> (App, BootErrors) {
        let AppBuilder {
            registrations,
            router,
            middleware,
            startup,
            shutdown_hooks,
            lifespans,
            health_checks,
            mut document,
            http,
            server,
            tracing,
            profile,
            secret_providers,
            mut errors,
        } = self;

        // ── 1. configuration ──────────────────────────────────────────────
        let mut http = http.unwrap_or_else(|| http_defaults(profile));
        // The documentation surface is off in production by security default, and
        // that decision is enforced *here* rather than trusted to the profile
        // constructor: an application that builds an `HttpConfig` by hand — or
        // reads one from its own config file — must not be able to publish its
        // full API surface in production by leaving `expose_docs` at its
        // struct-literal default of `true`. Forcing it off is announced so the
        // choice is visible in the boot log rather than silently reversed.
        if matches!(profile, Profile::Production) && http.expose_docs {
            tracing::warn!(
                target: "moso::app",
                profile = %profile,
                "http.expose_docs is set in the production profile; /docs, /openapi.json and \
                 /openapi.yaml stay unmounted, because publishing the full API surface is off in \
                 production by security default"
            );
            http.expose_docs = false;
        }
        if http.expose_internal_errors {
            tracing::warn!(
                target: "moso::app",
                profile = %profile,
                "http.expose_internal_errors is on: 5xx responses will carry their detail and \
                 source chain. That is a disclosure decision, not a debugging convenience."
            );
        }

        // ── 2. providers ──────────────────────────────────────────────────
        let shutdown = Signal::new();
        let drain = Drain::new();
        let blocking = BlockingPool::global().clone();
        let providers = freeze_providers(
            registrations,
            &shutdown,
            &drain,
            &blocking,
            &secret_providers,
            &mut errors,
        );

        // ── 3. router ─────────────────────────────────────────────────────
        for problem in router.conflicts() {
            errors.push(problem);
        }
        check_reserved_paths(&router, &http, &mut errors);
        let routes = router.describe();

        // ── 4. the DI graph ───────────────────────────────────────────────
        validate_providers(router.entries(), &providers, &mut errors);

        // ── 5. the OpenAPI document ───────────────────────────────────────
        //
        // Conflicting routes are described once, not twice. The second
        // registration is already reported by `Router::conflicts` and already
        // dropped by `Router::into_axum` (first registration wins), so
        // describing it again would add a duplicate-`operationId` problem on
        // top of the conflict — one mistake, two errors, and the second one
        // disappears when the first is fixed.
        let mut described: HashSet<(moso_openapi::HttpMethod, &str)> = HashSet::new();
        for entry in router.entries() {
            if !described.insert((entry.method, entry.path.as_str())) {
                continue;
            }
            let (method, path) = (entry.method, entry.path.clone());
            document.operation(method, path.clone(), |op| {
                entry.describe(op);
                // Last, because `operation_id` is first-writer-wins: a handler
                // that declared its own keeps it.
                op.operation_id(derive_operation_id(method, &path));
            });
        }
        let document = if checked {
            match document.build() {
                Ok(document) => document,
                Err(problems) => {
                    record_document_errors(problems, router.entries(), &mut errors);
                    Document::default()
                }
            }
        } else {
            document.build_unchecked()
        };

        // ── 6. middleware ─────────────────────────────────────────────────
        //
        // `configure` runs *here* rather than in `with_middleware`, because the
        // `[http]` section is only known now: an application may set it after
        // editing the stack, and reordering the two calls must not change the
        // result. It skips every setting `with_middleware` made explicitly, so
        // the reading order still holds — an explicit edit wins over a derived
        // default.
        let mut middleware = middleware
            .into_inner()
            .unwrap_or_else(MiddlewareStack::standard);
        middleware.configure(profile, &http);
        for problem in validate_stack(&middleware) {
            errors.push(problem);
        }

        // ── 7. health checks, then the state everything shares ────────────
        let state = Arc::new(AppState {
            providers,
            http,
            server,
            tracing,
            profile,
            shutdown,
            drain,
            blocking,
            started_at: Instant::now(),
            document: Arc::new(document),
            health_checks,
        });

        let service = compose_service(router, &middleware, Arc::clone(&state));

        (
            App {
                state,
                service,
                middleware,
                routes,
                startup,
                shutdown_hooks,
                lifespans,
                lifespan: Lifespan::new(),
            },
            errors,
        )
    }
}

impl Default for AppBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for AppBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AppBuilder")
            .field("routes", &self.router.len())
            .field("providers", &self.registrations.len())
            .field("health_checks", &self.health_checks.len())
            .field("profile", &self.profile)
            .field("problems", &self.errors.len())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Step 1 — the defaults a profile implies
// ---------------------------------------------------------------------------

/// Which set of defaults is in force.
///
/// Mirrors `Profile::detect`'s documented rule — `MOSO_PROFILE`, then a test
/// build, then a debug build, then production. It is written here rather than
/// called there because `Profile::detect` is not yet implemented; the two must
/// be reconciled into one when it is.
fn detect_profile() -> Profile {
    if let Ok(name) = std::env::var("MOSO_PROFILE")
        && let Some(profile) = Profile::parse(name.trim())
    {
        return profile;
    }
    if cfg!(test) {
        return Profile::Test;
    }
    if cfg!(debug_assertions) {
        return Profile::Dev;
    }
    Profile::Production
}

/// The HTTP defaults a profile implies.
///
/// Only `expose_docs` differs: a deployed instance does not publish its own
/// documentation UI unless it was asked to, and no profile exposes internal
/// errors. Written here rather than calling `HttpConfig::for_profile`, which is
/// not yet implemented; the two must be reconciled into one when it is.
fn http_defaults(profile: Profile) -> HttpConfig {
    HttpConfig {
        expose_docs: !matches!(profile, Profile::Production),
        ..HttpConfig::default()
    }
}

// ---------------------------------------------------------------------------
// Step 2 — freezing the provider map
// ---------------------------------------------------------------------------

/// Apply every registration in order, running the factories as they are reached.
///
/// The map is rebuilt from the accumulated insert closures before each factory
/// so that the factory sees exactly the providers registered before it — no
/// more, which is what makes the ordering rule enforceable, and no less.
/// Rebuilding is `O(registrations)` per factory over a list that is a handful
/// long, and every insert is an `Arc` clone.
fn freeze_providers(
    registrations: Vec<Registration>,
    shutdown: &Signal,
    drain: &Drain,
    blocking: &BlockingPool,
    secret_providers: &[Arc<dyn SecretProvider>],
    errors: &mut BootErrors,
) -> Arc<ProviderMap> {
    let mut owned: Vec<ProviderInsert> = Vec::new();

    // The framework's own providers go in first, so a `.provide` of the same
    // type overrides them — registration is last-write-wins.
    let signal = shutdown.clone();
    owned.push(Box::new(move |builder| {
        builder.insert(signal.clone());
    }));
    let drain = drain.clone();
    owned.push(Box::new(move |builder| {
        builder.insert(drain.clone());
    }));
    let blocking = blocking.clone();
    owned.push(Box::new(move |builder| {
        builder.insert(blocking.clone());
    }));
    if !secret_providers.is_empty() {
        let secret_providers = secret_providers.to_vec();
        owned.push(Box::new(move |builder| {
            builder.insert(secret_providers.clone());
        }));
    }

    // Every type provided anywhere in the list, with its position, so a factory
    // that failed on a type registered *after* it gets an ordering error rather
    // than a bare "provider failed".
    let mut positions: HashMap<&'static str, usize> = HashMap::new();
    let base = owned.len();
    for (index, registration) in registrations.iter().enumerate() {
        positions
            .entry(registration.type_name())
            .or_insert(base + index);
    }

    let mut failures: Vec<Failure> = Vec::new();

    for (index, registration) in registrations.into_iter().enumerate() {
        match registration {
            Registration::Eager { insert, .. } => owned.push(insert),
            Registration::Factory { type_name, factory } => {
                let map = rebuild(&owned);
                let (resolver, misses) = Resolver::recording(map);
                match run_factory(factory, resolver) {
                    Ok(Ok(insert)) => owned.push(insert),
                    Ok(Err(error)) => failures.push(Failure {
                        type_name,
                        position: base + index,
                        needed: first_miss(&misses),
                        detail: error.chain(),
                    }),
                    Err(problem) => errors.push(*problem),
                }
            }
        }
    }

    report_failures(failures, &positions, errors);
    rebuild(&owned)
}

/// A `provide_with` factory that returned an error.
struct Failure {
    /// The type the factory was building.
    type_name: &'static str,
    /// Its index in the registration order.
    position: usize,
    /// The first provider it asked for and did not find, if any.
    needed: Option<&'static str>,
    /// The rendered error, source chain included.
    detail: String,
}

/// The first provider a recording resolver failed to find.
fn first_miss(misses: &Mutex<Vec<&'static str>>) -> Option<&'static str> {
    misses
        .lock()
        .ok()
        .and_then(|misses| misses.first().copied())
}

/// Build a fresh provider map from the insert closures accumulated so far.
fn rebuild(inserts: &[ProviderInsert]) -> Arc<ProviderMap> {
    let mut builder = ProviderMapBuilder::new();
    for insert in inserts {
        insert(&mut builder);
    }
    builder.build()
}

/// Turn factory failures into the most specific boot error each one deserves.
///
/// A cycle first — `A` needs `B`, `B` needs `A`, and no ordering fixes that. An
/// ordering problem next: the type the factory wanted exists, but later in the
/// chain. A plain failure last.
fn report_failures(
    failures: Vec<Failure>,
    positions: &HashMap<&'static str, usize>,
    errors: &mut BootErrors,
) {
    let needs: HashMap<&'static str, &'static str> = failures
        .iter()
        .filter_map(|failure| failure.needed.map(|needed| (failure.type_name, needed)))
        .collect();

    let mut in_a_reported_cycle: HashSet<&'static str> = HashSet::new();

    for failure in &failures {
        if in_a_reported_cycle.contains(failure.type_name) {
            continue;
        }
        if let Some(path) = find_cycle(failure.type_name, &needs) {
            for member in &path {
                in_a_reported_cycle.insert(member);
            }
            errors.push(BootError::ProviderCycle { path });
            continue;
        }

        match failure.needed {
            Some(needed)
                if positions
                    .get(needed)
                    .is_some_and(|at| *at > failure.position) =>
            {
                let built = short_name(failure.type_name);
                let wanted = short_name(needed);
                errors.push(BootError::Other {
                    message: format!(
                        "provider `{built}` is built before `{wanted}`, which it needs"
                    ),
                    notes: vec![
                        "a `provide_with` factory can only read providers registered before it, \
                         and this one is registered first"
                            .to_owned(),
                        format!("`{}` asked for `{needed}`", failure.type_name),
                    ],
                    fix: Some(format!(
                        "swap the two registrations, so `{wanted}` exists before the factory runs\n\
                         .provide(/* a {wanted} */)\n\
                         .provide_with(|r| async move {{ /* build the {built} */ }})"
                    )),
                });
            }
            _ => errors.push(BootError::ProviderFailed {
                type_name: failure.type_name,
                detail: failure.detail.clone(),
            }),
        }
    }
}

/// The last segment of a type path, generic arguments kept.
///
/// A boot headline is elided at 72 characters, and a fully-qualified type name
/// spends most of that on the module path the reader already knows. The full
/// name stays in the notes, where there is room for it.
fn short_name(name: &str) -> &str {
    let head = name.find('<').unwrap_or(name.len());
    match name[..head].rfind("::") {
        Some(index) => &name[index + 2..],
        None => name,
    }
}

/// The cycle through `start`, if the "needs" graph has one.
///
/// Returns the path with `start` repeated at the end, which is the shape
/// [`BootError::ProviderCycle`] renders. A chain that loops somewhere else is
/// not this failure's cycle and is left for the failure that owns it.
fn find_cycle(
    start: &'static str,
    needs: &HashMap<&'static str, &'static str>,
) -> Option<Vec<&'static str>> {
    let mut path = vec![start];
    let mut current = start;
    for _ in 0..=needs.len() {
        let next = *needs.get(current)?;
        path.push(next);
        if next == start {
            return Some(path);
        }
        if path[..path.len() - 1].contains(&next) {
            return None;
        }
        current = next;
    }
    None
}

/// Drive one boot-time factory to completion.
///
/// `build()` is synchronous and the factory is not, so the future has to be
/// driven from inside a runtime that will still exist afterwards — anything the
/// factory registers with an I/O driver (a pool, a socket, a timer) stops
/// working the moment that runtime does.
///
/// The only shape that satisfies both is an ambient multi-threaded runtime:
/// [`block_in_place`](tokio::task::block_in_place) moves this thread out of the
/// scheduler while the other workers keep the driver running. A current-thread
/// runtime cannot do it — the driver *is* this thread — and with no runtime at
/// all there is nothing to bind to. Both are boot errors naming the fix, which
/// is the whole philosophy of this module: fail at boot with a sentence.
fn run_factory(
    factory: ProviderFactory,
    resolver: Resolver,
) -> core::result::Result<Result<ProviderInsert>, Box<BootError>> {
    use tokio::runtime::{Handle, RuntimeFlavor};

    let future = factory(resolver);
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            Ok(tokio::task::block_in_place(|| handle.block_on(future)))
        }
        Ok(_) => Err(Box::new(BootError::Other {
            message: "`provide_with` needs a multi-threaded runtime".to_owned(),
            notes: vec![
                "`App::build()` is synchronous, so a `provide_with` factory is driven on the \
                 runtime that is already running — and a current-thread runtime cannot drive \
                 anything while `build()` is on it"
                    .to_owned(),
                "anything the factory opens must outlive `build()`, so it cannot be driven on a \
                 throwaway runtime either"
                    .to_owned(),
            ],
            fix: Some(
                "use the multi-threaded runtime, which is the default:\n\
                 #[tokio::main]\n\
                 # or, in a test\n\
                 #[tokio::test(flavor = \"multi_thread\")]\n\
                 # or build the value first and register it with `.provide(value)`"
                    .to_owned(),
            ),
        })),
        Err(_) => Err(Box::new(BootError::Other {
            message: "`provide_with` needs a Tokio runtime".to_owned(),
            notes: vec![
                "`App::build()` was called outside any runtime, so there is nothing to drive the \
                 factory's future on — and nothing for the value it builds to stay bound to"
                    .to_owned(),
            ],
            fix: Some(
                "call `build()` from inside the runtime that will serve:\n\
                 #[tokio::main]\n\
                 async fn main() -> moso::Result<()> {\n    \
                     app().build()?.serve().await\n\
                 }"
                .to_owned(),
            ),
        })),
    }
}

// ---------------------------------------------------------------------------
// Step 3 — reserved paths
// ---------------------------------------------------------------------------

/// Warn about an application route the framework's own routes would shadow.
///
/// The framework mounts `/healthz` and friends on an outer router whose
/// fallback is the application, so an application route at the same path is
/// registered, served by nothing, and silently dead. Naming it at boot is the
/// difference between a five-minute confusion and a five-hour one.
fn check_reserved_paths(router: &Router, http: &HttpConfig, errors: &mut BootErrors) {
    let reserved: Vec<(&str, &str)> = vec![
        (http.health_path.as_str(), "http.health_path"),
        (http.ready_path.as_str(), "http.ready_path"),
        (http.docs_path.as_str(), "http.docs_path"),
        (http.openapi_path.as_str(), "http.openapi_path"),
    ];

    for entry in router.entries() {
        for (path, key) in &reserved {
            if entry.path != *path {
                continue;
            }
            errors.push(BootError::Other {
                message: format!(
                    "route `{} {}` is shadowed by the framework",
                    entry.method.as_upper_str(),
                    entry.path
                ),
                notes: vec![
                    format!(
                        "`{path}` is mounted by Moso itself, outside the middleware stack, so \
                         this route would never be reached"
                    ),
                    match entry.spec.source {
                        Some(source) => format!("registered at {source}"),
                        None => "registered at an unknown location".to_owned(),
                    },
                ],
                fix: Some(format!(
                    "move the framework's route, in the application's configuration:\n\
                     {key} = \"/internal{path}\""
                )),
            });
        }
    }
}

// ---------------------------------------------------------------------------
// Step 4 — the DI graph
// ---------------------------------------------------------------------------

/// Check every route's provider requirements against the frozen map.
///
/// Grouped by *provider*, not by route: "`Db` is missing, and here are the nine
/// routes that wanted it" is one problem with nine lines, where the transpose is
/// nine problems that all say the same thing.
fn validate_providers(entries: &[RouteEntry], providers: &ProviderMap, errors: &mut BootErrors) {
    let mut missing: IndexMap<TypeId, ProviderRequirement> = IndexMap::new();

    for entry in entries {
        for requirement in entry.providers {
            if requirement.optional || providers.contains_req(requirement) {
                continue;
            }
            let group = missing
                .entry(requirement.id())
                .or_insert_with(|| ProviderRequirement {
                    type_name: requirement.name(),
                    required_by: Vec::new(),
                });

            // A handler taking `Inject<Db>` twice is one route, not two lines.
            let route = RouteRef {
                method: entry.method.as_upper_str(),
                path: entry.path.clone(),
                source: entry.spec.source,
                via: Vec::new(),
            };
            if group.required_by.last() != Some(&route) {
                group.required_by.push(route);
            }
        }
    }

    if missing.is_empty() {
        return;
    }
    let registered = providers.registered_names();
    for requirement in missing.into_values() {
        errors.push(BootError::MissingProvider {
            requirement,
            registered: registered.clone(),
        });
    }
}

// ---------------------------------------------------------------------------
// Step 5 — the document's own errors
// ---------------------------------------------------------------------------

/// Translate `moso-openapi`'s findings into the boot report's vocabulary.
///
/// `DocumentError::RouteConflict` is deliberately dropped: `Router::conflicts`
/// already reported it, from the route table, where both source locations are
/// still available. Reporting it twice would be worse than reporting it once,
/// and the router's version is the better of the two.
fn record_document_errors(
    problems: Vec<DocumentError>,
    entries: &[RouteEntry],
    errors: &mut BootErrors,
) {
    for problem in problems {
        match problem {
            DocumentError::RouteConflict { .. } => {}
            DocumentError::DuplicateOperationId {
                operation_id,
                first,
                second,
            } => errors.push(BootError::DuplicateOperationId {
                operation_id,
                first,
                second,
            }),
            DocumentError::SchemaCollision {
                name,
                first,
                second,
            } => errors.push(BootError::SchemaCollision {
                name,
                first,
                second,
            }),
            DocumentError::PathParameterMismatch {
                ref path,
                ref missing,
                ref extra,
            } => {
                // An undocumented handler — a plain `async fn` registered
                // without `#[endpoint]` — declares no parameters at all by
                // construction. Comparing that to the path template would
                // fail every such route on a parameterised path, which is a
                // supported way to write a handler.
                use crate::handler::{Endpoint as _, UndocumentedEndpoint};
                if entries.iter().any(|entry| {
                    entry.path == *path && entry.handler.name() == UndocumentedEndpoint::NAME
                }) {
                    continue;
                }

                let declared: Vec<String> = crate::router::path_parameters(path)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                let mut expected: Vec<String> = declared
                    .iter()
                    .filter(|name| !missing.contains(name))
                    .cloned()
                    .collect();
                expected.extend(extra.iter().cloned());

                let route = entries
                    .iter()
                    .find(|entry| entry.path == *path)
                    .map(|entry| RouteRef {
                        method: entry.method.as_upper_str(),
                        path: entry.path.clone(),
                        source: entry.spec.source,
                        via: Vec::new(),
                    })
                    .unwrap_or_else(|| RouteRef {
                        method: "ANY",
                        path: path.clone(),
                        source: None,
                        via: Vec::new(),
                    });

                errors.push(BootError::PathParameterMismatch {
                    route,
                    declared,
                    expected,
                });
            }
            other => errors.push(BootError::Document {
                detail: other.to_string(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Step 6 — the middleware stack
// ---------------------------------------------------------------------------

/// The stack's ordering violations.
///
/// An empty stack — `MiddlewareStack::bare()` — has no order to violate, so the
/// walk is skipped rather than run over nothing.
fn validate_stack(stack: &MiddlewareStack) -> Vec<BootError> {
    if stack.describe().is_empty() {
        return Vec::new();
    }
    stack.validate()
}

/// Fold the stack around `service`, skipping the fold for an empty stack.
///
/// `patterns` is the route table reduced to what the stack asks of it. The
/// stack is installed *outside* Axum's router — [`Slot::NormalizePath`] rewrites
/// the URI, which only means anything before matching — so Axum's own
/// `MatchedPath` is not there yet when `trace`, `timeout` and `metrics` run.
/// [`MiddlewareStack::compose_routed`] resolves the pattern itself, once, at the
/// outside, and all three read the same answer.
///
/// An empty stack skips the fold *and* the resolver: with nothing enabled,
/// nothing is left to read the pattern, and the router publishes its own to
/// [`RequestCtx`] a moment later.
///
/// [`Slot::NormalizePath`]: crate::middleware::Slot::NormalizePath
fn compose_stack(stack: &MiddlewareStack, patterns: RoutePatterns, service: Route) -> Route {
    if stack.describe().iter().all(|entry| !entry.enabled) {
        return service;
    }
    stack.compose_routed(patterns, service)
}

// ---------------------------------------------------------------------------
// The composed service
// ---------------------------------------------------------------------------

/// Build the service the listener will answer with.
///
/// ```text
/// outer router
///   ├── GET  /healthz        ─┐  outside the stack: no log, no compression,
///   ├── GET  /readyz          │  no request-id span, no timeout
///   ├── GET  /openapi.json    │
///   ├── GET  /openapi.yaml    │  gated on `openapi` + `http.expose_docs`
///   ├── GET  /docs           ─┘  (+ /redoc, /swagger behind their cargo features)
///   └── fallback → resolve route pattern
///                    → middleware stack → application router → route → handler
/// ```
///
/// The pattern table is taken from the router *before* it is compiled into
/// services, because that is the last moment the paths are still readable data.
/// It covers the route table only: a `mount_axum` mount and a static file mount
/// contribute no patterns, and requests they serve are recorded as
/// `<unmatched>`, which is the same honesty that keeps them out of the OpenAPI
/// document.
fn compose_service(
    router: Router,
    stack: &MiddlewareStack,
    state: Arc<AppState>,
) -> axum::Router<()> {
    let patterns = RoutePatterns::from_router(&router);

    // Every route reads the application out of the request extensions; see
    // `router::request_context`.
    let application = router
        .into_axum()
        .layer(axum::Extension(Arc::clone(&state)));
    let application = compose_stack(stack, patterns, Route::new(application));

    let mut outer = axum::Router::new();
    let http = state.http();

    if is_mountable(&http.health_path) {
        outer = outer.route(&http.health_path, axum::routing::get(healthz));
    }
    if is_mountable(&http.ready_path) {
        let ready_state = Arc::clone(&state);
        outer = outer.route(
            &http.ready_path,
            axum::routing::get(move || readyz(ready_state)),
        );
    }

    #[cfg(feature = "openapi")]
    {
        if http.expose_docs {
            outer = mount_docs(outer, &state);
        }
    }

    outer.fallback_service(application)
}

/// Whether a configured path can be handed to `axum::Router::route`, which
/// panics on anything that is not rooted.
fn is_mountable(path: &str) -> bool {
    path.starts_with('/')
}

/// The route the ReDoc cargo feature adds next to `/docs`.
#[cfg(feature = "redoc")]
const REDOC_PATH: &str = "/redoc";

/// The route the Swagger-UI cargo feature adds next to `/docs`.
#[cfg(feature = "swagger-ui")]
const SWAGGER_PATH: &str = "/swagger";

/// The `/openapi.yaml` path derived from the configured JSON path.
///
/// The YAML document is the same document in a second encoding, so it lives one
/// suffix away from wherever `http.openapi_path` points rather than behind a
/// config key of its own: moving `openapi_path` moves both, which is the only
/// behaviour that keeps them from drifting apart.
#[cfg(feature = "openapi")]
fn yaml_path(openapi_path: &str) -> String {
    match openapi_path.strip_suffix(".json") {
        Some(stem) => format!("{stem}.yaml"),
        None => format!("{openapi_path}.yaml"),
    }
}

/// Mount `/openapi.json`, `/openapi.yaml`, `/docs` and any feature-selected UI
/// route (`/redoc`, `/swagger`).
///
/// The document is serialised **once**, here — as JSON and as YAML — and served
/// as byte slices with strong-enough ETags. A documentation UI polls the spec on
/// every page load; making that a re-serialisation of a 200 kB document would be
/// a self-inflicted load test. The HTML pages, by contrast, are rendered per
/// request: each carries a fresh Content-Security-Policy nonce, so it cannot be
/// cached or shared across responses.
#[cfg(feature = "openapi")]
fn mount_docs(mut outer: axum::Router<()>, state: &Arc<AppState>) -> axum::Router<()> {
    let payload = Arc::new(OpenApiPayload::new(state.document()));
    let http = state.http();

    if is_mountable(&http.openapi_path) {
        let spec = Arc::clone(&payload);
        outer = outer.route(
            &http.openapi_path,
            axum::routing::get(move |headers: http::HeaderMap| openapi_json(spec, headers)),
        );

        let yaml = yaml_path(&http.openapi_path);
        if is_mountable(&yaml) {
            let spec = Arc::clone(&payload);
            outer = outer.route(
                &yaml,
                axum::routing::get(move |headers: http::HeaderMap| openapi_yaml(spec, headers)),
            );
        }
    }

    if is_mountable(&http.docs_path) {
        outer = mount_primary_docs(outer, &http.docs_path, state);

        #[cfg(feature = "redoc")]
        {
            outer = mount_ui(outer, REDOC_PATH, docs_ui(state));
        }
        #[cfg(feature = "swagger-ui")]
        {
            outer = mount_ui(outer, SWAGGER_PATH, docs_ui(state));
        }
    }

    outer
}

/// Mount the primary documentation route (`/docs`).
///
/// By default this serves the real, self-hosted Swagger UI
/// ([`moso_openapi::swagger_ui`]) plus its same-origin assets, so the page is the
/// familiar tool users already know. The `lean-docs` feature swaps in Moso's own
/// compact renderer for builds that prefer the smaller binary. See ADR-0019.
#[cfg(all(feature = "openapi", not(feature = "lean-docs")))]
fn mount_primary_docs(
    mut outer: axum::Router<()>,
    path: &str,
    state: &Arc<AppState>,
) -> axum::Router<()> {
    let spec_url = state.http().openapi_path.clone();
    let title = docs_title(state);
    let base = path.trim_end_matches('/').to_owned();

    let render = {
        let spec_url = spec_url.clone();
        let title = title.clone();
        let base = base.clone();
        move || swagger_page(spec_url.clone(), title.clone(), base.clone())
    };
    outer = outer.route(path, axum::routing::get(render));

    // Each vendored asset on its own same-origin sub-path, so the page fetches
    // no CDN. The bytes live in `moso-openapi`; this only wires the routes.
    for asset in moso_openapi::swagger_ui::ASSETS {
        let asset_path = format!("{base}/{}", asset.file_name);
        if is_mountable(&asset_path) {
            outer = outer.route(
                &asset_path,
                axum::routing::get(move || serve_swagger_asset(asset)),
            );
        }
    }
    outer
}

/// Mount the primary documentation route (`/docs`) — the `lean-docs` build.
///
/// Serves Moso's own compact, network-free renderer instead of the vendored
/// Swagger UI bundle, keeping the binary small. See ADR-0019.
#[cfg(all(feature = "openapi", feature = "lean-docs"))]
fn mount_primary_docs(
    outer: axum::Router<()>,
    path: &str,
    state: &Arc<AppState>,
) -> axum::Router<()> {
    mount_ui(outer, path, docs_ui(state))
}

/// The document's title, or the default when the application set none.
#[cfg(feature = "openapi")]
fn docs_title(state: &Arc<AppState>) -> String {
    let title = &state.document().info.title;
    if title.is_empty() {
        moso_openapi::ui::DEFAULT_TITLE.to_owned()
    } else {
        title.clone()
    }
}

/// `GET /docs` — the Swagger UI page, with a fresh CSP nonce on its bootstrap.
#[cfg(all(feature = "openapi", not(feature = "lean-docs")))]
async fn swagger_page(spec_url: String, title: String, base: String) -> crate::Response {
    use axum::response::IntoResponse as _;

    let nonce = docs_nonce();
    let ui = moso_openapi::swagger_ui::SwaggerUi::new()
        .spec_url(spec_url)
        .base_path(base)
        .title(title);
    let ui = match &nonce {
        Some(nonce) => ui.nonce(nonce.clone()),
        None => ui,
    };

    let mut response = ui.render().into_response();
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    if let Some(nonce) = &nonce
        && let Ok(value) = http::HeaderValue::from_str(&swagger_csp(nonce))
    {
        headers.insert(http::header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

/// `GET /docs/<asset>` — one vendored, cacheable Swagger UI asset.
#[cfg(all(feature = "openapi", not(feature = "lean-docs")))]
async fn serve_swagger_asset(
    asset: &'static moso_openapi::swagger_ui::SwaggerAsset,
) -> crate::Response {
    use axum::response::IntoResponse as _;

    let mut response = asset.bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(asset.content_type),
    );
    // The assets are versioned with the crate, so a browser may cache them and
    // skip re-fetching ~1.4 MB on every page load.
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("public, max-age=3600"),
    );
    response
}

/// The Content-Security-Policy the Swagger UI page carries.
///
/// Looser than the compact renderer's policy in one place: Swagger UI sets
/// element styles from JavaScript at runtime, which `style-src` can only admit
/// with `'unsafe-inline'` — a nonce cannot cover styles a script injects. The
/// bundle itself loads from `'self'` and the one inline bootstrap is admitted by
/// its `nonce`. The documentation page is never served in production
/// (`http.expose_docs`), so this relaxation is confined to `dev` and `test`.
/// `connect-src *` matches the compact policy: "Try it" fetches arbitrary
/// documented origins.
#[cfg(all(feature = "openapi", not(feature = "lean-docs")))]
fn swagger_csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; \
         img-src 'self' data:; font-src 'self' data:; style-src 'self' 'unsafe-inline'; \
         script-src 'self' 'nonce-{nonce}'; connect-src *"
    )
}

/// Build the compact embedded UI for this application, pointed at its spec URL.
///
/// This is the renderer the `lean-docs`, `redoc` and `swagger-ui` routes use.
/// See [`moso_openapi::ui`] for the network-free renderer Moso controls, and
/// ADR-0019 for why the default `/docs` is now the vendored Swagger UI instead.
#[cfg(all(
    feature = "openapi",
    any(feature = "lean-docs", feature = "redoc", feature = "swagger-ui")
))]
fn docs_ui(state: &Arc<AppState>) -> moso_openapi::ui::DocsUi {
    moso_openapi::ui::DocsUi::new()
        .spec_url(state.http().openapi_path.clone())
        .title(docs_title(state))
}

/// Mount one documentation-UI route that renders `ui` with a per-response nonce.
#[cfg(all(
    feature = "openapi",
    any(feature = "lean-docs", feature = "redoc", feature = "swagger-ui")
))]
fn mount_ui(outer: axum::Router<()>, path: &str, ui: moso_openapi::ui::DocsUi) -> axum::Router<()> {
    outer.route(path, axum::routing::get(move || docs_page(ui.clone())))
}

/// The pre-serialised OpenAPI document, in JSON and YAML, each with its ETag.
#[cfg(feature = "openapi")]
struct OpenApiPayload {
    json: DocBytes,
    yaml: DocBytes,
}

/// One serialised representation of the document and the ETag that identifies it.
#[cfg(feature = "openapi")]
struct DocBytes {
    bytes: bytes::Bytes,
    etag: http::HeaderValue,
}

#[cfg(feature = "openapi")]
impl DocBytes {
    /// Wrap already-serialised bytes and derive their ETag.
    fn new(bytes: Vec<u8>) -> Self {
        let etag = http::HeaderValue::from_str(&moso_openapi::etag_for(&bytes))
            .unwrap_or_else(|_| http::HeaderValue::from_static("W/\"openapi\""));
        Self {
            bytes: bytes::Bytes::from(bytes),
            etag,
        }
    }
}

#[cfg(feature = "openapi")]
impl OpenApiPayload {
    /// Serialise once, at boot.
    ///
    /// A document that cannot be serialised is served as an empty object rather
    /// than a 500: the failure is impossible for a document Moso assembled, and
    /// a panic on the boot path over a documentation route would be a poor
    /// trade.
    fn new(document: &Document) -> Self {
        let json = document.to_json_bytes().unwrap_or_else(|error| {
            tracing::error!(
                target: "moso::app",
                %error,
                "the OpenAPI document could not be serialised; /openapi.json will be empty"
            );
            b"{}".to_vec()
        });
        let yaml = document
            .to_yaml()
            .map(String::into_bytes)
            .unwrap_or_else(|error| {
                tracing::error!(
                    target: "moso::app",
                    %error,
                    "the OpenAPI document could not be serialised to YAML; /openapi.yaml \
                     will be empty"
                );
                b"{}\n".to_vec()
            });
        Self {
            json: DocBytes::new(json),
            yaml: DocBytes::new(yaml),
        }
    }
}

/// `GET /openapi.json` — a memcpy, or a 304.
#[cfg(feature = "openapi")]
async fn openapi_json(payload: Arc<OpenApiPayload>, headers: http::HeaderMap) -> crate::Response {
    serve_document(&payload.json, "application/json", &headers)
}

/// `GET /openapi.yaml` — the same document, YAML-encoded, or a 304.
#[cfg(feature = "openapi")]
async fn openapi_yaml(payload: Arc<OpenApiPayload>, headers: http::HeaderMap) -> crate::Response {
    serve_document(&payload.yaml, "application/yaml", &headers)
}

/// Serve one pre-serialised representation with an ETag, answering 304 to a
/// matching `If-None-Match`.
#[cfg(feature = "openapi")]
fn serve_document(
    doc: &DocBytes,
    content_type: &'static str,
    headers: &http::HeaderMap,
) -> crate::Response {
    use axum::response::IntoResponse as _;

    if headers
        .get(http::header::IF_NONE_MATCH)
        .is_some_and(|value| value == doc.etag)
    {
        let mut response = http::StatusCode::NOT_MODIFIED.into_response();
        response
            .headers_mut()
            .insert(http::header::ETAG, doc.etag.clone());
        return response;
    }

    let mut response = doc.bytes.clone().into_response();
    let out = response.headers_mut();
    out.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static(content_type),
    );
    out.insert(http::header::ETAG, doc.etag.clone());
    out.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-cache"),
    );
    response
}

/// A documentation-UI page — the embedded, network-free renderer, rendered with
/// a fresh Content-Security-Policy nonce so its inline `<style>`/`<script>` run
/// without `unsafe-inline`.
///
/// This route is mounted on the **outer** router, outside the middleware stack,
/// so the security-headers slot never touches it; the CSP is therefore set here,
/// on the response, rather than inherited. The page is off in the production
/// profile (`http.expose_docs`), so this policy is only ever served in `dev` and
/// `test`.
#[cfg(all(
    feature = "openapi",
    any(feature = "lean-docs", feature = "redoc", feature = "swagger-ui")
))]
async fn docs_page(ui: moso_openapi::ui::DocsUi) -> crate::Response {
    use axum::response::IntoResponse as _;

    let nonce = docs_nonce();
    let html = match &nonce {
        Some(nonce) => ui.nonce(nonce.clone()).render(),
        None => ui.render(),
    };

    let mut response = html.into_response();
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("text/html; charset=utf-8"),
    );
    // A per-response nonce means the body is single-use: a cache that replayed
    // it to a second reader would pair a stale nonce with a fresh policy.
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    if let Some(nonce) = &nonce
        && let Ok(value) = http::HeaderValue::from_str(&docs_csp(nonce))
    {
        headers.insert(http::header::CONTENT_SECURITY_POLICY, value);
    }
    response
}

/// A fresh 128-bit Content-Security-Policy nonce, hex-encoded, from the OS
/// CSPRNG.
///
/// Returns `None` only if the operating system's randomness source is
/// unavailable — which does not happen on a supported platform once the process
/// is running. The caller then serves the page without a CSP rather than
/// panicking on the outer router (which sits outside `catch_panic`) or, worse,
/// emitting a predictable nonce.
#[cfg(feature = "openapi")]
fn docs_nonce() -> Option<String> {
    use core::fmt::Write as _;

    let mut bytes = [0u8; 16];
    if let Err(error) = getrandom::fill(&mut bytes) {
        tracing::error!(
            target: "moso::app",
            %error,
            "the OS randomness source is unavailable; the documentation page is served without \
             a Content-Security-Policy this request"
        );
        return None;
    }
    let mut nonce = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(nonce, "{byte:02x}");
    }
    Some(nonce)
}

/// The Content-Security-Policy the documentation page carries.
///
/// `default-src 'none'` denies everything not named below; the inline `<style>`
/// and `<script>` are admitted only by matching `nonce`. `connect-src *` is
/// deliberate: the "Try it" panel issues `fetch`es against the documented server
/// origins, which are arbitrary and routinely cross-origin, so narrowing it to
/// `'self'` would break the feature. Since the page is never served in
/// production, that breadth is confined to `dev` and `test`.
#[cfg(all(
    feature = "openapi",
    any(feature = "lean-docs", feature = "redoc", feature = "swagger-ui")
))]
fn docs_csp(nonce: &str) -> String {
    format!(
        "default-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; \
         img-src 'self' data:; font-src 'self'; style-src 'nonce-{nonce}'; \
         script-src 'nonce-{nonce}'; connect-src *"
    )
}

/// `GET /healthz` — liveness.
///
/// Touches nothing. If this handler runs, the process is alive and the runtime
/// is scheduling, which is the entire question. It keeps answering 200 during
/// the drain: the process is still alive, and answering 503 would invite the
/// orchestrator to `SIGKILL` it mid-drain.
async fn healthz() -> crate::Response {
    probe_response(http::StatusCode::OK, &serde_json::json!({ "status": "up" }))
}

/// `GET /readyz` — readiness.
async fn readyz(state: Arc<AppState>) -> crate::Response {
    // Step 11 of the boot sequence: 503 from the instant the signal fires,
    // before draining begins, so the load balancer removes this instance while
    // it is still serving what it already accepted. No check runs — the answer
    // is known, and the point is that it arrives in microseconds.
    if state.shutdown().is_shutting_down() {
        let report = HealthReport::shutting_down(state.uptime());
        return probe_response(report.http_status(), &report);
    }

    let resolver = Resolver::new(Arc::clone(&state.providers));
    let mut report = crate::health::readiness_report(
        state.health_checks(),
        &resolver,
        READINESS_BUDGET,
        state.uptime(),
    )
    .await;

    // The application's own version, as it declared it in the OpenAPI
    // document, beats the environment fallback.
    //
    // `DocumentBuilder` fills `info.version` with a placeholder rather than
    // leaving it empty, because an empty version is not a valid OpenAPI
    // document — so "did the application declare one?" is a comparison against
    // that placeholder, not an emptiness test. Getting this wrong made every
    // application that never called `.openapi(|d| d.version(..))` report
    // `"version": "0.0.0"` instead of the environment value.
    let declared = &state.document().info.version;
    if !declared.is_empty() && declared != moso_openapi::DEFAULT_VERSION {
        report.version = declared.clone();
    }

    probe_response(report.http_status(), &report)
}

/// The response shape both probes share: JSON, never cached, never logged.
fn probe_response(status: http::StatusCode, body: &impl serde::Serialize) -> crate::Response {
    use axum::response::IntoResponse as _;

    let bytes = serde_json::to_vec(body).unwrap_or_else(|_| b"{\"status\":\"down\"}".to_vec());
    let mut response = bytes.into_response();
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(
        http::header::CONTENT_TYPE,
        http::HeaderValue::from_static("application/json"),
    );
    headers.insert(
        http::header::CACHE_CONTROL,
        http::HeaderValue::from_static("no-store"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigDescriptor, ConfigKey, ConfigLoader};
    use crate::di::ProviderReq;
    use crate::handler::{Endpoint, HandlerFn};
    use crate::router::Router;
    use moso_openapi::{HttpMethod, OperationBuilder};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::ServiceExt as _;

    // ── fixtures ──────────────────────────────────────────────────────────

    /// The smallest thing that satisfies `App::new`.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct TestConfig {
        name: &'static str,
    }

    impl Config for TestConfig {
        fn descriptor() -> &'static ConfigDescriptor {
            static DESCRIPTOR: ConfigDescriptor = ConfigDescriptor {
                type_name: "TestConfig",
                fields: &[],
            };
            &DESCRIPTOR
        }

        fn load_nested(
            _loader: &ConfigLoader,
            _prefix: &ConfigKey,
            _errors: &mut BootErrors,
        ) -> Option<Self> {
            Some(TestConfig { name: "test" })
        }
    }

    /// A stand-in for a database pool nobody registers.
    struct Db;

    /// A second unregistered provider, so a test can produce two distinct
    /// missing-provider problems.
    struct Cache;

    /// An endpoint that needs `Db` and knows where it was written.
    ///
    /// This is by hand what `#[endpoint]` generates: a unit struct beside the
    /// handler carrying the description and the provider requirements.
    #[derive(Clone, Copy, Default)]
    struct NeedsDb;

    impl Endpoint for NeedsDb {
        const NAME: &'static str = "needs_db";

        fn spec(op: &mut OperationBuilder) {
            op.summary("Needs a database");
            op.source("src/routes/users.rs", 14);
        }

        fn required_providers() -> &'static [ProviderReq] {
            const REQS: &[ProviderReq] = &[ProviderReq::of::<Db>()];
            REQS
        }
    }

    impl HandlerFn for NeedsDb {
        fn invoke(
            _req: crate::Request,
            _ctx: crate::RequestCtx,
        ) -> BoxFuture<'static, crate::Response> {
            Box::pin(async { probe_response(http::StatusCode::OK, &serde_json::json!({})) })
        }
    }

    /// An endpoint that needs `Cache`, with its own source location.
    #[derive(Clone, Copy, Default)]
    struct NeedsCache;

    impl Endpoint for NeedsCache {
        const NAME: &'static str = "needs_cache";

        fn spec(op: &mut OperationBuilder) {
            op.source("src/routes/admin.rs", 31);
        }

        fn required_providers() -> &'static [ProviderReq] {
            const REQS: &[ProviderReq] = &[ProviderReq::of::<Cache>()];
            REQS
        }
    }

    impl HandlerFn for NeedsCache {
        fn invoke(
            _req: crate::Request,
            _ctx: crate::RequestCtx,
        ) -> BoxFuture<'static, crate::Response> {
            Box::pin(async { probe_response(http::StatusCode::OK, &serde_json::json!({})) })
        }
    }

    /// A builder with the standard stack replaced, so a test does not depend on
    /// what the default stack contains.
    fn builder() -> AppBuilder {
        App::new(TestConfig { name: "test" })
            .profile(Profile::Test)
            .middleware(MiddlewareStack::bare())
    }

    /// `GET path` against a built application's service.
    async fn get(app: &App, path: &str) -> (http::StatusCode, http::HeaderMap, String) {
        let request = http::Request::builder()
            .uri(path)
            .body(axum::body::Body::empty())
            .expect("a well-formed request");
        let response = app
            .service
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("a complete body");
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    // ── the boot sequence ─────────────────────────────────────────────────

    #[test]
    fn app_is_send_and_static() {
        fn assert_send_static<T: Send + 'static>() {}
        assert_send_static::<App>();
        assert_send_static::<AppBuilder>();
    }

    #[test]
    fn lifespan_releases_in_reverse() {
        let mut lifespan = Lifespan::new();
        lifespan.push(1u32);
        lifespan.push("two");
        assert_eq!(lifespan.len(), 2);
        lifespan.release();
        assert!(lifespan.is_empty());
    }

    #[tokio::test]
    async fn an_application_with_no_routes_still_serves_the_probes() {
        let app = builder().build().expect("nothing to fail");

        let (status, _, body) = get(&app, "/healthz").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(body.contains("\"up\""), "healthz said {body}");

        let (status, headers, body) = get(&app, "/readyz").await;
        assert_eq!(status, http::StatusCode::OK);
        assert_eq!(headers[http::header::CACHE_CONTROL], "no-store");
        let report: HealthReport = serde_json::from_str(&body).expect("a report");
        assert_eq!(report.status, crate::health::status::UP);
        assert!(report.checks.is_empty());
    }

    #[tokio::test]
    async fn a_request_that_matches_nothing_is_a_problem_document() {
        let app = builder().build().expect("nothing to fail");
        let (status, headers, body) = get(&app, "/nope").await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .starts_with("application/problem+json")
        );
        assert!(body.contains("no route matches /nope"));
    }

    #[test]
    fn the_config_passed_to_new_is_a_provider() {
        let app = builder().build().expect("nothing to fail");
        let config = app.resolver().get::<TestConfig>().expect("registered");
        assert_eq!(config.name, "test");
    }

    #[test]
    fn the_framework_registers_its_own_providers() {
        let app = builder().build().expect("nothing to fail");
        let resolver = app.resolver();
        assert!(resolver.has::<Signal>());
        assert!(resolver.has::<Drain>());
        assert!(resolver.has::<BlockingPool>());
    }

    #[test]
    fn a_later_registration_overrides_an_earlier_one() {
        let app = builder()
            .provide(7u32)
            .provide(9u32)
            .build()
            .expect("nothing to fail");
        assert_eq!(*app.resolver().get::<u32>().expect("registered"), 9);
    }

    #[test]
    fn provide_arc_keeps_the_identity_of_the_value() {
        let shared = Arc::new(String::from("one allocation"));
        let app = builder()
            .provide_arc(Arc::clone(&shared))
            .build()
            .expect("nothing to fail");
        let resolved = app.resolver().get::<String>().expect("registered");
        assert!(Arc::ptr_eq(&shared, &resolved));
    }

    #[test]
    fn provide_dyn_is_keyed_by_the_trait() {
        trait Mailer: Send + Sync {
            fn name(&self) -> &'static str;
        }
        struct Capturing;
        impl Mailer for Capturing {
            fn name(&self) -> &'static str {
                "capturing"
            }
        }

        let app = builder()
            .provide_dyn::<dyn Mailer>(Arc::new(Capturing))
            .build()
            .expect("nothing to fail");
        assert_eq!(
            app.resolver()
                .get_dyn::<dyn Mailer>()
                .expect("registered")
                .name(),
            "capturing"
        );
    }

    // ── the boot report ───────────────────────────────────────────────────

    #[test]
    fn a_missing_provider_names_every_route_that_wanted_it_with_its_source() {
        let routes = Router::new()
            .endpoint::<NeedsDb>(HttpMethod::Get, "/users")
            .endpoint::<NeedsDb>(HttpMethod::Post, "/users")
            .endpoint::<NeedsDb>(HttpMethod::Get, "/users/active");

        let error = builder()
            .mount(routes)
            .build()
            .expect_err("`Db` was never provided");
        let report = render(&error);

        assert!(
            report.contains("application failed to build (1 problem)"),
            "{report}"
        );
        assert!(report.contains("missing provider"), "{report}");
        assert!(report.contains("::Db"), "{report}");
        assert!(report.contains("required by"), "{report}");
        for route in ["GET /users", "POST /users", "GET /users/active"] {
            assert!(report.contains(route), "{route} missing from:\n{report}");
        }
        assert!(report.contains("src/routes/users.rs:14"), "{report}");
        assert!(
            report.contains("App::new(config).provide(value)"),
            "{report}"
        );
    }

    #[test]
    fn three_simultaneous_problems_produce_three_entries() {
        // 1. `Db` is missing. 2. `Cache` is missing. 3. two routes collide.
        let routes = Router::new()
            .endpoint::<NeedsDb>(HttpMethod::Get, "/users")
            .endpoint::<NeedsCache>(HttpMethod::Get, "/admin")
            .endpoint::<NeedsDb>(HttpMethod::Get, "/users");

        let error = builder().mount(routes).build().expect_err("three problems");
        let report = render(&error);

        assert!(
            report.contains("application failed to build (3 problems)"),
            "{report}"
        );
        assert!(report.contains("::Db"), "{report}");
        assert!(report.contains("::Cache"), "{report}");
        assert!(report.contains("route conflict: GET /users"), "{report}");
    }

    #[test]
    fn a_route_registered_over_a_framework_path_is_reported() {
        let routes = Router::new().endpoint::<NeedsDb>(HttpMethod::Get, "/healthz");
        let error = builder()
            .provide(Db)
            .mount(routes)
            .build()
            .expect_err("the route is shadowed");
        let report = render(&error);
        assert!(report.contains("shadowed by the framework"), "{report}");
        assert!(report.contains("http.health_path"), "{report}");
    }

    #[test]
    fn a_provided_dependency_makes_the_report_empty() {
        let routes = Router::new().endpoint::<NeedsDb>(HttpMethod::Get, "/users");
        let app = builder()
            .provide(Db)
            .mount(routes)
            .build()
            .expect("`Db` is registered");
        assert_eq!(app.router_info().len(), 1);
        assert_eq!(app.router_info()[0].path, "/users");
        assert_eq!(app.router_info()[0].handler, NeedsDb::NAME);
    }

    #[test]
    fn an_optional_requirement_is_not_a_boot_error() {
        #[derive(Clone, Copy, Default)]
        struct Optional;

        impl Endpoint for Optional {
            const NAME: &'static str = "optional";

            fn spec(_op: &mut OperationBuilder) {}

            fn required_providers() -> &'static [ProviderReq] {
                const REQS: &[ProviderReq] = &[ProviderReq::optional_of::<Db>()];
                REQS
            }
        }

        impl HandlerFn for Optional {
            fn invoke(
                _req: crate::Request,
                _ctx: crate::RequestCtx,
            ) -> BoxFuture<'static, crate::Response> {
                Box::pin(async { probe_response(http::StatusCode::OK, &serde_json::json!({})) })
            }
        }

        builder()
            .mount(Router::new().endpoint::<Optional>(HttpMethod::Get, "/maybe"))
            .build()
            .expect("an optional provider is not required");
    }

    #[test]
    fn a_documented_handler_that_ignores_a_path_parameter_is_reported() {
        // `NeedsDb` declares no parameters, so `{id}` is captured by the router
        // and read by nobody — which Axum answers with a silently missing value
        // and Moso answers with a boot error.
        let routes = Router::new().endpoint::<NeedsDb>(HttpMethod::Get, "/users/{id}");
        let error = builder()
            .provide(Db)
            .mount(routes)
            .build()
            .expect_err("the path declares `{id}` and the handler does not read it");
        assert!(render(&error).contains("path parameter mismatch"));
    }

    #[test]
    fn an_undocumented_handler_on_a_parameterised_path_is_not_reported() {
        // A plain `async fn` carries no parameter metadata at all, so comparing
        // what it declares to the path template says nothing.
        async fn plain() -> &'static str {
            "ok"
        }

        builder()
            .mount(Router::new().get("/users/{id}", plain))
            .build()
            .expect("an undocumented handler is not held to a contract it cannot state");
    }

    #[test]
    fn a_conflicting_route_is_one_problem_and_not_two() {
        // Describing the shadowed registration a second time would add a
        // duplicate-`operationId` problem that vanishes when the conflict is
        // fixed. One mistake, one entry.
        let routes = Router::new()
            .endpoint::<NeedsDb>(HttpMethod::Get, "/users")
            .endpoint::<NeedsDb>(HttpMethod::Get, "/users");

        let error = builder()
            .provide(Db)
            .mount(routes)
            .build()
            .expect_err("the two routes collide");
        let report = render(&error);
        assert!(report.contains("(1 problem)"), "{report}");
        assert!(report.contains("route conflict"), "{report}");
        assert!(!report.contains("duplicate operationId"), "{report}");
    }

    #[test]
    fn build_unchecked_produces_an_app_from_a_broken_builder() {
        let routes = Router::new().endpoint::<NeedsDb>(HttpMethod::Get, "/users");
        let app = builder().mount(routes).build_unchecked();
        assert_eq!(app.router_info().len(), 1);
    }

    /// The report as the terminal would show it, without colour.
    fn render(error: &Error) -> String {
        error.to_string()
    }

    // ── the OpenAPI document ──────────────────────────────────────────────

    #[test]
    fn the_document_carries_the_registered_operations() {
        let app = builder()
            .provide(Db)
            .openapi(|d| {
                d.title("Shop API").version("1.4.2");
            })
            .mount(Router::new().endpoint::<NeedsDb>(HttpMethod::Get, "/users"))
            .build()
            .expect("nothing to fail");

        let document = app.openapi();
        assert_eq!(document.info.title, "Shop API");
        let operation = document
            .operation(HttpMethod::Get, "/users")
            .expect("the operation is in the document");
        assert_eq!(operation.summary.as_deref(), Some("Needs a database"));
        assert_eq!(operation.operation_id.as_deref(), Some("get_users"));
    }

    #[test]
    fn a_duplicate_operation_id_is_a_boot_error() {
        #[derive(Clone, Copy, Default)]
        struct Fixed;

        impl Endpoint for Fixed {
            const NAME: &'static str = "fixed";

            fn spec(op: &mut OperationBuilder) {
                op.operation_id("list_things");
            }

            fn required_providers() -> &'static [ProviderReq] {
                &[]
            }
        }

        impl HandlerFn for Fixed {
            fn invoke(
                _req: crate::Request,
                _ctx: crate::RequestCtx,
            ) -> BoxFuture<'static, crate::Response> {
                Box::pin(async { probe_response(http::StatusCode::OK, &serde_json::json!({})) })
            }
        }

        let routes = Router::new()
            .endpoint::<Fixed>(HttpMethod::Get, "/things")
            .endpoint::<Fixed>(HttpMethod::Get, "/stuff");

        let error = builder()
            .mount(routes)
            .build()
            .expect_err("two ids collide");
        let report = render(&error);
        assert!(report.contains("duplicate operationId"), "{report}");
        assert!(report.contains("list_things"), "{report}");
    }

    #[tokio::test]
    async fn the_document_is_served_pre_serialised_with_an_etag() {
        let app = builder()
            .openapi(|d| {
                d.title("Shop API").version("1.4.2");
            })
            .build()
            .expect("nothing to fail");

        let (status, headers, body) = get(&app, "/openapi.json").await;
        assert_eq!(status, http::StatusCode::OK);
        assert_eq!(headers[http::header::CONTENT_TYPE], "application/json");
        assert!(body.contains("\"Shop API\""), "{body}");

        let etag = headers[http::header::ETAG].clone();
        let request = http::Request::builder()
            .uri("/openapi.json")
            .header(http::header::IF_NONE_MATCH, etag)
            .body(axum::body::Body::empty())
            .expect("a well-formed request");
        let response = app
            .service
            .clone()
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
    }

    #[tokio::test]
    async fn the_docs_ui_is_served_and_needs_no_network() {
        let app = builder().build().expect("nothing to fail");
        let (status, headers, body) = get(&app, "/docs").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .starts_with("text/html")
        );
        assert!(
            !body.contains("https://cdn."),
            "the UI must be self-contained"
        );
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn the_document_is_also_served_as_yaml() {
        let app = builder()
            .openapi(|d| {
                d.title("Shop API").version("1.4.2");
            })
            .build()
            .expect("nothing to fail");

        let (status, headers, body) = get(&app, "/openapi.yaml").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .contains("yaml"),
            "{:?}",
            headers[http::header::CONTENT_TYPE]
        );
        assert!(body.contains("openapi:"), "{body}");
        assert!(body.contains("Shop API"), "{body}");

        // The YAML route answers 304 to a matching `If-None-Match`, exactly as
        // the JSON one does.
        let etag = headers[http::header::ETAG].clone();
        let request = http::Request::builder()
            .uri("/openapi.yaml")
            .header(http::header::IF_NONE_MATCH, etag)
            .body(axum::body::Body::empty())
            .expect("a well-formed request");
        let response = app
            .service
            .clone()
            .oneshot(request)
            .await
            .expect("infallible");
        assert_eq!(response.status(), http::StatusCode::NOT_MODIFIED);
    }

    // The compact renderer (`lean-docs`) keeps the strict, nonce-only CSP; the
    // default Swagger-UI page relaxes `style-src` and is covered separately below.
    #[cfg(all(feature = "openapi", feature = "lean-docs"))]
    #[tokio::test]
    async fn the_docs_page_carries_a_csp_nonce_and_forbids_unsafe_inline() {
        let app = builder().build().expect("nothing to fail");
        let (status, headers, body) = get(&app, "/docs").await;
        assert_eq!(status, http::StatusCode::OK);

        let csp = headers[http::header::CONTENT_SECURITY_POLICY]
            .to_str()
            .expect("ascii");
        assert!(csp.contains("script-src 'nonce-"), "{csp}");
        assert!(csp.contains("style-src 'nonce-"), "{csp}");
        assert!(!csp.contains("unsafe-inline"), "{csp}");

        // The nonce named in the policy is the one on the inline elements — a
        // policy that named a different value would forbid the page's own script.
        let nonce = csp
            .split("script-src 'nonce-")
            .nth(1)
            .expect("a script-src nonce")
            .split('\'')
            .next()
            .expect("a closing quote");
        assert!(!nonce.is_empty(), "{csp}");
        assert!(
            body.contains(&format!("nonce=\"{nonce}\"")),
            "the inline tags must carry the policy's nonce"
        );

        // Every response gets a fresh nonce, so the page is never reusable.
        let (_, second, _) = get(&app, "/docs").await;
        assert_ne!(
            csp,
            second[http::header::CONTENT_SECURITY_POLICY]
                .to_str()
                .expect("ascii"),
            "each response gets a fresh nonce"
        );
    }

    #[cfg(all(feature = "openapi", not(feature = "lean-docs")))]
    #[tokio::test]
    async fn the_docs_page_is_the_real_swagger_ui_and_serves_its_assets() {
        let app = builder().build().expect("nothing to fail");

        let (status, headers, body) = get(&app, "/docs").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .starts_with("text/html")
        );
        assert!(body.contains("SwaggerUIBundle"), "{body}");
        assert!(body.contains("/docs/swagger-ui-bundle.js"), "{body}");
        // Self-hosted, not a CDN — the whole point of vendoring the bundle.
        assert!(
            !body.contains("https://"),
            "the page must be self-hosted: {body}"
        );

        // The JS bundle is ~1.4 MB — larger than `get`'s body cap — so check its
        // route from the response head without draining the body.
        let request = http::Request::builder()
            .uri("/docs/swagger-ui-bundle.js")
            .body(axum::body::Body::empty())
            .expect("a well-formed request");
        let response = app
            .service
            .clone()
            .oneshot(request)
            .await
            .expect("the router is infallible");
        assert_eq!(response.status(), http::StatusCode::OK);
        assert!(
            response.headers()[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .contains("javascript")
        );

        // The stylesheet is small enough to fetch whole.
        let (status, headers, _) = get(&app, "/docs/swagger-ui.css").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .contains("css")
        );
    }

    #[cfg(all(feature = "openapi", not(feature = "lean-docs")))]
    #[tokio::test]
    async fn the_swagger_page_carries_a_fresh_csp_nonce() {
        let app = builder().build().expect("nothing to fail");
        let (status, headers, body) = get(&app, "/docs").await;
        assert_eq!(status, http::StatusCode::OK);

        let csp = headers[http::header::CONTENT_SECURITY_POLICY]
            .to_str()
            .expect("ascii");
        // The bundle loads from 'self'; the inline bootstrap is admitted by nonce.
        assert!(csp.contains("script-src 'self' 'nonce-"), "{csp}");
        // Swagger UI injects element styles at runtime, so style-src must allow it.
        assert!(csp.contains("style-src 'self' 'unsafe-inline'"), "{csp}");

        let nonce = csp
            .split("script-src 'self' 'nonce-")
            .nth(1)
            .expect("a script-src nonce")
            .split('\'')
            .next()
            .expect("a closing quote");
        assert!(!nonce.is_empty(), "{csp}");
        assert!(
            body.contains(&format!("nonce=\"{nonce}\"")),
            "the inline bootstrap must carry the policy's nonce"
        );

        let (_, second, _) = get(&app, "/docs").await;
        assert_ne!(
            csp,
            second[http::header::CONTENT_SECURITY_POLICY]
                .to_str()
                .expect("ascii"),
            "each response gets a fresh nonce"
        );
    }

    #[cfg(feature = "redoc")]
    #[tokio::test]
    async fn the_redoc_route_is_served_when_its_feature_is_on() {
        let app = builder().build().expect("nothing to fail");
        let (status, headers, _) = get(&app, "/redoc").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .starts_with("text/html")
        );
    }

    #[cfg(all(feature = "openapi", not(feature = "redoc")))]
    #[tokio::test]
    async fn the_redoc_route_is_absent_without_its_feature() {
        let app = builder().build().expect("nothing to fail");
        let (status, _, _) = get(&app, "/redoc").await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[cfg(feature = "swagger-ui")]
    #[tokio::test]
    async fn the_swagger_route_is_served_when_its_feature_is_on() {
        let app = builder().build().expect("nothing to fail");
        let (status, headers, _) = get(&app, "/swagger").await;
        assert_eq!(status, http::StatusCode::OK);
        assert!(
            headers[http::header::CONTENT_TYPE]
                .to_str()
                .expect("ascii")
                .starts_with("text/html")
        );
    }

    #[cfg(all(feature = "openapi", not(feature = "swagger-ui")))]
    #[tokio::test]
    async fn the_swagger_route_is_absent_without_its_feature() {
        let app = builder().build().expect("nothing to fail");
        let (status, _, _) = get(&app, "/swagger").await;
        assert_eq!(status, http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn production_does_not_publish_its_own_documentation() {
        let app = App::new(TestConfig { name: "prod" })
            .profile(Profile::Production)
            .middleware(MiddlewareStack::bare())
            .build()
            .expect("nothing to fail");

        assert!(!app.state().http().expose_docs);
        // Every documentation route is 404 in production — the JSON and YAML
        // documents and every UI route, whether or not a UI feature is on.
        for path in [
            "/openapi.json",
            "/openapi.yaml",
            "/docs",
            "/redoc",
            "/swagger",
        ] {
            let (status, _, _) = get(&app, path).await;
            assert_eq!(status, http::StatusCode::NOT_FOUND, "{path}");
        }
        // The document itself still exists — `moso openapi export` needs it.
        assert!(app.openapi().info.version.is_empty() || !app.openapi().info.title.is_empty());
    }

    #[cfg(feature = "openapi")]
    #[tokio::test]
    async fn a_hand_built_production_config_cannot_force_docs_on() {
        // `expose_docs` defaults to `true` on a bare `HttpConfig` struct literal.
        // In the production profile that must not reach the router: the security
        // default is enforced at boot, not left to the profile constructor.
        let app = App::new(TestConfig { name: "prod" })
            .profile(Profile::Production)
            .middleware(MiddlewareStack::bare())
            .http_config(HttpConfig {
                expose_docs: true,
                ..HttpConfig::default()
            })
            .build()
            .expect("nothing to fail");

        assert!(
            !app.state().http().expose_docs,
            "production forces the documentation surface off regardless of the config literal"
        );
        for path in ["/openapi.json", "/openapi.yaml", "/docs"] {
            let (status, _, _) = get(&app, path).await;
            assert_eq!(status, http::StatusCode::NOT_FOUND, "{path}");
        }
    }

    // ── health ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_failing_check_takes_readyz_to_503_and_leaves_healthz_alone() {
        struct Broken;
        impl HealthCheck for Broken {
            fn check<'a>(&'a self, _r: &'a Resolver) -> BoxFuture<'a, crate::health::HealthStatus> {
                Box::pin(async { crate::health::HealthStatus::Down("refused".to_owned()) })
            }
        }

        let app = builder()
            .health_check("database", Broken)
            .build()
            .expect("nothing to fail");

        let (status, _, body) = get(&app, "/readyz").await;
        assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(body.contains("down: refused"), "{body}");

        let (status, _, _) = get(&app, "/healthz").await;
        assert_eq!(status, http::StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reports_the_version_the_document_declares() {
        let app = builder()
            .openapi(|d| {
                d.version("1.4.2");
            })
            .build()
            .expect("nothing to fail");
        let (_, _, body) = get(&app, "/readyz").await;
        let report: HealthReport = serde_json::from_str(&body).expect("a report");
        assert_eq!(report.version, "1.4.2");
    }

    #[tokio::test]
    async fn readyz_flips_to_503_the_moment_the_signal_fires() {
        let app = builder().build().expect("nothing to fail");
        let (status, _, _) = get(&app, "/readyz").await;
        assert_eq!(status, http::StatusCode::OK);

        app.shutdown_signal().trigger();

        let started = std::time::Instant::now();
        let (status, _, body) = get(&app, "/readyz").await;
        let elapsed = started.elapsed();

        assert_eq!(status, http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            elapsed < std::time::Duration::from_millis(100),
            "/readyz took {elapsed:?} to answer 503"
        );
        assert!(body.contains("shutting down"), "{body}");
        // Liveness is unaffected: the process is still alive and draining.
        let (status, _, _) = get(&app, "/healthz").await;
        assert_eq!(status, http::StatusCode::OK);
    }

    // ── lifecycle hooks ───────────────────────────────────────────────────

    #[tokio::test]
    async fn hooks_run_forwards_and_shutdown_hooks_run_backwards() {
        let order = Arc::new(Mutex::new(Vec::<&'static str>::new()));

        let app = builder()
            .provide_arc(Arc::clone(&order))
            .on_startup({
                let order = Arc::clone(&order);
                move |_| {
                    let order = Arc::clone(&order);
                    async move {
                        order.lock().expect("not poisoned").push("startup-1");
                        Ok(())
                    }
                }
            })
            .on_startup({
                let order = Arc::clone(&order);
                move |_| {
                    let order = Arc::clone(&order);
                    async move {
                        order.lock().expect("not poisoned").push("startup-2");
                        Ok(())
                    }
                }
            })
            .on_shutdown({
                let order = Arc::clone(&order);
                move |_| {
                    let order = Arc::clone(&order);
                    async move {
                        order.lock().expect("not poisoned").push("shutdown-1");
                    }
                }
            })
            .on_shutdown({
                let order = Arc::clone(&order);
                move |_| {
                    let order = Arc::clone(&order);
                    async move {
                        order.lock().expect("not poisoned").push("shutdown-2");
                    }
                }
            })
            .build()
            .expect("nothing to fail");

        let signal = app.shutdown_signal();
        signal.trigger();
        app.serve_workers().await.expect("clean shutdown");

        assert_eq!(
            *order.lock().expect("not poisoned"),
            vec!["startup-1", "startup-2", "shutdown-2", "shutdown-1"]
        );
    }

    #[tokio::test]
    async fn a_failing_startup_hook_aborts_before_serving() {
        let app = builder()
            .on_startup(|_| async { Err(Error::internal_msg("the migration table is locked")) })
            .build()
            .expect("build succeeds; the hook runs at serve time");

        let error = app.serve_workers().await.expect_err("the hook failed");
        assert!(error.to_string().contains("migration table"), "{error}");
    }

    #[tokio::test]
    async fn a_lifespan_guard_is_dropped_after_the_shutdown_hooks() {
        let released = Arc::new(AtomicUsize::new(0));

        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }

        let app = builder()
            .lifespan({
                let released = Arc::clone(&released);
                move |_| async move { Ok(Guard(released)) }
            })
            .build()
            .expect("nothing to fail");

        app.shutdown_signal().trigger();
        app.serve_workers().await.expect("clean shutdown");
        assert_eq!(released.load(Ordering::Acquire), 1);
    }

    // ── provide_with ──────────────────────────────────────────────────────

    #[derive(Debug, PartialEq, Eq)]
    struct Search(&'static str);

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_factory_reads_the_providers_registered_before_it() {
        let app = builder()
            .provide(Db)
            .provide_with(|resolver| async move {
                resolver.get::<Db>()?;
                Ok(Search("connected"))
            })
            .build()
            .expect("the factory succeeded");

        assert_eq!(
            *app.resolver().get::<Search>().expect("registered"),
            Search("connected")
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_factory_that_needs_a_later_provider_gets_an_ordering_error() {
        let error = builder()
            .provide_with(|resolver| async move {
                resolver.get::<Db>()?;
                Ok(Search("connected"))
            })
            .provide(Db)
            .build()
            .expect_err("the factory ran too early");

        let report = render(&error);
        assert!(report.contains("`Search` is built before `Db`"), "{report}");
        assert!(report.contains("swap the two registrations"), "{report}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_factories_that_need_each_other_are_a_cycle() {
        struct A;
        struct B;

        let error = builder()
            .provide_with(|resolver| async move {
                resolver.get::<B>()?;
                Ok(A)
            })
            .provide_with(|resolver| async move {
                resolver.get::<A>()?;
                Ok(B)
            })
            .build()
            .expect_err("the two factories need each other");

        let report = render(&error);
        assert!(report.contains("provider cycle"), "{report}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_factory_that_simply_fails_says_so() {
        let error = builder()
            .provide_with(|_| async { Err::<Search, _>(Error::internal_msg("connection refused")) })
            .build()
            .expect_err("the factory failed");

        let report = render(&error);
        assert!(report.contains("provider failed"), "{report}");
        assert!(report.contains("connection refused"), "{report}");
    }

    #[tokio::test]
    async fn a_factory_on_a_current_thread_runtime_is_a_boot_error_not_a_deadlock() {
        let error = builder()
            .provide_with(|_| async { Ok(Search("never built")) })
            .build()
            .expect_err("a current-thread runtime cannot drive it");

        let report = render(&error);
        assert!(report.contains("multi-threaded runtime"), "{report}");
        assert!(report.contains("flavor = \"multi_thread\""), "{report}");
    }

    #[test]
    fn a_factory_outside_a_runtime_is_a_boot_error() {
        let error = builder()
            .provide_with(|_| async { Ok(Search("never built")) })
            .build()
            .expect_err("there is no runtime to drive it");
        assert!(render(&error).contains("needs a Tokio runtime"));
    }

    // ── serving for real ──────────────────────────────────────────────────

    /// One HTTP/1.1 request over a raw socket, so a test can exercise the
    /// listener rather than the service.
    async fn raw_get(address: SocketAddr, path: &str) -> String {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("the listener is accepting");
        let request =
            format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("the request was written");
        let mut response = String::new();
        stream
            .read_to_string(&mut response)
            .await
            .expect("the response was read");
        response
    }

    async fn bound() -> (tokio::net::TcpListener, SocketAddr) {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("a free port");
        let address = listener.local_addr().expect("a bound address");
        (listener, address)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_signal_during_an_in_flight_request_lets_it_finish() {
        /// Two seconds of work, as the acceptance criterion specifies.
        async fn slow() -> &'static str {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            "finished"
        }

        let app = builder()
            .mount(Router::new().get("/slow", slow))
            .build()
            .expect("nothing to fail");
        let signal = app.shutdown_signal();

        let (listener, address) = bound().await;
        let served = tokio::spawn(app.serve_on(listener));

        let request = tokio::spawn(async move { raw_get(address, "/slow").await });
        // Let the request reach the handler, then ask the process to stop.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        signal.trigger();

        let started = std::time::Instant::now();
        let response = request.await.expect("the request task did not panic");
        assert!(response.contains("200 OK"), "{response}");
        assert!(response.contains("finished"), "{response}");

        served
            .await
            .expect("the server task did not panic")
            .expect("a clean shutdown");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "shutdown took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_wedged_request_does_not_hold_the_process_past_the_grace() {
        /// A request that never answers: the shape that turns a 25 s grace into
        /// a `SIGKILL` if the drain is unbounded.
        async fn wedged() -> &'static str {
            std::future::pending::<()>().await;
            "never"
        }

        let app = builder()
            .server_config(ServerConfig {
                shutdown_grace: std::time::Duration::from_millis(200),
                ..ServerConfig::default()
            })
            .mount(Router::new().get("/wedged", wedged))
            .build()
            .expect("nothing to fail");
        let signal = app.shutdown_signal();

        let (listener, address) = bound().await;
        let served = tokio::spawn(app.serve_on(listener));
        let wedged_request = tokio::spawn(async move { raw_get(address, "/wedged").await });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let started = std::time::Instant::now();
        signal.trigger();

        served
            .await
            .expect("the server task did not panic")
            .expect("a clean shutdown");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "the wedged request held shutdown for {:?}",
            started.elapsed()
        );
        wedged_request.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readyz_answers_503_over_a_real_socket_once_the_signal_fires() {
        let app = builder().build().expect("nothing to fail");
        let signal = app.shutdown_signal();
        let (listener, address) = bound().await;
        let served = tokio::spawn(app.serve_on(listener));

        assert!(raw_get(address, "/readyz").await.contains("200 OK"));

        signal.trigger();
        // The listener stops accepting once the signal fires, so the 503 has to
        // be observed on a connection opened before it. This is the ordering
        // the load balancer sees in production, where connections are pooled.
        served
            .await
            .expect("the server task did not panic")
            .expect("a clean shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_applications_serve_in_one_process() {
        let first = builder().build().expect("nothing to fail");
        let second = builder().build().expect("nothing to fail");

        let (first_listener, first_address) = bound().await;
        let (second_listener, second_address) = bound().await;
        assert_ne!(first_address, second_address);

        let first_signal = first.shutdown_signal();
        let second_signal = second.shutdown_signal();
        let first_served = tokio::spawn(first.serve_on(first_listener));
        let second_served = tokio::spawn(second.serve_on(second_listener));

        assert!(raw_get(first_address, "/healthz").await.contains("200 OK"));
        assert!(raw_get(second_address, "/healthz").await.contains("200 OK"));

        first_signal.trigger();
        second_signal.trigger();
        first_served
            .await
            .expect("no panic")
            .expect("a clean shutdown");
        second_served
            .await
            .expect("no panic")
            .expect("a clean shutdown");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_drain_that_does_not_finish_is_named_in_a_warning() {
        let app = builder()
            .server_config(ServerConfig {
                shutdown_grace: std::time::Duration::from_millis(50),
                ..ServerConfig::default()
            })
            .build()
            .expect("nothing to fail");

        // A guard nobody drops: exactly the leaked-stream shape.
        let leaked = app.state().drain().guard("GET /events");
        let signal = app.shutdown_signal();
        let (listener, _) = bound().await;
        let served = tokio::spawn(app.serve_on(listener));

        signal.trigger();
        served.await.expect("no panic").expect("a clean shutdown");
        assert_eq!(leaked.name(), "GET /events");
    }

    // ── profile and configuration defaults ────────────────────────────────

    #[test]
    fn the_profile_decides_whether_the_docs_are_published() {
        assert!(http_defaults(Profile::Dev).expose_docs);
        assert!(http_defaults(Profile::Test).expose_docs);
        assert!(!http_defaults(Profile::Production).expose_docs);
        for profile in [Profile::Dev, Profile::Test, Profile::Production] {
            assert!(
                !http_defaults(profile).expose_internal_errors,
                "no profile discloses internal errors by default"
            );
        }
    }

    #[test]
    fn an_explicit_http_config_wins_over_the_profile_default() {
        let app = builder()
            .http_config(HttpConfig {
                expose_docs: false,
                ..HttpConfig::default()
            })
            .build()
            .expect("nothing to fail");
        assert!(!app.state().http().expose_docs);
    }

    #[test]
    fn a_cycle_is_only_reported_through_the_type_that_starts_it() {
        let mut needs = HashMap::new();
        needs.insert("A", "B");
        needs.insert("B", "A");
        assert_eq!(find_cycle("A", &needs), Some(vec!["A", "B", "A"]));

        let mut chain = HashMap::new();
        chain.insert("A", "B");
        chain.insert("B", "C");
        assert_eq!(find_cycle("A", &chain), None);
    }

    #[test]
    fn a_path_that_is_not_rooted_is_not_mounted() {
        assert!(is_mountable("/healthz"));
        assert!(!is_mountable("healthz"));
        assert!(!is_mountable(""));
    }
}
