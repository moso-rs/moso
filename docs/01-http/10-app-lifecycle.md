# 10 — `App`, Lifecycle & Boot-Time Validation

> ✅ **Status: implemented.** `App`, `AppBuilder`, `AppState`, `Resolver`, `Lifespan`, the boot
> sequence, the multi-problem boot report with fixes, `/healthz` + `/readyz`, graceful drain within
> the configured grace, `serve_workers`, and two apps in one process. Anything below that names a
> battery (`Db`, `Kv`, a mailer, a job worker) is illustrative — those crates do not exist.

## Why `App` exists

Axum gives you a `Router` and `axum::serve`. Everything between — configuration, connection pools,
graceful shutdown, background workers, health checks, "did I wire up the thing this handler needs" —
is the user's problem. `App` is where the framework earns its name.

The design goal: **anything that can fail should fail at boot with a sentence, not at 3am with a
`panic` in a request.**

## Public API

```rust
// spec — moso-core/src/app.rs

pub struct App { /* opaque */ }

pub struct AppBuilder { /* opaque */ }

impl App {
    /// Start a builder. `config` is any type deriving `Config`; it is stored as a
    /// provider so handlers can `Inject<Config<AppConfig>>`.
    pub fn new<C: AppConfigTrait>(config: C) -> AppBuilder;

    /// Serve using the address/TLS/limits from config. Installs signal handlers.
    pub async fn serve(self) -> Result<()>;

    /// Serve on a caller-provided listener (tests, socket activation, custom TLS).
    pub async fn serve_on(self, listener: TcpListener) -> Result<()>;

    /// Run the app's workers only (no HTTP). Used by `moso worker` / a worker container.
    pub async fn serve_workers(self) -> Result<()>;

    /// Consume the app, returning the composed tower service. Full escape hatch.
    pub fn into_service(self) -> axum::Router<()>;

    /// The generated OpenAPI document.
    #[cfg(feature = "openapi")]
    pub fn openapi(&self) -> &openapi::Document;
}

impl AppBuilder {
    // ── dependency providers ───────────────────────────────────────────────
    /// Register an app-lifetime value retrievable via `Inject<T>`.
    pub fn provide<T: Send + Sync + 'static>(self, value: T) -> Self;

    /// Register a value constructed at boot, possibly fallibly and asynchronously.
    pub fn provide_with<T, F, Fut>(self, f: F) -> Self
    where F: FnOnce(&Resolver) -> Fut + Send + 'static,
          Fut: Future<Output = Result<T>> + Send,
          T: Send + Sync + 'static;

    /// Register a trait object provider: `provide_dyn::<dyn Mailer>(SmtpMailer::new(..))`.
    pub fn provide_dyn<T: ?Sized + Send + Sync + 'static>(self, value: Arc<T>) -> Self;

    // ── routing ────────────────────────────────────────────────────────────
    pub fn mount(self, router: Router) -> Self;
    pub fn mount_at(self, prefix: &str, router: Router) -> Self;
    pub fn mount_axum(self, prefix: &str, router: axum::Router<()>) -> Self;

    // ── middleware ─────────────────────────────────────────────────────────
    /// Replace the default middleware stack wholesale.
    pub fn middleware(self, stack: MiddlewareStack) -> Self;
    /// Adjust the default stack (add/remove/reorder) — see 17-middleware.md.
    pub fn with_middleware(self, f: impl FnOnce(&mut MiddlewareStack)) -> Self;

    // ── lifecycle ──────────────────────────────────────────────────────────
    /// Run before the listener binds. Failure aborts boot.
    pub fn on_startup<F, Fut>(self, f: F) -> Self
    where F: FnOnce(Resolver) -> Fut + Send + 'static, Fut: Future<Output = Result<()>> + Send;

    /// Run after the listener closes and in-flight requests drain.
    pub fn on_shutdown<F, Fut>(self, f: F) -> Self
    where F: FnOnce(Resolver) -> Fut + Send + 'static, Fut: Future<Output = ()> + Send;

    /// RAII form: acquire in the async block, release when the returned guard drops.
    pub fn lifespan<F, Fut, G>(self, f: F) -> Self
    where F: FnOnce(Resolver) -> Fut + Send + 'static,
          Fut: Future<Output = Result<G>> + Send, G: Send + 'static;

    // ── health ─────────────────────────────────────────────────────────────
    /// Named readiness probe. All must pass for /readyz to return 200.
    pub fn health_check(self, name: &'static str, check: impl HealthCheck) -> Self;

    // ── batteries ──────────────────────────────────────────────────────────
    #[cfg(feature = "jobs")]  pub fn mount_jobs(self, registry: JobRegistry) -> Self;
    #[cfg(feature = "admin")] pub fn with_admin(self, admin: AdminBuilder) -> Self;
    #[cfg(feature = "auth")]  pub fn with_auth<B: AuthBackend>(self, backend: B) -> Self;

    /// Validate and finalise. This is where boot-time errors are produced.
    pub fn build(self) -> Result<App>;
}
```

## The boot sequence (normative order)

`AppBuilder::build()` then `App::serve()` execute exactly these steps:

```
build():
  1. Resolve config          → typed, all sources layered, secrets redacted in Debug
  2. Freeze provider map     → run `provide_with` factories in dependency order
  3. Compose router          → merge mounted routers; detect route conflicts
  4. Validate DI graph       → every OperationSpec's required providers exist   ★
  5. Validate authz          → every `#[requires(..)]` permission is registered ★
  6. Build OpenAPI document  → also detects duplicate operation ids / schema name clashes ★
  7. Validate jobs           → every enqueue site's job type is registered      ★
  8. Compose middleware      → assert stack ordering invariants                 ★
  → App

serve():
  9.  Run `on_startup` hooks in registration order (fail ⇒ abort, non-zero exit)
  10. Run migrations if `config.database.auto_migrate` (default: false in prod profile)
  11. Bind listener; log the bound address, the docs URL, and the profile
  12. Start job workers if `config.jobs.run_in_web_process` (default: true in dev, false in prod)
  13. Install SIGINT/SIGTERM handler
  14. Serve
  ── on signal ──
  15. Stop accepting; drain in-flight up to `shutdown_grace` (default 25s)
  16. Stop job workers, allowing current jobs to finish or re-queue
  17. Run `on_shutdown` hooks in reverse order; drop lifespan guards
  18. Flush tracing/OTel exporters, then exit 0
```

★ = the checks that make Moso's boot different from an Axum app's. Each has a hand-written error.

## Boot error output (this is a product surface)

`build()` returns `Err(Error::Boot(BootErrors))`, which renders as a grouped report — **all**
problems at once, not the first one:

```
error: application failed to build (3 problems)

  ✗ missing provider: `shop::db::Db`
      required by  GET /users            src/routes/users.rs:14
                   POST /users           src/routes/users.rs:31
                   GET /users/{id}       src/routes/users.rs:47
      fix          add `.provide(db)` to your `App` builder in src/lib.rs
                   let db = moso::db::connect(&cfg.database).await?;
                   App::new(cfg).provide(db)

  ✗ route conflict: GET /users/{id}  and  GET /users/{user_id}
      first        src/routes/users.rs:47
      second       src/routes/admin.rs:22
      note         path parameters must have the same name at the same position
      fix          rename one parameter, or nest one router under a distinct prefix

  ✗ unknown permission: "posts.publsh"
      used by      POST /posts/{id}/publish   src/routes/posts.rs:88
      did you mean "posts.publish"?
      note         permissions are declared in the `permissions! { .. }` block in src/authz.rs
```

**Requirements**
- MUST report every problem in one pass (no fail-fast).
- MUST include a source location. Location comes from `#[endpoint]` capturing
  `file!()`/`line!()` into the `OperationSpec`.
- MUST include a concrete `fix` line with code where the fix is mechanical.
- MUST use Levenshtein suggestion for any name-like mismatch (permission, provider type name,
  job name, config key).
- Colour and Unicode box-drawing are used when stdout is a TTY, plain ASCII otherwise.

## `Resolver` — reading providers outside a request

```rust
// spec
pub struct Resolver { /* opaque, cheap clone */ }

impl Resolver {
    pub fn get<T: Send + Sync + 'static>(&self) -> Result<&T>;
    pub fn get_dyn<T: ?Sized + Send + Sync + 'static>(&self) -> Result<Arc<T>>;
    pub fn config<C: AppConfigTrait>(&self) -> &C;
}
```

Used by `on_startup`, `provide_with`, jobs, and CLI tasks. Inside a request you use `Inject<T>`
instead, which is infallible *because* boot validated it — this asymmetry is the whole point.

## Health checks

```rust
// spec
#[async_trait]
pub trait HealthCheck: Send + Sync + 'static {
    async fn check(&self, r: &Resolver) -> HealthStatus;
    /// If false, failure degrades /readyz but never /healthz. Default true.
    fn critical(&self) -> bool { true }
}

pub enum HealthStatus { Up, Degraded(String), Down(String) }
```

Two endpoints, mounted automatically, excluded from OpenAPI and from access logs:

- `GET /healthz` — liveness. Returns 200 as long as the process can serve. Never touches the DB.
- `GET /readyz` — readiness. Runs all registered checks concurrently with a 2 s budget. Body:
  ```json
  { "status": "degraded", "checks": { "database": "up", "redis": "down: connection refused" },
    "version": "1.4.2", "commit": "a1b2c3d", "uptime_s": 43120 }
  ```

`moso::db` and `moso::kv` register their checks automatically when provided. Both paths are
configurable (some platforms reserve them).

## Graceful shutdown

```rust
// spec
pub mod shutdown {
    /// A clonable handle that resolves when shutdown begins.
    pub struct Signal { /* opaque */ }
    impl Signal {
        pub async fn recv(&self);
        pub fn is_shutting_down(&self) -> bool;
    }
    /// Injectable: `Inject<Signal>` — long-lived handlers (SSE, WS) should select on it.
    }
```

Rules:
- The default 25 s grace is under the common 30 s orchestrator kill timeout, deliberately.
- WebSocket and SSE handlers get `Inject<Signal>` and are expected to close cleanly; the framework
  logs a warning naming any route still open at the end of grace, which is how you find the leak.
- Job workers stop pulling new work immediately, then finish or re-queue in-flight jobs.
- `/readyz` starts returning 503 **immediately** on signal, before draining begins, so load
  balancers remove the instance while it is still serving.

## Multiple processes from one binary

The generated binary supports process roles, so the same image runs as web, worker, or scheduler:

```
shop                 # web (default)
shop worker --queues=default,mail --concurrency=8
shop scheduler
shop task seed
shop migrate
```

Implemented by `moso::runtime::main!` in the generated `main.rs` when the `jobs` feature is on.
This avoids the "separate worker binary that drifts from the web binary" failure mode.

## Testing hook

```rust
// spec — moso-test
impl App {
    /// Bind to port 0, return the app plus a client pointed at it. Test-only.
    #[cfg(feature = "test")]
    pub async fn spawn_test(self) -> Result<TestApp>;
}
```

Because `lib.rs::app()` is the real composition root, tests exercise the real DI graph, the real
middleware stack, and the real OpenAPI document. See `04-devex/43-testing.md`.

## Acceptance criteria (WP-02)

1. `App::new(cfg).build()` with no routes succeeds and serves `/healthz`, `/readyz`.
2. A handler using `Inject<T>` with no `provide::<T>` fails `build()` with the boot report above,
   listing every offending route with file:line. Covered by a UI test.
3. Three simultaneous problems produce three entries in one report.
4. SIGTERM during a 10 s in-flight request: request completes, process exits 0 within 11 s,
   `/readyz` returned 503 within 100 ms of the signal.
5. `App` is `Send + 'static`; two `App`s can be built and served in one process (test isolation).
6. `into_service()` output serves the same routes under plain `axum::serve`.
