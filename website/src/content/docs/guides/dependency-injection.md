---
title: Dependency injection
description: Register application-lifetime values as providers, resolve request-scoped values as dependencies, and let the build step prove the graph is complete before you serve traffic.
order: 8
status: shipped
---

Moso splits dependency injection along lifetime. A value that lives as long as the process (a
database handle, a mailer, an HTTP client, your configuration) is a **provider**, registered once on
the `App` builder and read with `Inject<T>`. A value that exists only for one request (the current
user, the tenant, an open transaction) is a **dependency**, written as `impl Dependency for T` and
read with `Depends<T>`.

The split buys one guarantee: `Inject<T>` cannot fail where you use it. No `?`, no `.expect()`, no
`FromRef` to implement. `App::build()` walks every registered handler, collects the providers each
one declares, and refuses to hand you an application if one is missing. There is no `State<S>` and
no `Router<S>` generic parameter.

## The two tiers

| | `Inject<T>` (provider) | `Depends<T>` (dependency) |
| --- | --- | --- |
| Lifetime | application | one request |
| Registered | `App::provide(..)` | `impl Dependency for T` |
| Resolution | type-map lookup plus `Arc` clone | async fn, memoised per request |
| Can fail | no, boot proved it exists | yes, as a typed `Error` |
| Documents itself | no | yes: security schemes, 401/403 |
| Examples | `Db`, `Kv`, `Mailer`, `AppConfig` | `CurrentUser`, `Tenant`, `RequestTx` |

Both arrive in a handler as ordinary parameters, and both come from the prelude, along with
`Dependency`, `RequestCtx` and `ProviderReq`.

## The smallest working example

`Inject<T>` derefs to `T`, so method calls work directly. Destructuring it in the parameter pattern
gives you the `Arc<T>`, and `into_inner()` does the same after the fact.

```rust title="src/routes/users.rs"
use moso::prelude::*;

/// A user, as the API returns one.
#[derive(Schema)]
pub struct UserOut {
    /// Stable identifier.
    pub id: u64,
}

/// A database handle, registered once at boot.
#[derive(Default)]
pub struct Db;
impl Db {
    /// Every user, newest first.
    async fn all(&self) -> Vec<UserOut> { vec![UserOut { id: 1 }] }
}

/// List users.
#[endpoint]
async fn list(Inject(db): Inject<Db>) -> Result<Page<UserOut>> {
    // `db` is an `Arc<Db>`; `&*db` and method calls work through `Deref`.
    Ok(Page::new(db.all().await))
}
```

Register the value in your composition root, the one expression that builds the application:

```rust title="src/lib.rs"
App::new(config)
    .provide(Store::new())
    .provide(Metrics::default())
    .mount(routes::router().layer(ObserveLayer::new()))
    .health_check("store", StoreIsReachable)
    .build()
```

`App::new(config)` registers your configuration type as a provider too, so `Inject<AppConfig>` works
with no extra line, as does `ctx.config::<AppConfig>()`. See
[configuration](./configuration.md). Read whatever the builder needs out of the config value
*before* you pass it in, because it moves.

## Registering providers

| Method | Signature | Use it for |
| --- | --- | --- |
| `provide` | `provide<T: Send + Sync + 'static>(self, value: T) -> Self` | A value you construct on the spot. |
| `provide_arc` | `provide_arc<T: Send + Sync + 'static>(self, value: Arc<T>) -> Self` | A value you already share elsewhere. Two registrations can alias one allocation, and `Arc::ptr_eq` against the original is true. |
| `provide_dyn` | `provide_dyn<T: ?Sized + Send + Sync + 'static>(self, value: Arc<T>) -> Self` | A trait object. Handlers take `Inject<dyn Trait>` and never name the concrete type. |
| `provide_with` | `provide_with<T, F, Fut>(self, f: F) -> Self` where `F: FnOnce(Resolver) -> Fut` | Something built at boot, asynchronously and fallibly, from other providers. |

Registration is keyed by `TypeId` and is last-write-wins. Registering the same type twice keeps the
last one, which is the entire implementation of provider overrides in tests.

The framework registers three providers of its own before yours: `Signal` (the shutdown signal),
`Drain` (the in-flight request counter) and `BlockingPool`. Because they go in first, your
`.provide` of the same type wins. `secret_provider(..)` adds a fourth,
`Vec<Arc<dyn SecretProvider>>`, when you call it at least once.

### Trait objects

`provide_dyn` is the swap point for tests and the reason the map stores each value as an
`Arc<Arc<T>>`. Register the trait object, inject the trait object:

```rust title="src/lib.rs"
use moso::prelude::*;
use std::sync::Arc;

/// Anything that can send a message.
pub trait Mailer: Send + Sync + 'static {
    /// Send one.
    fn send(&self, to: &str, body: &str);
}

/// The production implementation.
pub struct SmtpMailer;
impl Mailer for SmtpMailer {
    fn send(&self, _to: &str, _body: &str) {}
}

/// Send a welcome message.
#[endpoint]
async fn welcome(Inject(mailer): Inject<dyn Mailer>) -> Result<moso::response::NoContent> {
    mailer.send("ada@example.com", "hello");
    Ok(moso::response::NoContent)
}

let app = App::new(AppConfig { smtp: "localhost:25".to_owned() })
    .provide_dyn::<dyn Mailer>(Arc::new(SmtpMailer))
    .mount(Router::new().post("/welcome", moso::ep!(welcome)));
```

> [!IMPORTANT]
> `provide_dyn::<dyn Mailer>(Arc::new(SmtpMailer))` registers `dyn Mailer` and nothing else.
> `Inject<SmtpMailer>` is still unregistered, and a handler asking for it is a boot error. Depending
> on the concrete type is exactly what the trait object exists to prevent.

### Providers that depend on providers

`provide_with` hands your factory a `Resolver` over the providers registered **before** it. Ordering
in the builder expression is load bearing: there is no lazy, demand-driven resolution, and the map
is rebuilt from the registrations so far each time a factory runs.

```rust title="src/lib.rs"
let app = App::new(config)
    .provide(Db)
    .provide_with(|resolver| async move {
        resolver.get::<Db>()?;          // registered above, so it resolves
        Ok(Search("connected"))
    })
    .mount(routes::router())
    .build()?;

// Outside a request, the same values come back through a `Resolver`.
assert_eq!(*app.resolver().get::<Search>()?, Search("connected"));
```

Get the order wrong and the boot report names both types and the edit:

```text
  x provider `Search` is built before `Db`, which it needs
      note         a `provide_with` factory can only read providers registered before it, and this one is registered first
                   `app::Search` asked for `app::Db`
      fix          swap the two registrations, so `Db` exists before the factory runs
                   .provide(/* a Db */)
                   .provide_with(|r| async move { /* build the Search */ })
```

Two factories that each read the other produce a `provider cycle` problem naming the path instead. A
factory that returns `Err` for its own reasons reports that error chain under `cause`, against its
own type. Those three cases are told apart by recording, per factory, which types it asked for and
did not find.

> [!WARNING]
> `App::build()` is synchronous and drives each `provide_with` factory with
> `tokio::task::block_in_place` on the ambient runtime, so a pool the factory opens stays bound to
> the runtime that will serve requests with it. That needs a **multi-threaded** runtime. Building
> outside a runtime, or under the default `#[tokio::test]` (which is current-thread), is a boot
> error naming the fix: use `#[tokio::main]`, `#[tokio::test(flavor = "multi_thread")]`, or build the
> value eagerly and pass it to `.provide`.

### Reading providers outside a request

`App::resolver()` gives you a `Resolver` for startup hooks, shutdown hooks, lifespan factories,
health checks and CLI tasks. It is the outside-a-request half of the model.

| Method | Returns | Notes |
| --- | --- | --- |
| `get::<T>()` | `Result<Arc<T>>` | A provider by concrete type. |
| `get_dyn::<dyn Trait>()` | `Result<Arc<T>>` | A trait-object provider. |
| `get_arc::<T>()` | `Result<Arc<T>>` | Identical to `get`. |
| `config::<C>()` | `Result<Arc<C>>` | The application's configuration. |
| `has::<T>()` | `bool` | Whether a provider is registered. |

Inside a hand-written extractor, guard or `Dependency`, use the request context instead:
`ctx.provider::<T>()` returns `Result<Arc<T>>` and `ctx.try_provider::<T>()` returns
`Option<Arc<T>>`. The raw map is reachable as `app.state().providers()`, a `ProviderMap` with `get`,
`contains`, `contains_req`, `len`, `is_empty` and `registered_names`.

## Request-scoped dependencies

A `Dependency` is a type with an async constructor over the request context.

```rust
pub trait Dependency: Clone + Send + Sync + 'static {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) { let _ = op; }

    fn resolve<'a>(ctx: &'a RequestCtx) -> impl Future<Output = Result<Self>> + Send + 'a;
}
```

Here is a real one. The parsing lives in a plain function so that every branch can be unit-tested
without building a `RequestCtx`, which is a pattern worth copying:

```rust title="src/auth.rs"
/// Who the request is acting as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Actor {
    /// The name posts are attributed to.
    pub name: String,
    /// Whether this caller may publish and may see every draft.
    pub editor: bool,
}

impl Dependency for Actor {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        Ok(Self::from_headers(ctx.headers()))
    }
}
```

Take `Depends<Actor>` in a handler and you have it. A failing `resolve` returns a typed `Error`,
which becomes a status and a [problem document](./errors.md) without your handler body running.

```rust title="src/routes/posts.rs"
#[endpoint(errors = BlogError)]
async fn list(
    Inject(store): Inject<Store>,
    Inject(config): Inject<AppConfig>,
    Depends(actor): Depends<Actor>,
    Query(query): Query<ListPosts>,
) -> Result<Page<PostOut>> {
    /* .. */
}
```

`Option<Depends<T>>` works too: `Option<T>` is itself an extractor that turns a failure into `None`
while forwarding the inner provider requirements unchanged. Note the consequence, which is easy to
misread: `Option<Inject<T>>` still declares a **required** provider. For a genuinely optional
provider you need `ProviderReq::optional_of::<T>()`, described below.

### What a dependency can read

`RequestCtx` is the whole surface: `headers()`, `method()`, `uri()`, `path()`, `version()`,
`matched_path()` (`/users/{id}` rather than `/users/42`), `path_params()`, `request_id()`,
`limits()`, `state()` and `shutdown()`. `ctx.extension::<T>()` clones a value a middleware inserted,
which is the documented handshake between a Tower layer and a dependency. `ctx.depends::<Other>()`
resolves another dependency, so dependencies compose and the per-request cache makes the composition
free.

A handler may take a bare `RequestCtx` parameter, but treat that as a last resort. `Path`, `Query`,
`Headers`, `Inject` and `Depends` say what a handler reads. A bare context does not.

### Documenting itself

`Dependency::describe(op)` runs for every handler that takes `Depends<T>`, so one dependency
contributes its security scheme and its 401 or 403 to every operation that uses it. `Inject`
deliberately contributes nothing: an injected pool is not part of your API contract. See
[OpenAPI](./openapi.md).

### Memoisation and single-flight

Resolution is memoised per request, keyed by `TypeId`. Two extractors, a guard and the handler all
asking for `CurrentUser` cost one `resolve`, because the guard and the handler share one
`RequestCtx` and therefore one cache.

```rust
/// Whoever asks for `CurrentUser` twice pays for it once.
#[endpoint]
async fn whoami(ctx: RequestCtx) -> Result<NoContent> {
    let first = ctx.depends::<CurrentUser>().await?;
    let second = ctx.depends::<CurrentUser>().await?;
    assert_eq!(first.id, second.id);   // one resolve, cached by `TypeId`
    Ok(NoContent)
}
```

Resolution is also single-flight. Each cache slot is a `tokio::sync::OnceCell`, so two futures that
await the same dependency type concurrently share one `resolve` rather than starting two database
queries. The mutex guarding the slot vector is only ever held long enough to find or create a cell,
never across an await point, because holding it there would let a second extractor on the same
thread deadlock the request.

A failed `resolve` is **not** memoised. The slot exists but holds nothing, so a retry inside the same
request runs `resolve` again. An error is not sticky for the rest of the request.

The cost of single-flight is one shape the framework cannot detect: a dependency whose `resolve`
awaits itself, directly or around a cycle, waits on a cell it is itself initialising and never
completes. That is indistinguishable from the legitimate concurrent case, so nothing tries to tell
them apart and the request timeout is the backstop. Provider cycles, which *can* be detected
cheaply, are rejected at boot instead.

## `#[derive(Dependency)]`

The derive writes two shapes.

**Composition.** Every field is itself a `Dependency`, resolved through the same per-request cache,
and `PROVIDER_REQ` becomes the union of the fields' requirements.

**Wrap and check.** `from` names the dependency you wrap, `check` names a predicate over it, and the
derive writes the resolve-then-check body plus the 403 documentation.

```rust
use moso::prelude::*;

/// Composition: every field is itself a dependency.
#[derive(Dependency, Clone)]
pub struct Editing {
    /// Who is editing.
    user: CurrentUser,
    /// Which tenant they are editing in.
    tenant: Tenant,
}

/// Wrap-and-check, the common case.
#[derive(Dependency, Clone)]
#[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
pub struct AdminUser(pub CurrentUser);
```

Taking `Depends<AdminUser>` in a handler signature is then the whole authorisation rule, and the 403
is in the OpenAPI document for that operation without another line.

### Container attributes

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `from` | path | none | The dependency this one wraps. Selects the wrap-and-check shape. A generic dependency is reachable quoted: `from = "Scoped<Foo>"`. |
| `check` | string | none | A predicate over the wrapped value. Requires `from`. |
| `error` | string | `"not permitted"` | The `detail` of a failed check. Requires `check`. |
| `status` | integer | `403` | The status a failed check produces. Requires `check`. |
| `unwrap` | bool | inferred | Whether to take `.0` of what `from` resolved. |
| `manual` | flag | absent | Emit no trait impl, only the requirement table. |

`unwrap` is inferred and rarely written: a wrapper whose field type differs from `from` unwraps, one
whose field type *is* `from` does not, and a unit struct unwraps.

`status` maps to an [error kind](./errors.md), so only these are legal: 400, 401,
403, 404, 405, 406, 409, 410, 412, 413, 414, 415, 416, 422, 423, 429, 500, 501, 502, 503, 504.
Anything else is a compile error naming the nearest supported status and listing the whole set.

The `check` string has three spellings:

| Written | Becomes |
| --- | --- |
| `"is_admin"` | `this.is_admin` (a field) |
| `"is_admin()"` | `this.is_admin()` (a zero-argument method) |
| anything else | verbatim, with `this` bound to the wrapped value, for example `"this.role == Role::Admin"` |

### Field attributes

| Key | Meaning |
| --- | --- |
| `default` | Fill the field with `Default::default()` instead of resolving it. Contributes nothing to `PROVIDER_REQ`. |
| `provider` | Read an `Arc<T>` from the provider map. The field type must be spelled `Arc<T>`. Adds `ProviderReq::of::<T>()` to `PROVIDER_REQ`. |

```rust
use moso::prelude::*;
use std::sync::Arc;

/// The user, plus the pool the rest of the handler will want anyway.
#[derive(Dependency, Clone)]
pub struct Session {
    /// Resolved through the per-request cache.
    user: CurrentUser,
    /// Read straight from the provider map; adds `ProviderReq::of::<Db>()`.
    #[depends(provider)]
    db: Arc<Db>,
    /// Not resolved at all.
    #[depends(default)]
    touched: bool,
}
```

### When the derive is not enough

`#[depends(manual)]` emits only the requirement table, as `Self::MOSO_PROVIDER_REQ`, and leaves the
trait impl to you. It exists instead of a "the macro writes everything except `resolve`" shape,
which would silently recurse forever whenever you forgot to write the inherent method, because
`Self::resolve(ctx)` would then resolve to the trait method the macro generated, and there is no
syntax for "the inherent one only".

```rust
#[derive(Dependency, Clone)]
#[depends(manual)]
pub struct AdminUser(pub User);

impl Dependency for AdminUser {
    const PROVIDER_REQ: &'static [ProviderReq] = Self::MOSO_PROVIDER_REQ;

    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        /* whatever the two generated shapes cannot express */
    }
}
```

The derive rejects enums and **generic types** outright, because `PROVIDER_REQ` is a `const` built
from the fields and a `const` cannot read the generic parameters of the item around it. A generic
dependency has to be a hand-written impl, and on stable Rust an associated constant cannot
concatenate two generic constant slices, so such an impl gets one requirement slot and has to choose
what to spend it on. Every other misuse (`check` without `from`, `provider` on a field that is not an
`Arc<T>`, `default` and `provider` together, an unknown key) is a compile error with a note and a
paste-able fix, and the derive still emits a placeholder impl so that one bad attribute does not
become a page of unsatisfied trait bounds at every use site.

## Providers in middleware and guards

A `#[middleware]` function may take `Inject<T>` parameters, as long as they come before `req` and
`next`. They are extracted through a `RequestCtx` recovered from the request extensions.

```rust title="src/middleware.rs"
#[moso::middleware]
pub async fn observe(
    Inject(metrics): Inject<Metrics>,
    req: Request,
    next: Next,
) -> Result<Response> {
    metrics.requests.fetch_add(1, Ordering::Relaxed);
    Ok(next.run(req).await)
}
```

`Depends<T>` is rejected there at macro expansion time, because middleware runs before extraction
and the request cache is empty:

```text
error: `Depends<CurrentUser>` cannot be used in middleware
         = note: middleware runs before extractors, so request dependencies are not yet available
         = help: read a middleware-inserted value with `req.extensions()`, or move this logic into
                 a `Dependency` impl and use it in the handler
```

Guards take no `Inject` parameters at all; a guard reads what it needs from the `RequestCtx` it is
handed. Neither guards nor middleware participate in boot validation, which is the next section's
main caveat. See [middleware](./middleware.md).

## Boot-time validation

`#[endpoint]` builds each handler's `required_providers()` at compile time by concatenating the
`PROVIDER_REQ` of every parameter type. `Depends<Editor>` forwards `Editor::PROVIDER_REQ`, which the
derive set from `Actor::PROVIDER_REQ`, so however deep the graph is, it is flattened into a
`&'static [ProviderReq]` before `main` runs. There is no registry, no linker section and no
inventory crate. See [routing](./routing.md) for how handlers are registered.

`App::build()` then runs six steps and collects **every** problem rather than stopping at the first:
configuration is resolved, providers are frozen (running each `provide_with` factory in order), the
router is composed, the DI graph is validated, the OpenAPI document is built, and the middleware
stack is composed.

Validation walks every route's requirements against the frozen map, skipping optional ones, and
groups misses by provider. Here is the exact report, asserted character for character by a test in
the repository:

```text
error: application failed to build (1 problem)

  x missing provider: `boot_report::Store`
      required by  GET /list  crates/moso/tests/boot_report.rs:LINE
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Store = /* construct it */;
                   App::new(config).provide(value)
```

Every route that wanted the same provider appears under one heading with its `file:line:column`,
because "`Db` is missing and here are the nine routes that wanted it" is one problem with nine lines
where the transpose is nine problems that all say the same thing. A handler that takes `Inject<Db>`
twice contributes one line, not two. A near-miss against a registered name adds a `did you mean`
line. Missing providers sort ahead of route conflicts and everything else, because a missing provider
is usually the root cause of what is under it.

`ProviderReq::optional_of::<T>()` declares a requirement the boot walk skips and which yields `None`
at runtime; `Cookies` uses it for `CookieKey`. `build_unchecked()` skips validation entirely, which
is what `moso openapi export --force` uses and what a test that wants to inspect a deliberately
broken application uses. Never call it from `main`.

### Keeping the check when you write the impl yourself

A hand-written `Extract`, `Dependency` or guard that reads a provider is only covered by the boot
check if it says so. Declaring the requirement is one const:

```rust title="src/extract/tenant.rs"
use moso::deps::http;
use moso::prelude::*;

impl Extract for TenantId {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<Db>()];

    fn describe(op: &mut OperationBuilder) { let _ = op; }

    async fn extract(parts: &mut http::request::Parts, ctx: &RequestCtx) -> Result<Self> {
        let db = ctx.provider::<Db>()?;   // declared above, so boot proved it exists
        TenantId::lookup(&db, parts).await
    }
}
```

### What boot validation does not cover

> [!WARNING]
> These three holes are real. Do not rely on the boot check to catch them.

- **A `#[middleware]` function's `Inject<T>` is not checked at boot.** The macro does generate a
  public `PROVIDER_REQ` on the layer (`ObserveLayer::PROVIDER_REQ`), and it is correct, but
  `Router::layer` erases the layer and validation reads only the route's endpoint requirements. A
  middleware that injects a provider nobody registered builds fine and returns 500 on the first
  request.
- **A `Guard` has no `PROVIDER_REQ` at all.** A guard that calls `ctx.provider::<T>()` or
  `ctx.config::<C>()` is invisible to validation. Reading the config is safe in practice, since
  `App::new` always registers it; anything else is a runtime 500.
- **A hand-written `Dependency` that omits `PROVIDER_REQ` loses the check.** This is by design, and
  the runtime error says so, but note that `RequestTx` in the ORM does exactly this: it reads `Db`
  from the provider map without declaring it, so a route taking `Depends<RequestTx>` without a `Db`
  provider fails at the first request rather than at boot. `Kv` and `Jobs` declare theirs correctly
  and are the model to copy.

When a requirement does escape the check, the runtime error is at least explicit about why:

```text
no provider is registered for `Db`.
This is only reachable when a hand-written `Extract` or `Dependency` impl reads a provider it did
not declare, or when the application was built with `AppBuilder::build_unchecked`.
help: declare it, so that `App::build()` reports this at boot instead of at 3am:
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<Db>()];
help: and register the value on the builder:
    .provide(/* a Db */)
```

`moso check` exists, but **it does not have this lint.** Its ten lints read the assembled router, the
generated document and a lexical scan of `src/`; an empty `PROVIDER_REQ` on a hand-written
`Dependency` impl is a fact about types, which needs a parse the CLI deliberately does not do. So
nothing lints them, and the rule stands on its own: declare everything the impl reaches transitively,
or the route drops out of the boot check and a boot error becomes a production 500.

## Testing

Swapping a provider is one line on the test harness, because registration is last-write-wins. The
test edits the real builder rather than assembling a second, simpler application:

```rust title="tests/greet.rs"
let app = TestApp::builder()
    .app(app())
    .override_provider(Greeting("overridden".to_owned()))
    .spawn()
    .await
    .expect("boots");

app.client()
    .get("/greet")
    .send()
    .await
    .assert_json_path("/username", "overridden");
```

For a trait object use `override_provider_dyn::<dyn Mailer>(Arc::new(CapturingMailer))`, which is
why a mailer, a clock or a payment gateway is worth registering as `dyn Trait` in the first place.
To pin your application's configuration so a stray environment variable cannot change what the tests
expect, register a second value of the same type: `override_provider(AppConfig { .. })`. Anything the
builder does not name goes through `customise(|app| ..)`. More in [testing](./testing.md).

Assertions about the per-request cache are available from a `RequestCtx`: `ctx.cache().len()` counts
the distinct dependency types asked for, and `ctx.cache().contains::<T>()` is true only once `T` has
resolved successfully. A failed or in-flight resolve reads as `false`.

### Substituting a dependency's `resolve`

`DependencyOverrides` is a FastAPI-style table that replaces a dependency's `resolve` with a fixture
closure. The table is itself an ordinary provider, so you install it with `.provide(overrides)` and
no new API is involved.

```rust title="tests/me.rs"
use moso::di::DependencyOverrides;
use moso::prelude::*;

#[tokio::test(flavor = "multi_thread")]
async fn an_override_replaces_the_resolve() -> Result<()> {
    let mut overrides = DependencyOverrides::new();
    // The closure still gets the `RequestCtx`, so a fixture can read the
    // request it is standing in for.
    overrides.insert::<CurrentUser, _, _>(|_ctx| async {
        Ok(CurrentUser { name: "fixture".to_owned() })
    });

    let app = App::new(AppConfig::default())
        .provide(overrides)                     // the table is an ordinary provider
        .mount(moso::routes! { GET "/me" => me })
        .build()?;

    // the response body is `"fixture"`, not `"real"`
    Ok(())
}
```

This needs the `test` cargo feature, which belongs in `[dev-dependencies]` and nowhere else:

```toml title="Cargo.toml"
[dev-dependencies]
moso = { version = "0.1", features = ["test"] }
```

Both the table and the lookup in `RequestCtx::depends` are compiled out without it, so a production
build cannot have a dependency silently replaced and the `depends` fast path carries nothing extra.

`TestAppBuilder::override_dependency` now ships: it replaces a dependency's `resolve` with a fixture
closure, so wiring the `DependencyOverrides` table by hand (as above) or through
`.customise(|app| app.provide(overrides))` is the lower-level form rather than the only one.

For a unit test that needs a context without a server, `AppState::for_tests()` plus
`RequestCtx::from_inner(RequestCtxInner { .. })` build one directly, both behind the same `test`
feature. Remember that providers are `Arc`-shared: a test that holds a `Resolver` or an `Arc<Store>`
keeps the value alive past the application that registered it, which matters when the value owns a
connection pool.

## Performance characteristics

`Inject<T>::extract` is a `TypeId` hash, a `downcast_ref` and an `Arc` clone. No allocation, no lock,
no copy of the value. Tests pin both halves of that claim: one proves the map hands back the
registered allocation rather than a copy, another proves the cost is exactly one strong-count
increment, released on drop.

Building the `RequestCtx` costs one `HeaderMap` clone, one `Extensions` clone, the matched-path
`Arc<str>`, the request id and a `Limits` copy, once per request. Provider values are never cloned;
the map is frozen at boot and never mutated again. At the end of the request the context and its
dependency cache are dropped and the provider map is untouched.

The design target is roughly 15 ns per lookup and under 200 ns of total DI overhead per request. The
first is bounded by a smoke test asserting under 5 microseconds per lookup, two orders of magnitude
looser so that a debug build on a loaded CI box still passes. **The 200 ns end-to-end figure is not
benchmarked.** Treat it as intent, not as a measurement.

The deliberate cost is one extra allocation per provider at boot and one extra pointer hop per
lookup, because each value is stored as `Arc<Arc<T>>`. That is what lets `T` be `dyn Mailer`:
`Arc::downcast` recovers the concrete type, so a singly-wrapped `Arc<SmtpMailer>` could never be read
back as `Arc<dyn Mailer>`. Storing the handle itself makes `provide` and `provide_dyn` one code path,
which is what makes swapping a capturing mailer into a test possible at all.

## Failure modes

| Symptom | Cause | Fix |
| --- | --- | --- |
| A boot error saying `missing provider` | A handler takes `Inject<T>` and nothing registered `T`. | Add `.provide(value)` to the composition root. |
| A boot error saying `is built before` | A `provide_with` factory reads a provider registered after it. | Swap the two registrations. |
| A boot error saying `provider cycle` | Two factories each read the other. | Break the cycle, usually by building one eagerly. |
| A boot error saying `provider failed` | A `provide_with` factory returned `Err`. | Read the `cause` line; it carries the factory's error chain. |
| A boot error about a multi-threaded runtime | `build()` ran outside a runtime or under the default `#[tokio::test]`. | Use `#[tokio::test(flavor = "multi_thread")]`, or `.provide` an eagerly built value. |
| A compile error saying `cannot be used in middleware` | `Depends<T>` in a `#[middleware]` signature. | Read a middleware-inserted value with `req.extensions()`, or move the logic into a `Dependency` used in the handler. |
| A compile error saying `cannot be used on a generic type` | `#[derive(Dependency)]` on a generic struct. | Write the `impl Dependency` by hand. |
| A 500 saying `no provider is registered` | A middleware, guard or hand-written impl read a provider it did not declare. | Declare `PROVIDER_REQ`, and register the value. |
| A request that hangs on a dependency | A `resolve` that awaits itself, directly or around a cycle. | Break the cycle. The request timeout is the only backstop. |

Two hard limits worth knowing: `MAX_HANDLER_PARAMS` is 16, counting extractors, `Inject`s and
`Depends` together, and a handler registered without `#[endpoint]` declares no requirements at all,
so the boot check skips it entirely.

## See also

- [Extractors](./extractors.md) for the `Extract` trait that `Inject` and `Depends` implement.
- [Middleware](./middleware.md) for `#[middleware]` and the `Inject` parameters it accepts.
- [Configuration](./configuration.md), because the type you pass to `App::new` is a provider.
- [Testing](./testing.md) for the harness, `override_provider` and the rest.
- [Transactions and pooling](./transactions.md) for `RequestTx`, the flagship dependency.
- [Permissions and roles](./permissions.md) for the largest real user of trait-object providers.
