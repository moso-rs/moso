# 03 — Crate Layout

> **Status: partially implemented.** The HTTP half of this document is built. The data layer and the
> batteries are not, and are marked ⛔ below. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md) for the
> full ledger.

## Repository layout — as built

```
moso/
├── Cargo.toml                  # workspace, resolver 3, edition 2024
├── Cargo.lock
├── rustfmt.toml
├── crates/
│   ├── moso/                   # facade + prelude + __private   (the only user-facing dep)
│   ├── moso-core/              # App, Router, Handler, Extract, Error, DI, Config, middleware
│   ├── moso-macros/            # #[endpoint], routes!, ep!, #[middleware], 6 derives
│   ├── moso-schema/            # Schema, Validate, the JSON Schema model, constrained types
│   ├── moso-openapi/           # OpenAPI 3.1 model, builders, the embedded doc UI, diffing
│   ├── moso-test/              # TestApp, TestClient, log/contract assertions
│   ├── moso-ui-tests/          # trybuild corpus: wrong programs + snapshotted diagnostics
│   └── moso-cli/               # the `moso` binary
├── examples/
│   ├── minimal/                # hello world
│   └── crud/                   # the tutorial app: posts CRUD, guards, config, custom errors
└── docs/                       # these documents
```

### Planned, not present

`rust-toolchain.toml`, `deny.toml`, `.cargo/config.toml`, `xtask/`, `templates/` (the `moso new`
templates are `include_str!`-embedded in `moso-cli/templates/`, not `rust-embed`'d),
`examples/realworld`, `examples/bench`, and `tests/integration/` (cross-crate behaviour lives in
each crate's own `tests/` directory instead).

### Out of scope for this build

`moso-sql`, `moso-orm`, `moso-orm-macros`, `moso-migrate`, `moso-kv`, `moso-auth`, `moso-authz`,
`moso-jobs`, `moso-mail`, `moso-storage`, `moso-admin`. **None of these crates exists, and nothing
in the shipped code references them.** Their specifications below are retained as design intent, not
as a description of anything you can compile.

## Crate-by-crate specification

Each entry gives: purpose, public surface headline, dependencies, feature flags, and the work
package that builds it (see `05-delivery/51-work-packages.md`).

---

### `moso` — facade ✅

**Purpose:** the single dependency a user adds. Re-exports, prelude, feature plumbing, and the
hidden `__private` module macro output resolves against. **Contains no logic and no types of its
own.**

```rust
// as built — moso/src/lib.rs (shape)
pub mod prelude;
#[doc(hidden)] pub mod __private;          // the ONLY path macro output may name

pub use moso_core::{
    app, config, ctx, deps, di, error, extract, handler, health, http_config, middleware,
    openapi, response, router, schema, shutdown, task,
};
pub use moso_core::{App, AppBuilder, AppState, BoxError, BoxFuture, Config, Dependency, Depends,
    Describe, Endpoint, Error, ErrorKind, Extract, ExtractBody, Guard, Handler, HandlerFn,
    HealthCheck, HealthStatus, Inject, IntoResponse, Lifespan, Limits, MethodRouter,
    MiddlewareStack, Next, Problem, ProviderReq, Request, RequestCtx, Resolver, Response, Result,
    Route, Router, Signal, Slot};
pub use moso_macros::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub use moso_core::{COMPONENTS_SCHEMAS_PREFIX, REQUEST_ID_HEADER};

pub mod prelude {
    pub use moso_core::{App, Router, Error, Result};
    pub use moso_core::extract::{Depends, Form, Inject, Json, Path, Query};
    pub use moso_core::{Dependency, Extract, RequestCtx};
    pub use moso_core::IntoResponse;
    pub use moso_core::response::{Created, Empty, NoContent, Page};
    pub use moso_core::openapi::{OperationBuilder, ResponseSpec, SecurityScheme};
    pub use moso_core::schema::{Cursor, Email, Id, Schema, Slug};
    pub use moso_core::config::{Config, SecretString};
    pub use moso_macros::*;
}
```

**Rule (held):** the prelude MUST NOT exceed 40 items, and every item in it MUST be needed by the
tutorial app. Current count: 28 named items + 10 macros = 38.

The macro re-export is a **glob**, `pub use moso_macros::*`, in both `lib.rs` and `prelude.rs`. That
was a build-order concession — the facade had to compile before the macro crate landed — and it is
worth revisiting now that both exist: an explicit list documents itself and would make an
accidentally-added macro a deliberate decision. Tracked in `63-implementation-status.md`.

**The `__private` rule.** Macro output names `::moso::__private::X` and nothing else — never
`::moso_core::X`, never `::moso_schema::X`. It re-exports every trait, type, helper and substrate
crate a generated body needs (including `serde`, so an application can `#[derive(Schema)]` without
adding `serde` to its own manifest, and cannot end up with a *different* `serde`). A `#[cfg(test)]`
module in `moso/src/lib.rs` names every path a macro emits, so a missing re-export fails there
instead of in user code.

**Deps:** `moso-core` (unconditional), `moso-macros`, `moso-openapi` (optional, feature `openapi`).
**WP:** WP-01.

---

### `moso-core` — the runtime core ✅
**Purpose:** everything that is true of every Moso app.

Public surface headline (all present):
- `App`, `AppBuilder`, `AppState`, `Resolver`, `Lifespan`
- `Router`, `MethodRouter`, `Route`, `RouteEntry`, `RouteInfo`, `StaticSource`
- `Handler`, `HandlerAdapter`, `Endpoint`, `HandlerFn`, `ErasedHandler`, `UndocumentedEndpoint`
- `Extract`, `ExtractBody`, `Describe`, `Opaque`/`OpaqueBody`, `MosoExt`/`MosoExtBody`
- `IntoResponse` (re-export of Axum's) and the response set
- `Error`, `ErrorKind`, `Result`, `Problem`, `BootError`, `BootErrors`
- `ProviderReq`, `ProviderMap`, `Inject`, `Dependency`, `Depends`, `RequestCtx`
- `Config`, `ConfigSource`, `ConfigLoader`, `Profile`, `SecretString`, `Reloadable`
- `MiddlewareStack`, `Slot`, `Next`, `Guard`, `CustomLayer`
- `HealthCheck`, `HealthStatus`, `task::blocking`, `shutdown::Signal`, `Drain`

**Deps:** `axum`, `tower`, `tower-http`, `hyper`, `hyper-util`, `tokio`, `http`, `http-body(-util)`,
`bytes`, `mime`, `cookie`, `serde`, `serde_json`, `serde_urlencoded`, `serde_path_to_error`,
`form_urlencoded`, `percent-encoding`, `toml`, `futures-util`, `pin-project-lite`, `indexmap`,
`arc-swap`, `zeroize`, `dotenvy`, `humantime`, `ulid`, `tracing`, `thiserror`,
**`moso-schema` and `moso-openapi` — both unconditional.**

**Features:** `openapi` (default), `tracing` (default), `compression`, `cors`, `multipart`, `ws`.

> **Decision D1 — `moso-openapi` is a non-optional dependency of `moso-core`.**
> The original design made it optional. That would make `Extract::describe(&mut OperationBuilder)`,
> `Endpoint::spec`, `Describe`, `Guard::describe` and `Dependency::describe` — i.e. five public trait
> signatures — feature-dependent, so a crate compiled without the feature would present a *different
> trait* to the same name. Every downstream extractor would need `#[cfg]` on its impl. Not worth it.
> The `openapi` cargo feature now controls exactly one thing: **whether the `/docs` and
> `/openapi.json` routes are mounted.** The document is assembled either way, so
> `moso openapi export` works in every build. The runtime half of the same decision is
> `http.expose_docs`.

**Constraint:** `moso-core` MUST NOT depend on any database crate. ✅ (There is no database crate in
the workspace at all.) The "compiles in under 15 s from cold on 8 cores" budget is **not measured** —
there is no `xtask bench-compile`.

**WP:** WP-02, WP-03, WP-06, WP-08, WP-09, WP-10.

---

### `moso-macros` — HTTP-layer proc macros ✅

Shipped: `#[endpoint]`, `routes!`, `ep!`, `#[middleware]`, and the derives `Schema`, `Constrained`,
`Responder`, `Dependency`, `Config`, `Error`.

Not shipped (they belong to crates that do not exist): `permissions!`, `sql!`.

`ep!` is new since the original layout and is not optional: Rust cannot attach an associated type to
a `fn` item, so `#[endpoint]` emits a companion type and `routes!`/`ep!` are how it is named. See
[ADR-0013](../adr/0013-handler-registration.md).

**Deps:** `syn 2`, `quote`, `proc-macro2`, `darling`, `heck`, `regex` (to validate
`#[schema(pattern = …)]` at expansion time rather than at first request).
**Rule (held):** proc-macro crates MUST NOT depend on any runtime Moso crate. Generated code refers
to `::moso::__private::*`.
**WP:** WP-04, WP-05, WP-07.

---

### `moso-schema` — the model layer ✅
**Purpose:** the "one model, three jobs" trait set, independent of HTTP.

```rust
// as built
pub trait Schema: Serialize + DeserializeOwned + Validate + Send + Sync + 'static {
    fn schema_name() -> Cow<'static, str>;
    fn json_schema(generator: &mut SchemaGenerator) -> SchemaNode;
    fn schema_ref() -> SchemaRef { SchemaRef::inline_or_named(Self::schema_name()) }
    const HAS_CONSTRAINTS: bool = false;
}

pub trait Validate {
    fn validate(&self, ctx: &mut ValidationCtx) -> Result<(), ValidationErrors>;
}
```

The parameter is `generator`, not `gen`: **`gen` is a reserved keyword in edition 2024.** The return
type is `SchemaNode`, not `JsonSchema`.

> **Decision D2 — the JSON Schema model lives in `moso-schema`, not `moso-openapi`.**
> `moso_schema::json_schema` owns `SchemaNode`, `SchemaGenerator`, `SchemaRef`, `ObjectBuilder`,
> `StringBuilder`, `NumberBuilder`, `ArrayBuilder`, `TypeSet`, `JsonType`, `AdditionalProperties`
> and `Discriminator`. `moso-openapi` **depends on** `moso-schema` and embeds generated nodes into
> `components/schemas`.
> The reason is the dependency direction: `Schema::json_schema` must name the generator, so if the
> generator lived in `moso-openapi` then `moso-schema` would depend on `moso-openapi` and the claim
> "`moso-schema` is usable standalone, with no HTTP and no OpenAPI" would be false. It is a JSON
> Schema 2020-12 model that OpenAPI happens to embed, not an OpenAPI construct.
> `SchemaGenerator::new(ref_prefix: &'static str)`; the default prefix is `"#/components/schemas/"`
> (`moso_schema::json_schema::DEFAULT_REF_PREFIX`, re-exported as
> `moso::COMPONENTS_SCHEMAS_PREFIX`).

Also shipped: the constrained newtypes (`Email`, `Url`, `Slug`, `Password`, `PhoneE164`, `Hostname`,
`IpCidr`, `Trimmed`, `Sanitised<P>`, `Cursor`, `NonEmpty<T>`, `Bounded<T, MIN, MAX>`,
`Length<T, MIN, MAX>`, `Id<E>`), `ValidationErrors`/`FieldError` with JSON-Pointer paths, the closed
`codes` set, `MessageProvider`, the `checks::*` helpers, and `Schema` impls for the primitives,
`Option`, `Vec`, `HashMap`/`BTreeMap`, tuples, `chrono`, `uuid` and `url`.

**Deps:** `serde`, `serde_json`, `indexmap`, `smallvec`, `once_cell`, `regex`,
`unicode-segmentation`, `chrono`, `uuid`, `url`, `email_address`. **All unconditional** — the
original design made the model-type dependencies optional, and the feature matrix that would have
produced was not worth the ~6 crates it saves. `rust_decimal` and `time` are not supported.

**Rule (held):** `moso-schema` MUST NOT depend on `http`, `axum` or `sqlx`. ✅
**WP:** WP-05.

---

### `moso-openapi` — document generation ✅

OpenAPI 3.1 data model (`Document`, `PathItem`, `Operation`, `Components`, `Param`, `ResponseSpec`,
`SecurityScheme`), the `OperationBuilder`/`DocumentBuilder` pair that `Endpoint::spec` writes into,
the document differ (`diff.rs`, incl. breaking-change classification), and the doc UI.

It does **not** own a `SchemaGenerator` (D2). `OperationBuilder` owns one *by value* for the
duration of one operation, and `DocumentBuilder` lends it out and takes it back; that is what keeps
`fn describe(op: &mut OperationBuilder)` free of a lifetime parameter, which in turn keeps it out of
every trait signature and every macro expansion.

**The doc UI is Moso's own, not a vendored Scalar/ReDoc/Swagger bundle.** `ui.rs` is a single
self-contained HTML document with inlined CSS and vanilla JS whose only network request is a
same-origin `fetch` of the spec URL. The `scalar` / `redoc` / `swagger-ui` cargo features select
which route `moso-core` mounts; **all three render the same UI.** Vendoring three real bundles would
add megabytes of third-party JavaScript to every Moso binary, and shipping one renderer we control
is the only version of "works air-gapped" we can actually keep. Two unit tests hold the line: one
asserts the template contains no absolute URL, no `<script src>`, no external stylesheet and no
`@import`; the other checks the rendered document is balanced.

**Deps:** `serde`, `serde_json`, `indexmap`, `moso-schema`.
**Features:** `scalar` (default), `redoc`, `swagger-ui`.
**WP:** WP-07.

---

### `moso-test` ✅ (reduced scope)

`TestApp`/`TestAppBuilder`, `TestClient` with typed helpers and rich failure output, `TestResponse`,
`TestClock`, `LogAssertions`, `RequestRecord`, the JSON `diff`, and `contract` assertions against the
generated OpenAPI document. The `server` feature (default) additionally binds a real ephemeral port
and drives the app over a socket with `reqwest`; without it the harness calls the composed
`tower::Service` in process.

Absent, because the batteries they assert against do not exist: per-test database via Postgres
template databases, `#[derive(Factory)]`, job draining, mail assertions, `assert_queries!`. They are
**not stubbed**.

**WP:** WP-21.

---

### `moso-ui-tests` ✅ (new)

Not in the original layout. A `trybuild` corpus of programs that must *fail* to compile, with the
diagnostic snapshotted. It is a workspace member rather than a `tests/ui/` directory because
`trybuild` needs a real package with its own dependency on `moso`.

---

### `moso-cli` ⚠️ (reduced scope)

The `moso` binary. Shipped subcommands: `new`, `openapi export`, `openapi check`, `routes`,
`doctor`, `config`, `self completions`. Templates are embedded with `include_str!`.

Not shipped: `dev`, `run`, `check`, `generate`, `db`, `middleware`, `client`, `jobs`, `authz`,
`task`, `test`, `build`, `deploy`, `self update`. `04-devex/40-cli.md` records which and why.

`moso routes` and `moso openapi` work by running the application binary with `--dump-routes` /
`--dump-openapi` and reading what it answers — a consequence of ADR-0004: the route table is
ordinary Rust and cannot be read without running it. `moso new` writes `src/dump.rs` into every
generated project for exactly this reason.

**WP:** WP-23 (partial).

---

## ⛔ Not in this build

The following crates are specified for later milestones. Nothing in the current workspace depends on
them, references them, or stubs them.

<details>
<summary><code>moso-sql</code> — the sealed query facade (WP-11)</summary>

**Purpose:** the risk-containment crate. All SQL construction goes through here. The public API is
Moso's own types; the implementation initially delegates to `sea-query`.

```rust
// spec — the entire public surface is Moso-owned
pub struct Select { /* opaque */ }
pub struct Insert { /* opaque */ }
pub struct Update { /* opaque */ }
pub struct Delete { /* opaque */ }
pub enum Expr { /* opaque */ }
pub struct Sql { pub text: String, pub args: Vec<Value> }
pub trait Dialect { fn build(&self, stmt: &Statement) -> Sql; }
```

**No `sea-query` type appears in any public signature anywhere in Moso.** Enforced by a CI check
(`xtask check-sealed`) that greps the generated rustdoc JSON for foreign paths. See ADR-0005.
</details>

<details>
<summary><code>moso-orm</code>, <code>moso-orm-macros</code>, <code>moso-migrate</code> (WP-12 – WP-15)</summary>

`Db`, `Entity`, `Column`, `Select<E>`, `Relation`, `Related<T>`, `Transaction`, `Loaded`. Execution
via `sqlx`; construction via `moso-sql`. `#[derive(Entity)]`, `#[derive(Projection)]`,
`#[derive(Embedded)]`, `#[derive(Enum)]`, and the **schema descriptor** that `moso-migrate` diffs.
Snapshot (`migrations/.schema.json`), differ, migration generation, forward/backward runner,
advisory-lock-guarded, with a destructive-change policy.
</details>

<details>
<summary><code>moso-kv</code> (WP-16)</summary>

```rust
#[async_trait]
pub trait KvStore: Send + Sync + 'static {
    async fn get_raw(&self, key: &Key) -> Result<Option<Bytes>>;
    async fn set_raw(&self, key: &Key, val: Bytes, ttl: Option<Duration>) -> Result<()>;
    async fn delete(&self, key: &Key) -> Result<bool>;
    async fn incr(&self, key: &Key, by: i64, ttl: Option<Duration>) -> Result<i64>;
    async fn compare_and_swap(&self, key: &Key, old: Option<Bytes>, new: Bytes) -> Result<bool>;
    async fn scan(&self, prefix: &Key, cursor: Cursor) -> Result<(Vec<Key>, Cursor)>;
    fn capabilities(&self) -> Capabilities;
}
```
Backends: `memory`, `redis` (via `fred`), `postgres`.
</details>

<details>
<summary><code>moso-auth</code>, <code>moso-authz</code>, <code>moso-jobs</code>, <code>moso-mail</code>, <code>moso-storage</code>, <code>moso-admin</code> (WP-17 – WP-22)</summary>

Sessions, passwords, JWT, OAuth2, passkeys, API keys, `CurrentUser<U>`; the typed permission
registry, `Policy<Action, Resource>`, `Authorized<A, R>`, `#[requires(..)]`; `#[job]`, queues,
scheduler, workers; `Mailer` and `Storage` traits; the auto-generated CRUD admin. All specified in
`docs/03-batteries/`, none built.
</details>

---

## Dependency rules

Enforced in CI by `xtask check-deps` in the original plan. **There is no `xtask` in this build**, so
these are currently reviewed by hand. Status of each:

1. ✅ `moso-macros` MUST NOT depend on any runtime Moso crate. (Its manifest carries a comment
   saying so.)
2. ✅ `moso-core` MUST NOT depend on any battery crate. (Vacuously — there are none.)
3. ✅ `moso-schema` MUST NOT depend on `http`, `axum` or `sqlx`.
4. ✅ No Moso crate may depend on `moso` (the facade) — **except `moso-test`, deliberately.** The
   harness's job is to drive a user's application, which depends on the facade, and going through
   `moso-core` instead would let the harness and the application disagree about feature resolution.
   The rule is amended to "no crate the facade depends on may depend on the facade", which
   `moso-test` satisfies (the facade does not depend on it).
5. n/a Batteries MUST NOT depend on each other except along declared edges — no batteries exist.
6. ⛔ **Measured, and over budget.** Total third-party dependency count for `moso` with `default`
   features: ≤ 90 crates; with `full`: ≤ 260. `xtask check-deps` now measures the first half from
   `cargo tree -p moso --edges normal`, and on 2026-07-30 it reported **155 crates against a budget
   of 90**, so this rule fails the gate. The earlier claim that the graph was "well inside the
   budget by inspection" was wrong, which is the argument for measuring it. The `full` half still
   has nothing to measure, because the facade has no `full` feature.

   The four groups worth naming, because each is one decision rather than many: the HTTP substrate
   (`axum`, `hyper`, `tower`, `tower-http` and their `h2`/`futures-*`/`tokio` closure); the `cookie`
   crate's AEAD stack for signed and private cookies (`aes-gcm`, `chacha20`, `ghash`, `polyval`,
   `hkdf`, `hmac`, `sha2` and the `crypto-common`/`generic-array`/`typenum` layer beneath them);
   `url`'s IDNA support (`idna` → `idna_adapter` → seven `icu_*` crates → `zerovec`, `yoke`,
   `zerofrom`, `tinystr`, `litemap`, `writeable`, `potential_utf`); and `darling` + `syn` +
   `proc-macro2` for the derives. Closing the gap is a design decision — feature-gate the cookie
   AEAD, take IDNA off the default path, or raise the number and say why — not a tidy-up, and it
   needs an owner before the batteries land and make it worse.

## Versioning

All Moso crates version in lockstep and carry `=x.y.z` path+version pins on each other, declared once
in `[workspace.dependencies]`. Members refer to them with `dep.workspace = true` and may only *add*
features on top; this is how the workspace avoids version skew. (`xtask release` does not exist yet.)

Third-party crates that appear in Moso's **public API** (`axum`, `tower`, `http`, `serde`, `tokio`,
plus `bytes`, `serde_json`, `tower-http` and `tracing`) are re-exported under `moso::deps::*` and
their major versions are part of Moso's semver contract. Bumping any of them is a Moso breaking
change. This is documented prominently, because the axum 0.6→0.7 hyper-1.0 forced-upgrade is a live
memory for our target users.

One dependency is declared outside `[workspace.dependencies]`: `tracing-core`, in
`moso-test/Cargo.toml`, because `tracing` does not re-export `tracing_core::span::Current` and
`Subscriber::current_span` must return it. It is already in the tree, so it costs no extra
compilation, but it is an exception to the single-declaration rule and should be promoted.
