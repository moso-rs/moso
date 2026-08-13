---
title: Health and shutdown
description: What App::build validates before it returns, how the liveness and readiness probes answer, and how a Moso process drains in-flight work before it exits.
order: 12
status: shipped
---

`App` is the part of Moso that Axum has no counterpart for. It takes your configuration, providers,
routers and middleware, validates the whole assembly in one pass, and then owns the process: binding
the listener, running startup hooks, answering the orchestrator's probes, and draining on `SIGTERM`
under a deadline you control.

The rule the whole module is built around: anything that can fail should fail at boot with a
sentence, not at 3am with a panic inside a request. That is why `build()` returns every problem it
found rather than the first one, why a missing provider is a boot error rather than a 500, and why
`/readyz` answers 503 within milliseconds of a signal rather than after the drain.

## The smallest thing that works

An application with no routes at all still serves both probes.

```rust title="src/lib.rs"
use moso::prelude::*;

/// Everything this application reads from its environment.
#[derive(Config, Clone, Debug)]
pub struct AppConfig {
    /// Service name.
    #[config(default = "shop")]
    pub name: String,
}

pub fn app() -> Result<App> {
    App::new(AppConfig::load()?).build()
}
```

```rust title="src/main.rs"
#[tokio::main]
async fn main() -> moso::Result<()> {
    shop::app()?.serve().await
}
```

```bash
curl -s localhost:3000/healthz
# {"status":"up"}

curl -s localhost:3000/readyz
# {"status":"up","checks":{},"version":"0.1.0","uptime_s":0}
```

Nothing was registered to make that happen. `/healthz`, `/readyz`, and (when the `openapi` feature
is on and `http.expose_docs` is true) `/openapi.json` and `/docs` are mounted on an outer router
whose fallback is your application:

```text
outer router
  ├── GET  /healthz        ─┐  outside the stack: no access log, no compression,
  ├── GET  /readyz          │  no request-id span, no timeout
  ├── GET  /openapi.json    │
  ├── GET  /docs           ─┘
  └── fallback → middleware stack → application router → route → handler
```

A liveness probe running twice a second would otherwise be the majority of your log volume, and
`/openapi.json` is a byte slice serialised once at boot that should cost a memcpy. The price of that
exclusion is that an application route at one of those paths would be silently dead, so registering
one is a boot error that names the config key to move.

## What `build()` does

`AppBuilder::build()` runs seven steps, collecting problems rather than returning on the first.

1. **Configuration.** Whatever `.http_config(..)` set, otherwise defaults derived from the profile.
   If `expose_internal_errors` is on, a `WARN` is emitted at `target: "moso::app"`.
2. **Providers.** A fresh `Signal`, a fresh `Drain` and a clone of the global `BlockingPool` go in
   first, then your registrations in order. Before each `provide_with` factory runs, the map is
   rebuilt from the registrations accumulated so far, so a factory sees exactly what was registered
   before it and no more.
3. **Router.** Route conflicts are drained into the report, every path is compared against
   `health_path`, `ready_path`, `docs_path` and `openapi_path`, and the route table is captured.
4. **The dependency graph.** Every route's declared `ProviderReq` is checked against the frozen
   provider map. Misses are grouped by provider, so one hole is one problem with nine `required by`
   lines rather than nine problems that all say the same thing.
5. **The OpenAPI document.** Each route is described exactly once, then `document.build()` surfaces
   duplicate `operationId`s, schema name collisions and path-parameter mismatches.
6. **Middleware.** `MiddlewareStack::configure(profile, &http)` applies derived defaults, skipping
   anything you set explicitly, then `validate()` reports ordering violations.
7. **State and service.** `AppState` is frozen, the clock is started, and the service is composed.

If nothing was collected you hold an `App` whose only remaining failure modes are "the port is
taken" and "a startup hook said no".

Step 2 registers four things before any of yours, so a later `.provide(..)` of the same type wins:

| Type | Reachable as | What it is |
| --- | --- | --- |
| `Signal` | `Inject<Signal>` | The process shutdown signal |
| `Drain` | `Inject<Drain>` | The handle that hands out `ShutdownGuard`s |
| `BlockingPool` | `Inject<BlockingPool>` | The pool for `spawn_blocking` work |
| your `C: Config` | `Inject<AppConfig>` | Whatever you passed to `App::new` |

A `Vec<Arc<dyn SecretProvider>>` joins them when you registered at least one `.secret_provider(..)`.

### The boot report

Every problem, one pass, with a source location that `#[endpoint]` captured from `file!()` and
`line!()`, and a `fix` line you can paste:

```text
error: application failed to build (2 problems)

  x missing provider: `shop::db::Db`
      required by  GET /users       src/routes/users.rs:14
                   POST /users      src/routes/users.rs:31
                   GET /users/{id}  src/routes/users.rs:47
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Db = /* construct it */;
                   App::new(config).provide(value)

  x route conflict: GET /users/{id}  and  GET /users/{user_id}
      first        src/routes/users.rs:47
      second       src/routes/admin.rs:22
      note         path parameters must have the same name at the same position
      fix          rename one parameter, or nest one router under a distinct prefix
```

Colour and box drawing appear on a TTY and plain ASCII everywhere else. `NO_COLOR` and
`MOSO_NO_COLOR` force plain output. Problems are sorted before rendering: missing providers first
(usually the root cause), then route conflicts, then configuration, then everything else.

What boot detects:

| Problem | Typical cause |
| --- | --- |
| missing provider | an `Inject<T>` with no `.provide(..)` |
| route conflict | two registrations on the same method and path, or parameter names that disagree |
| route shadowed by the framework | an application route at `/healthz`, `/readyz`, `/docs` or `/openapi.json` |
| path parameter mismatch | `/users/{id}` where the documented handler reads no `id` |
| legacy path syntax | an Axum 0.7 style `:id` segment where Moso wants `{id}` |
| missing or invalid configuration | a required key with no value, or one that does not coerce to its type |
| duplicate operationId | two operations deriving the same id |
| schema name collision | two `#[derive(Schema)]` types with the same name in the document |
| provider failed | a `provide_with` factory returned `Err` |
| provider ordering | a factory read a provider registered after it |
| provider cycle | two factories that each need the other |
| middleware ordering | a slot placed where the stack forbids it |

A missing provider that is genuinely optional is declared with `ProviderReq::optional_of::<T>()` in
a custom extractor and is skipped by step 4.

`build_unchecked()` skips the report and hands back an `App` from a broken builder. It exists for
`moso openapi export --force` and for tests that want to inspect a broken application. Never call it
from `main`.

> [!WARNING]
> `provide_with` needs an ambient multi-threaded Tokio runtime. `build()` is synchronous and the
> factory is not, so the future is driven on the runtime that will still exist afterwards. Calling
> `build()` on a current-thread runtime, or outside any runtime, is a boot error naming the fix. In
> tests use `#[tokio::test(flavor = "multi_thread", worker_threads = 2)]`, or construct the value
> first and register it with `.provide(value)`.

## What `serve()` does

`serve()` binds `ServerConfig::bind` and delegates to `serve_on`, which does the rest:

1. Runs the `on_startup` hooks in registration order, then acquires the lifespan guards. Hooks come
   first on purpose: a lifespan usually opens an expensive resource and a hook usually checks the
   cheap thing, so a misconfigured deployment fails in a second rather than after a 30 s connect
   timeout. A failure releases whatever was already acquired and returns `Err`.
2. Logs one line at `target: "moso::app"` with the bound address, the profile, and the docs URL when
   `expose_docs` is on.
3. Spawns a task awaiting SIGINT or SIGTERM (Ctrl-C only on non-Unix), which calls `signal.trigger()`.
4. Serves with Axum's graceful shutdown wired to that signal.
5. On the signal: `/readyz` starts answering 503 immediately, `/healthz` keeps answering 200, and the
   listener stops accepting.
6. Drains in-flight connections, bounded by `shutdown_grace`.
7. Waits for outstanding `ShutdownGuard`s on what is left of the same budget.
8. Runs the `on_shutdown` hooks in reverse order, drops the lifespan guards innermost first, and logs
   `shutdown complete`.

`serve` and `serve_on` return `Result<()>`. Neither calls `std::process::exit`, so the exit code is
your `main`'s business.

Nothing reads a bind address out of your config struct automatically, and there is no `MOSO_BIND` or
`MOSO_PORT`: `serve` binds `ServerConfig::bind`, which defaults to `0.0.0.0:3000`. `HttpConfig` and
`ServerConfig` are plain structs rather than `#[config(nested)]` sections, because `moso-core` cannot
depend on the macro crate, so where the address comes from stays your decision. Read what the
listener needs before the configuration moves into the builder, where it becomes a provider:

```rust title="src/lib.rs"
use moso::http_config::ServerConfig;

pub fn build() -> Result<App> {
    let config = AppConfig::load()?;

    // Read these first: `config` is about to move into the builder.
    let bind = config.bind;
    let grace = config.shutdown_grace;

    App::new(config)
        .server_config(ServerConfig { bind, shutdown_grace: grace, ..ServerConfig::default() })
        .mount_at("/api/v1", routes::router())
        .build()
}
```

## Health and readiness

Two probes with two different jobs.

`/healthz` is liveness. It touches nothing. If the handler runs, the process is alive and the runtime
is scheduling, which is the entire question. A liveness probe that queries the database turns a
database blip into a rolling restart of every instance, which turns a blip into an outage. It also
keeps answering 200 during the drain, because a 503 there invites the orchestrator to `SIGKILL` the
process mid-drain.

`/readyz` is readiness: should this instance receive traffic right now? It runs every registered
check and folds the results into one report. Both probes answer JSON with `cache-control: no-store`
and neither passes through the middleware stack.

### Registering a check

```rust title="src/health.rs"
use moso::prelude::*;
use moso::{BoxFuture, HealthCheck, HealthStatus, Resolver};

/// A database handle.
pub struct Db;
impl Db {
    /// Round-trip one cheap statement.
    async fn ping(&self) -> Result<()> { Ok(()) }
}

/// Is the database reachable?
pub struct DatabaseCheck;

impl HealthCheck for DatabaseCheck {
    fn check<'a>(&'a self, r: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
        Box::pin(async move {
            match r.get::<Db>() {
                Ok(db) => match db.ping().await {
                    Ok(()) => HealthStatus::Up,
                    Err(e) => HealthStatus::Down(e.to_string()),
                },
                Err(e) => HealthStatus::Down(e.to_string()),
            }
        })
    }
}
```

Register it with `.provide(db).health_check("database", DatabaseCheck)`. The name is the key it
appears under in the report, so keep it stable: an operator greps for it.

### Critical and non-critical

`critical()` defaults to `true`. Override it to `false` for a dependency whose failure should be
visible without taking the instance out of rotation:

```rust
impl HealthCheck for StoreIsReachable {
    fn check<'a>(&'a self, resolver: &'a Resolver) -> BoxFuture<'a, HealthStatus> {
        Box::pin(async move {
            match resolver.get::<Store>() {
                Ok(store) => HealthStatus::Degraded(format!("{} posts", store.len())),
                Err(error) => HealthStatus::Down(error.to_string()),
            }
        })
    }

    fn critical(&self) -> bool {
        false
    }
}
```

Three statuses, and only critical checks move the top-level `status`:

| `HealthStatus` | Renders as | Effect on a critical check |
| --- | --- | --- |
| `Up` | `up` | none |
| `Degraded(String)` | `degraded: <reason>` | `status` becomes at worst `degraded`, still **200** |
| `Down(String)` | `down: <reason>` | `status` becomes `down`, **503** |

Degraded is 200 on purpose. It means "serving, imperfectly"; answering 503 would take the last
instance out of rotation over a warm cache being cold, which turns a partial outage into a total one.

A report with a critical degraded check, a non-critical failure, and a declared version:

```json
{
  "status": "degraded",
  "checks": {
    "database": "up",
    "cache": "degraded: cold, 12% hit rate",
    "search": "down: connection refused"
  },
  "version": "1.4.2",
  "uptime_s": 412
}
```

### The budget

Every check runs concurrently, and each one is bounded by the **whole** 2 s `READINESS_BUDGET`
rather than a share of it, so two checks that each take 1.5 s still make a ready instance because
they overlap. A check that exceeds the budget is reported as `down: timed out after 2s`: a probe that
hangs is a probe that failed, and leaving the orchestrator waiting is worse than answering.

Keep checks cheap. `/readyz` runs every one of them on every probe, several times a second, forever.
A `SELECT 1` is the shape; a table scan is not.

### Version, commit and uptime

`version` comes from the OpenAPI document's `info.version` when you declared one
(`.openapi(|d| d.version(env!("CARGO_PKG_VERSION")))`), otherwise `MOSO_VERSION`, otherwise the
framework's own version. `commit` is read from `MOSO_COMMIT`, then `GIT_COMMIT`, then `SOURCE_COMMIT`,
and is omitted entirely when none is set.

`uptime_s` is measured from `build()`, not from the first accepted connection, so it includes the
time your startup hooks took.

### Moving the probe paths

Some platforms reserve `/healthz`. Both paths are fields on `HttpConfig`:

```rust
use moso::http_config::HttpConfig;

App::new(config).http_config(HttpConfig {
    health_path: "/internal/healthz".to_owned(),
    ready_path: "/internal/readyz".to_owned(),
    ..HttpConfig::default()
})
```

A path that is not rooted is **not mounted at all** rather than reported: setting `health_path` to
`""` disables the probe silently, because `axum::Router::route` panics on an unrooted path.

### Running the checks outside HTTP

The same checks answer from a CLI subcommand or a test, with no listener involved:

```rust
let report = moso::health::readiness_report(
    app.state().health_checks(),
    &app.resolver(),
    moso::health::READINESS_BUDGET,
    app.state().uptime(),
)
.await;

assert!(report.is_ready());
```

`moso::health::run_checks` gives you the raw `(name, status, critical)` rows without the reporting
layer.

> [!IMPORTANT]
> The batteries do **not** register their own checks. `moso::db::Db::health_check()`,
> `Kv::health_check()` and `StorageConfig::health_check(..)` exist, but you pass them to
> `.health_check(name, check)` yourself. That means constructing the handle before the builder:
> `let db = moso::db::Db::connect_url(config.database_url.expose()).await?;` then
> `.health_check("database", db.health_check()).provide(db)`.

## Graceful shutdown

`SIGTERM` arrives. What happens next, in order:

```text
SIGTERM
  ├─▶ /readyz answers 503 within milliseconds, so the load balancer removes this
  │   instance while it is still serving what it already accepted
  ├─▶ the listener stops accepting
  ├─▶ in-flight requests drain, up to server.shutdown_grace (25 s by default)
  ├─▶ outstanding ShutdownGuards drain, on what is left of the same budget
  ├─▶ on_shutdown hooks run in reverse registration order
  └─▶ lifespan guards drop, innermost first
```

That first step is the whole of graceful shutdown in a load-balanced deployment. `/readyz` short
circuits on the same flag the signal sets, so it does not run a single check:

```json
{
  "status": "down",
  "checks": { "process": "down: shutting down, draining in-flight requests" },
  "version": "1.4.2",
  "uptime_s": 3600
}
```

### The drain deadline

`shutdown_grace` defaults to 25 s. That is deliberately under the 30 s an orchestrator typically
allows before `SIGKILL`, because a grace longer than the kill timeout means the process is killed
mid-drain, which is the exact thing the grace existed to prevent.

Axum's `with_graceful_shutdown` waits for every connection without a bound. Moso races it against a
deadline and, when the deadline wins, logs a `WARN` and drops the serve future, closing whatever is
left. Both drain stages measure from the moment the signal fired rather than each starting a fresh
grace, so two stages cannot double the deploy window. Triggering is idempotent, and a second trigger
does not restart the clock.

Set your own through `shutdown_grace` on the `ServerConfig` you hand to `.server_config(..)`, and
keep it under whatever your orchestrator waits before `SIGKILL`.

### Cooperating from a handler

A long-lived handler must stop on its own. Take `Inject<Signal>` and select on it:

```rust
use moso::prelude::*;
use moso::response::NoContent;
use moso::Signal;

/// Work that stops when the process is asked to.
#[endpoint]
async fn export(Inject(signal): Inject<Signal>) -> Result<NoContent> {
    tokio::select! {
        () = signal.recv() => {}
        () = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
    }
    Ok(NoContent)
}
```

`recv()` after the trigger returns immediately, so a handler that starts during the drain does not
wait out the grace. `signal.is_shutting_down()` answers the same question without awaiting, and
`signal.trigger()` starts a shutdown from inside the application, which is how a `/admin/stop`
endpoint or a fatal background error stops the process.

## Work that must outlive a request

Axum's graceful shutdown drains HTTP connections. It knows nothing about a task you spawned, a
consumer loop, or a stream whose response has already been flushed. For those, take a named guard
from the `Drain`:

```rust
use moso::prelude::*;
use moso::response::NoContent;
use moso::shutdown::Drain;
use moso::Signal;

/// Stream until the process is asked to stop.
#[endpoint]
async fn events(
    Inject(drain): Inject<Drain>,
    Inject(signal): Inject<Signal>,
) -> Result<NoContent> {
    let _guard = drain.guard("GET /events");
    loop {
        tokio::select! {
            () = signal.recv() => break,
            () = tokio::time::sleep(std::time::Duration::from_secs(1)) => { /* push a frame */ }
        }
    }
    Ok(NoContent)
}
```

The guard deregisters on drop. Outside a request, reach the same drain through
`app.state().drain().guard("nightly-export")`.

Whatever is still held when the grace expires is named in one `WARN` line, de-duplicated with `(xN)`
counts:

```text
WARN moso::app: the drain did not finish inside the grace period; these are still open.
     A long-lived handler must select on `Inject<Signal>` and close.
     grace=25s outstanding=3 still_open="GET /events (x2), nightly-export"
```

Without that line the symptom is "deploys take 25 seconds" and nobody knows why.

> [!NOTE]
> Nothing takes a guard per HTTP request automatically. The drain counts only what you asked for, so
> the warning names your long-lived units of work rather than arbitrary slow requests.

Background workers follow the same pattern from a startup hook. `Worker::run` and `Scheduler::run` in
[the jobs battery](./jobs.md) take the framework's `Signal`, so a worker spawned at boot stops with
the process:

```rust
.on_startup(|r| async move {
    let signal = r.get::<Signal>()?.as_ref().clone();
    let worker = /* build your Worker from the providers on `r` */;
    tokio::spawn(async move { worker.run(signal).await });
    Ok(())
})
```

## Lifecycle hooks

Three registration points, with ordering chosen so a hook can rely on everything registered before it
still being alive.

| Hook | Runs | Order | Failure |
| --- | --- | --- | --- |
| `on_startup(\|r\| async { .. })` | before the listener binds | registration order | aborts boot, releases what was acquired |
| `lifespan(\|r\| async { Ok(guard) })` | after the startup hooks | registration order | aborts boot |
| `on_shutdown(\|r\| async { .. })` | after the drain | **reverse** registration order | cannot fail; returns `()` |

Lifespan guards drop innermost first, **after** the shutdown hooks, so a hook can still use the
resource it is about to release.

```rust
use moso::prelude::*;

/// Unsubscribes on drop, after the drain has finished.
pub struct Consumer(String);
impl Drop for Consumer {
    fn drop(&mut self) { /* unsubscribe */ }
}

let app = App::new(config)
    .lifespan(|r| async move {
        let cfg = r.config::<AppConfig>()?;
        Ok(Consumer(cfg.broker.clone()))
    });
```

Every hook receives a `Resolver`, which reads the frozen provider map outside a request and returns
`Result<Arc<T>>` with a message naming the type when it is absent. See
[dependency injection](./dependency-injection.md) for the rest of that surface.

## More than one application in a process

`App` owns everything it needs. There is no global registry, no link-time collection, no ambient
state, which is exactly what makes two applications in one process work:

```rust
let public = tokio::spawn(build_public()?.serve_on(public_listener));
let admin = tokio::spawn(build_admin()?.serve_on(admin_listener));
```

Each has its own `Signal`, its own `Drain`, its own provider map and its own document. Trigger them
independently with `app.shutdown_signal()`. This is the same property that makes test isolation work,
and it is why [testing](./testing.md) can boot the real application per test.

Three ways to run one:

- `serve()` binds `ServerConfig::bind` and takes over the process.
- `serve_on(listener)` uses a listener you bound: port 0 in a test, socket activation, or a TLS
  terminator handing over an accepted socket.
- `serve_workers()` runs the startup hooks and lifespan guards and waits for the signal, with no HTTP
  listener at all. The same binary in a worker role cannot drift from what the web process proved at
  boot.

`into_service()` is the full escape hatch: it returns an `axum::Router<()>` with state attached, so
`Inject<T>` still works when you drive it with your own accept loop. Startup hooks and lifespan
guards do **not** run, because they belong to `serve`. A test that needs them should use `serve_on`
with a port-0 listener.

## Inspecting a built application

| Call | Gives you |
| --- | --- |
| `app.state()` | the frozen `AppState`: providers, HTTP limits, server config, profile, signal, drain, uptime, document, checks |
| `app.resolver()` | a `Resolver` over the provider map, outside a request |
| `app.openapi()` | the generated document, in every build, whatever the `openapi` feature says |
| `app.router_info()` | one `RouteInfo` per route in registration order, which is what `moso routes` prints |
| `app.shutdown_signal()` | a clone of the signal, for triggering shutdown from a test or a supervisor |

## Failure modes

- **`build()` on a current-thread runtime with a `provide_with` registered** is a boot error, not a
  deadlock. Use `#[tokio::main]` (multi-threaded by default) or `.provide(value)`.
- **A `provide_with` factory can only read providers registered before it.** There is no topological
  sort. The wrong order is a boot error reading "provider `Search` is built before `Db`, which it
  needs", with a "swap the two registrations" fix.
- **Registration is last-write-wins**, including over the framework's own `Signal`, `Drain` and
  `BlockingPool`. That is how test overrides work, and also how you shadow a framework provider by
  accident.
- **Reserved-path checking ignores `expose_docs`.** Registering `GET /docs` is a boot error even in a
  production profile where the framework would not have mounted it.
- **Dev and test both expose `/docs`;** production does not. An explicit `.http_config(..)` wins over
  the profile default in every case.
- **`expose_internal_errors` is a warning, not a refusal.** It is announced at boot at `WARN` in
  every profile and left on, because there are legitimate uses and a framework that refuses is a
  framework people patch out.
- **On non-Unix platforms only Ctrl-C stops the process.** A failure to install the SIGTERM handler is
  logged at `WARN` and degraded to Ctrl-C rather than aborting boot.
- **Who flushes tracing on the way out depends on who installed the subscriber.** With the
  `subscriber` feature, `AppBuilder::tracing_config` installs one at serve time and the returned
  `TracingGuard` flushes it on shutdown. Without it, your `main` owns the subscriber and its guard, so
  `shutdown complete` is the last event written through it and a missing line in your collector is a
  flush you did not do. See [observability](./observability.md).

### Configuration that lives outside `build()`

A few knobs on `ServerConfig` are owned elsewhere on purpose, so it is worth knowing where they act.

- **`ServerConfig::validate` is a check you call directly.** `build()` does not run it, so validate
  your `server.tls` and `shutdown_grace` settings explicitly when you set them; the function is there,
  written and tested, for exactly that.
- **TLS termination is delegated to the proxy or load balancer in front of the process.** `TlsConfig`
  reserves the keys so `moso config` and `.env.example` know about them, leaving room for a later
  in-process option without a breaking change.
- **`keep_alive`, `nodelay`, `http2_prior_knowledge` and `worker_threads` configure the accept loop,
  which `axum::serve` owns.** Drive `into_service()` with your own
  `hyper_util::server::conn::auto` loop to set them; `worker_threads` belongs to the runtime you build
  in `main`.

A couple of names from the design documents live elsewhere too: worker startup goes through
`serve_workers()` rather than a `mount_jobs`/`with_auth`/`with_admin` builder, the test harness is
`moso_test::TestApp` rather than `App::spawn_test`, and process-role dispatch is your `main`'s job
rather than a `moso::runtime::main!`.

## See also

- [Configuration](./configuration.md) for `Config`, profiles and where `HttpConfig` values come from.
- [Dependency injection](./dependency-injection.md) for `provide`, `provide_with`, `provide_dyn` and
  the `Resolver`.
- [Errors](./errors.md) for what a boot error is made of and how runtime errors differ.
- [Middleware](./middleware.md) for the stack that boot configures and validates.
- [Testing](./testing.md) for booting the real application per test, over a socket or in process.
- [Observability](./observability.md) for the log lines this page mentions and the subscriber you own.
