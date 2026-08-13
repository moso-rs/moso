# 02 - Architecture

> **Status: the shipped build is the CORE + MODEL layers.** The DATA layer and the BATTERIES row of
> the diagram below are design intent - no such crate exists. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## The layer cake

```
┌───────────────────────────────────────────────────────────────────────────┐
│  YOUR APPLICATION                                                          │
│  src/routes/*.rs   src/models/*.rs   src/jobs/*.rs   src/services/*.rs     │
└───────────────────────────────────────────────────────────────────────────┘
                                    │  depends on exactly one crate: `moso`
┌───────────────────────────────────▼───────────────────────────────────────┐
│  FACADE            moso                                                    │
│  re-exports + prelude + feature flags. Contains no logic.                  │
└───────────────────────────────────────────────────────────────────────────┘
┌──────────────┬──────────────┬──────────────┬──────────────┬───────────────┐
│  BATTERIES   │ moso-auth ⛔ │ moso-authz ⛔│ moso-jobs ⛔ │ moso-admin ⛔ │
│              │ moso-mail ⛔ │ moso-storage⛔│ moso-kv ⛔   │ moso-test  ✅ │
└──────────────┴──────────────┴──────────────┴──────────────┴───────────────┘
┌───────────────────────────────┬───────────────────────────────────────────┐
│  MODEL LAYER                  │  DATA LAYER                               │
│  moso-schema ✅ (Schema,      │  moso-orm  ⛔ (Entity, Query, Relations)  │
│    Validate, AND the JSON     │  moso-migrate ⛔ (snapshot diff, runner)  │
│    Schema model - see D2)     │  moso-sql  ⛔ (SEALED query facade)       │
│  moso-openapi ✅ (3.1 doc)    │                                           │
└───────────────────────────────┴───────────────────────────────────────────┘
┌───────────────────────────────────────────────────────────────────────────┐
│  CORE            moso-core ✅                                              │
│  App · Router · Handler · Extract · IntoResponse · Error · Config · DI     │
└───────────────────────────────────────────────────────────────────────────┘
┌───────────────────────────────────────────────────────────────────────────┐
│  SUBSTRATE       axum · tower · tower-http · hyper · tokio · serde         │
│                  (sqlx would join this row with the data layer)           │
└───────────────────────────────────────────────────────────────────────────┘
```

Two corrections to the picture, both load-bearing:

- **`moso-openapi` sits *below* `moso-core`, not beside it.** `moso-core` depends on it
  **unconditionally** (decision **D1**), because `Extract::describe`, `ExtractBody::describe`,
  `Endpoint::spec`, `Describe::describe`, `Guard::describe` and `Dependency::describe` all name
  `OperationBuilder`. Making the dependency optional would make six public trait signatures
  feature-dependent - a crate built without the feature would present a *different trait* under the
  same name, and every downstream extractor would need `#[cfg]` on its impl. The `openapi` cargo
  feature therefore controls exactly one thing: whether `/docs` and `/openapi.json` are **mounted**.
  The document is assembled either way.
- **The JSON Schema model lives in `moso-schema`, not `moso-openapi`** (decision **D2**), for a
  dependency-direction reason: `Schema::json_schema(&mut SchemaGenerator)` must name the generator,
  so if the generator lived in `moso-openapi` then `moso-schema` would depend on it and the claim
  "`moso-schema` is usable standalone, with no HTTP and no OpenAPI" would be false.
  `moso_schema::json_schema` owns `SchemaNode`, `SchemaGenerator`, `SchemaRef` and the builders;
  `moso-openapi` depends on `moso-schema` and embeds the nodes into `components/schemas`.

Macros (`moso-macros`) sit orthogonally: they generate code that references **only**
`::moso::__private::*` - never a runtime crate by name, never the substrate directly. This means a
macro can be re-implemented without touching runtime crates and vice versa, and it is why
`moso-macros` depends on no Moso crate at all.

## The relationship with Axum - precisely

**Axum is the engine. Moso is the cockpit.** This is a load-bearing decision; get it exactly right.

### What we take from Axum
- `axum::Router` and `matchit` routing, including nesting, fallbacks, and method routing.
- `axum::serve`, connection handling, HTTP/1 + HTTP/2 via hyper.
- The Tower `Service`/`Layer` model and the whole `tower-http` middleware ecosystem.
- WebSocket upgrade, SSE, multipart, and `axum-extra` utilities.
- Its `IntoResponse` - Moso's `IntoResponse` is a **re-export**, not a parallel trait. Anything that
  is an Axum response is a Moso response.

### What Moso owns
- **`Extract` / `ExtractBody`** - Moso's own extractor traits (see `01-http/12`). They are strict
  supersets of Axum's `FromRequestParts` / `FromRequest`: same extraction method plus a
  `describe()` method that contributes to the OpenAPI operation and a `PROVIDER_REQ` const that
  feeds the boot-time DI check.
  Interop is by **wrapper in both directions**, not by blanket impl: `Opaque<T>` / `OpaqueBody<T>`
  lift an Axum extractor into a Moso handler, and `MosoExt<T>` / `MosoExtBody<T>` do the reverse.
  The design originally specified `impl<T: Extract> axum::extract::FromRequestParts<()> for T`;
  that is an orphan-rule violation (E0210, `T` uncovered), so it does not exist. See the
  implementation note on [ADR-0002](../adr/0002-own-the-handler-traits.md). Neither adapter
  contributes to the documentation, which is the honest behaviour.
- **`Handler`** - Moso's handler trait, with `#[diagnostic::on_unimplemented]` written by us. Its
  associated `type Endpoint` cannot be attached to a plain `fn` item, so `#[endpoint]` emits a
  companion unit struct and `routes!` / `ep!` name it. See
  [ADR-0013](../adr/0013-handler-registration.md).
- **`Router`** - accumulates `RouteEntry` values (method, path, `OperationSpec`, provider
  requirements, the erased handler, layers, guards) and lowers to `axum::Router` at
  `App::build()`. It is *not* a wrapper around a live `axum::Router`: keeping the entries as data
  until boot is what lets `.tag()`, `.guard()` and `.nest()` retro-apply metadata to routes already
  registered, and what makes `Router::conflicts()` and `moso routes` possible.
- **`App`** - lifecycle, DI container, config, boot-time validation, shutdown. Axum has no
  equivalent; this is where the "framework" lives.
- **`Error`** - a concrete error type with a taxonomy and RFC 9457 rendering.

### Why not just use Axum's traits?
Three reasons, in order of weight:

1. **OpenAPI.** An extractor must be able to say "I read a `Json<CreateUser>` body, here is its
   schema, here is the 422 I can produce." Axum's traits have no place to put that. Bolting it on
   externally is exactly the `utoipa` annotation-drift problem we exist to solve.
2. **Diagnostics.** We must own the trait that fails so we can own the message. Axum's `Handler`
   error is the single most-complained-about thing in the ecosystem.
3. **DI.** `Inject<T>` needs a per-request dependency cache and boot-time graph validation. Axum's
   `State`/`FromRef` gives compile errors that read as trait-resolution vomit and can't express
   "resolved once per request, shared between three extractors."

### Escape hatches (all first-class, all tested)

```rust
// as built - moso-core::router
impl Router {
    /// Consume the Moso router, yielding the underlying Axum router.
    /// OpenAPI metadata is dropped. Use at the very edge of your app.
    pub fn into_axum(self) -> axum::Router<()>;

    /// Mount an arbitrary Axum router at `prefix`. Contributes nothing to OpenAPI.
    pub fn mount_axum(self, prefix: &'static str, router: axum::Router<()>) -> Self;

    /// Apply any `tower::Layer` to every route registered so far.
    pub fn layer<L>(self, layer: L) -> Self
        where L: tower::Layer<Route> + Clone + Send + Sync + 'static, /* + Service bounds */;
}
```

`.describe_mount(path, ops)` was sketched and not built: an `axum::Router` genuinely carries no
metadata Moso can read, so the honest options were "document nothing" or "let the user hand-write an
`OperationSpec`". The first shipped; the second is additive if anyone asks for it.

```rust
// as built - using an Axum extractor inside a Moso handler
use moso::extract::Opaque;

#[moso::endpoint]
async fn peek(Opaque(uri): Opaque<axum::extract::OriginalUri>) -> Result<NoContent> { /* ... */ }
```

At the data layer, symmetrically, `Db::pool() -> &sqlx::PgPool` would expose the raw pool.
⛔ There is no `moso-orm` in this build, so that escape hatch does not exist yet.

**Rule:** every Moso abstraction MUST expose the layer beneath it. An abstraction you cannot escape
is a cage, and Rust developers correctly refuse cages.

## The life of a request

```
  hyper accepts connection
        │
        ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ tower-http default stack (order fixed by moso-core):         │
  │  1. CatchPanic          → 500 problem+json, logged            │
  │  2. RequestId           → x-request-id, into tracing span     │
  │  3. TraceLayer          → span open, timing, OTel context     │
  │  4. Timeout             → configurable, default 30s           │
  │  5. NormalizePath       → trailing-slash policy               │
  │  6. Cors                → off unless configured               │
  │  7. SecurityHeaders     → HSTS/X-CTO/Referrer-Policy/CSP      │
  │  8. Compression         → br/gzip, respects Accept-Encoding   │
  │  9. RateLimit           → optional, KV-backed                 │
  │ 10. Session             → optional, loads session lazily      │
  └─────────────────────────────────────────────────────────────┘
        │
        ▼
  axum::Router match  →  Moso HandlerAdapter
        │
        ▼
  ┌─────────────────────────────────────────────────────────────┐
  │ RequestCtx created (one allocation, arena-backed):           │
  │   · Arc<AppState>  (providers, config, db, kv)               │
  │   · DependencyCache (per-request memoisation for Depends)    │
  │   · Extensions (request id, trace id, session handle)        │
  └─────────────────────────────────────────────────────────────┘
        │
        ▼
  Extractors run left-to-right:
     non-body extractors (Parts)  →  exactly one body extractor (last)
        │            │
        │            └─ Json<T>: deserialize → T::validate() → 422 on failure
        │
        ▼
  Handler future (asserted Send by generated code)
        │
        ▼
  Result<R, Error>  →  IntoResponse
        │                    └─ Error → problem+json (RFC 9457), status from taxonomy
        ▼
  response travels back out through the stack (compression, headers, trace close)
```

**Corrections to the diagram, as built.** The shipped stack has **14** named slots, not 10, in this
order: `CatchPanic`, `RequestId`, `Trace`, `SensitiveHeaders`, `CatchError`, `Timeout`, `BodyLimit`,
`NormalizePath`, `Cors`, `SecurityHeaders`, `Compression`, `RateLimit`, `Session`, `Metrics`
(`Slot::ORDER`, outermost first). Two of them are **reserved and empty**: nothing is installed into
`RateLimit` (it needs a KV backend) or `Session` (it needs auth). `Metrics` is innermost by design -
it runs after routing, so its `route` label is the *pattern* and not one time series per id.
`RequestCtx` is created by the handler adapter and inserted into the request extensions, which is
how `MosoExt<T>`, `ctx_from_parts` and `#[middleware]`'s leading extractors reach it.
Full, current order and rationale: [`01-http/17-middleware.md`](../01-http/17-middleware.md).

### Invariants the framework enforces

- **I1** At most one body extractor per handler, and it MUST be last. Violation is a compile error
  with a hand-written message (`01-http/12`, `04-devex/41`).
- **I2** Every extractor is `Send`. Every handler future is `Send`. Asserted by generated code so
  the error points at the user's line, not into `axum::handler`.
- **I3** A handler's declared response type is the type documented in OpenAPI. There is no way to
  return an undocumented shape except `Raw<T>`, which documents itself as `unknown`.
- **I4** All `Inject<T>` requirements are satisfiable at `App::build()`. Boot fails with a list of
  missing providers and the handlers that need them.

## Dependency injection: the two-tier model

Moso deliberately splits what FastAPI conflates.

| Tier | Trait | Lifetime | Example | Failure mode |
| --- | --- | --- | --- | --- |
| **Providers** | `Inject<T>` | app lifetime, `Arc`-shared | a pool, a client, the root `Config` | Missing → **boot error**, never a request error |
| **Dependencies** | `Dependency` / `Depends<T>` | one request, memoised | `CurrentUser`, `Tenant` | May fail → typed `Error` → HTTP response |

(There is no `Provider<T>` trait. A provider is a *value* in a `HashMap<TypeId, Arc<dyn Any>>`
registered with `App::provide`; the trait side of the tier is the extractor `Inject<T>` and its
`PROVIDER_REQ`.)

This is why Moso can validate at boot what Axum can only fail on at runtime: providers are a closed
set known at `App::build()`, and `#[endpoint]` emits the list of provider `TypeId`s each handler
needs.

Full spec in [`01-http/15-dependency-injection.md`](../01-http/15-dependency-injection.md).

## Compile-time architecture (this is a runtime concern too)

Three rules keep app rebuilds small. They constrain *our* code, not the user's.

- **A1 - Shallow generics at the boundary.** `Router`, `App`, and `Error` are concrete types. No
  `Router<S>` state parameter (this is the main reason Axum apps monomorphise heavily and why
  `FromRef` errors are so bad). App state lives in a type-map behind `Arc`.
- **A2 - Erase early.** A handler is converted to a `BoxCloneService` at registration. The
  generic surface of `#[endpoint]` codegen is ~40 lines regardless of app size.
- **A3 - Macros emit little.** Every derive's output size is measured in CI
  (`xtask expand-size`). Budgets in `04-devex/42-compile-times.md`.

Consequence for users: the recommended layout puts routes in a library crate and `main.rs` as a
6-line shim, so `cargo build` on a handler edit recompiles one crate, links, and is done.

## Feature-flag topology

The facade crate `moso` has **no default features that pull a database driver**. This matters: a
user evaluating Moso for a stateless service must not compile sqlx.

```toml
# as built - moso/Cargo.toml
[features]
default     = ["http", "openapi", "tracing"]

# `moso-core` is an unconditional dependency of the facade, so `http` is an accepted
# no-op kept for compatibility with the topology described here.
http        = []
openapi     = ["moso-core/openapi", "dep:moso-openapi"]
tracing     = ["moso-core/tracing"]
compression = ["moso-core/compression"]
cors        = ["moso-core/cors"]
multipart   = ["moso-core/multipart"]
ws          = ["moso-core/ws"]
```

⛔ The battery features - `orm`, `sqlite`, `mysql`, `kv`, `redis`, `auth`, `oauth`, `passkeys`,
`authz`, `jobs`, `mail`, `storage`, `admin` and the `full` umbrella - **do not exist**, because the
crates they would enable do not exist. The design intent above stands; the manifest will grow into
it. Note that `full` is also what the ≤ 260-crate dependency budget in `03-crate-layout.md` was to
be measured against, so that budget is currently unmeasurable.

The principle that produced this shape held: **the facade has no default feature that pulls a
database driver.** A user evaluating Moso for a stateless service compiles no SQL at all - trivially
so today.

CI MUST build the powerset of the "interesting" flag combinations (`cargo hack --feature-powerset
--depth 2`) - feature-flag rot is how batteries-included frameworks become unbuildable.
⛔ **There is no CI configuration in the repository**, so this is currently unenforced.

## Threading and blocking

- All Moso APIs are `async`. Anything CPU-bound the framework does internally (argon2 hashing,
  image thumbnailing in `moso-storage`, template rendering above a size threshold) goes through
  `moso::task::blocking()`, a thin wrapper over `tokio::task::spawn_blocking` with a dedicated,
  bounded pool and a tracing span.
- `moso::task::blocking` is public and documented, because the #1 async footgun for FastAPI
  refugees is calling blocking code in an async fn. It is a bounded `BlockingPool` with a tracing
  span, reachable per-app through `AppState::blocking()` and process-wide through
  `BlockingPool::global()`.
  ⛔ `moso check`'s `blocking_in_async` lint does not exist - there is no `moso check`.

## Error philosophy

One concrete `moso::Error` type, not a generic `E`. Rationale:

- Handlers return `Result<T, moso::Error>` (aliased `moso::Result<T>`), so `?` works across every
  battery without `.map_err`.
- `Error` carries a **taxonomy** (`ErrorKind`) that maps to a status code, a machine-readable
  `type` URI, and a decision about whether the detail is safe to show the client.
- User error types integrate via `impl From<MyError> for moso::Error` or
  `#[derive(moso::Error)]`, which generates the status/type mapping from attributes.

Full spec in [`01-http/16-errors.md`](../01-http/16-errors.md).

## What is deliberately *not* in the architecture

- **No actor system.** (Actix's vestigial actor model is a documented conceptual overhang.)
- **No link-time registries** (`inventory`/`ctor`). They break in static libs, wasm, and tests;
  they make ordering non-deterministic; and they hide the route table from `cargo expand`.
  Routes are registered explicitly via `routes!`. See ADR-0004. Neither crate is in the dependency
  tree, and the price is visible: `moso routes` and `moso openapi export` have to *run* the
  application binary (with `--dump-routes` / `--dump-openapi`) because the route table is ordinary
  Rust that only exists once `router()` has been called.
- **No separate codegen build step.** Pavex's transpiler buys better diagnostics at the cost of a
  non-standard build. We take `#[diagnostic::on_unimplemented]` + assertion codegen + `moso check`
  instead, and accept a slightly lower diagnostic ceiling. See ADR-0003.
- **No runtime reflection.** Rust has none; every "automatic" behaviour is a macro expansion you
  can print.
