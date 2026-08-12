# 15 — Dependency Injection

> ✅ **Status: implemented.** `Inject`, `Depends`, `Dependency`, the frozen provider map,
> `provide_with` factories resolved as a DAG with cycle detection, request-scoped memoisation,
> `ProviderReq` and the boot-time graph check, and `#[derive(Dependency)]`.
> Two notes: `ProviderReq::type_name` is a **`fn() -> &'static str`**, not a `&'static str`, because
> `core::any::type_name` is not const-stable on the pinned toolchain and `ProviderReq::of` must be
> `const`. `RequestCtx::provider::<T>()` returns `Result<Arc<T>>` — fallible in the signature only,
> since boot proved the provider exists, which is what lets `Inject<T>` present it as infallible.
> ⛔ `moso-core` has no `test` cargo feature yet, so the provider-override table is reachable only
> from `moso-core`'s own tests; see `06-reference/63-implementation-status.md`.

## The two tiers, restated

FastAPI's `Depends()` covers everything from "the DB session" to "the current user." That
uniformity is pleasant but it makes every dependency a potential runtime failure and gives no
startup guarantee. Moso splits the concept:

| | `Inject<T>` (Provider) | `Depends<T>` (Dependency) |
| --- | --- | --- |
| Lifetime | application | one request |
| Registered | `App::provide(..)` | `impl Dependency for T` |
| Resolution | type-map lookup, `Arc` clone | async fn, memoised per request |
| Can fail | **no** — boot guaranteed it | yes → typed `Error` → HTTP status |
| Documents itself | no | yes (security schemes, 401/403) |
| Examples | `Db`, `Kv`, `Mailer`, `Config<AppConfig>`, `Storage` | `CurrentUser`, `Tenant`, `RequestTx`, `Locale` |

The payoff: **`Inject<T>` is infallible at the use site.** No `?`, no `.expect()`, no
`FromRef` trait error. If the provider is missing, the app did not boot.

## `Inject<T>`

```rust
// spec
pub struct Inject<T: 'static>(pub Arc<T>);

impl<T: Send + Sync + 'static> Extract for Inject<T> {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<T>()];
    fn describe(_: &mut OperationBuilder) {}          // invisible in the API contract
    async fn extract(_: &mut Parts, ctx: &RequestCtx) -> Result<Self> {
        Ok(Inject(ctx.provider::<T>()))               // infallible by construction
    }
}

impl<T> Deref for Inject<T> { type Target = T; }
```

```rust
// example
#[endpoint]
async fn list(Inject(db): Inject<Db>) -> Result<Page<UserOut>> { /* db is a &Db via Deref */ }
```

### Registering providers

```rust
// example
App::new(cfg)
    .provide(db)                                       // concrete value
    .provide_with(|r| async move {                     // fallible, may read other providers
        let cfg = r.config::<AppConfig>();
        SearchClient::connect(&cfg.search).await
    })
    .provide_dyn::<dyn Mailer>(Arc::new(SmtpMailer::new(&cfg.mail)?))   // trait object
```

`provide_with` closures form a DAG resolved by demand at boot. A cycle produces:

```
error: provider cycle detected
  SearchClient → Db → SearchClient
  fix: break the cycle by constructing one of these eagerly, or introduce a lazy handle
```

### Trait-object providers

`provide_dyn::<dyn Mailer>` makes `Inject<dyn Mailer>` work, which is the key to testability:
`TestApp` swaps in a `CapturingMailer` without touching handler code. Every battery that has a
side effect (mail, storage, payments-in-user-code) is documented with this pattern.

## `Depends<T>`

```rust
// spec
pub trait Dependency: Clone + Send + Sync + 'static {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];
    fn describe(op: &mut OperationBuilder) {}
    fn resolve(ctx: &RequestCtx) -> impl Future<Output = Result<Self>> + Send;
}
```

Resolution is **memoised per request by `TypeId`**: two extractors and a piece of middleware all
asking for `CurrentUser` cause one database query. This is FastAPI's dependency cache, made
explicit.

### Derive for composition

```rust
// example
#[derive(Dependency, Clone)]
pub struct AdminUser(pub User);

impl Dependency for AdminUser {
    fn describe(op: &mut OperationBuilder) {
        CurrentUser::describe(op);
        op.response(403, ResponseSpec::problem("Admin required"));
    }
    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        let CurrentUser(u) = ctx.depends::<CurrentUser>().await?;   // composes, and is cached
        if !u.is_admin { return Err(Error::forbidden("admin required")); }
        Ok(AdminUser(u))
    }
}
```

For the common "wrap and check" shape, the derive writes this for you:

```rust
// example — equivalent to the above
#[derive(Dependency, Clone)]
#[depends(from = CurrentUser, check = "is_admin", error = "admin required")]
pub struct AdminUser(pub User);
```

### Dependencies with parameters

FastAPI does this with closures returning dependencies. Rust does it with types:

```rust
// example — a dependency parameterised at the type level
pub struct RateLimited<const PER_MIN: u32>;

impl<const PER_MIN: u32> Dependency for RateLimited<PER_MIN> {
    fn describe(op: &mut OperationBuilder) {
        op.response(429, ResponseSpec::problem("Rate limit exceeded"));
        op.extension("x-rate-limit", json!({ "per_minute": PER_MIN }));
    }
    async fn resolve(ctx: &RequestCtx) -> Result<Self> { /* KV counter */ }
}

#[endpoint]
async fn send_otp(_: Depends<RateLimited<5>>, Json(b): Json<OtpRequest>) -> Result<Empty> { … }
```

This is strictly better than FastAPI's version: the limit is in the type, so it is in the docs, and
it costs nothing at runtime.

## Boot-time graph validation

Each `#[endpoint]` emits `required_providers()`, computed as the union of every parameter type's
`PROVIDER_REQ`. `App::build()` walks every registered operation and checks the provider map.

```rust
// spec
pub struct ProviderReq {
    pub type_id: fn() -> TypeId,
    pub type_name: &'static str,
    pub optional: bool,
}
```

Transitive requirements (a `Dependency` that itself uses `Inject<Db>`) are declared by the
dependency's own `PROVIDER_REQ` const, which the derive computes from its fields. Manual `impl`s
must declare it; forgetting means you lose the boot check but nothing breaks — `ctx.provider::<T>()`
then returns a clear runtime error naming the missing provider and telling you to add
`PROVIDER_REQ`. `moso check` warns about manual `Dependency` impls with an empty `PROVIDER_REQ`
that reference `ctx.provider`.

### The error, again (this is the one users will see most)

```
error: application failed to build (1 problem)

  ✗ missing provider: `shop::search::SearchClient`
      required by  GET /search              src/routes/search.rs:18
                   via dependency `SearchScope`   src/deps.rs:40
      registered providers:
                   shop::db::Db
                   shop::config::AppConfig
                   moso_kv::Kv
      fix          add to your builder in src/lib.rs:
                   .provide_with(|r| async move { SearchClient::connect(..).await })
```

## Request-scoped transactions (the killer use case)

```rust
// spec — moso-orm
/// A transaction opened lazily on first use and committed after a 2xx response,
/// rolled back on error or a >=400 response.
pub struct RequestTx(/* opaque */);
impl Dependency for RequestTx { … }
```

```rust
// example
#[endpoint]
async fn transfer(
    Depends(tx): Depends<RequestTx>,
    Json(body): Json<Transfer>,
) -> Result<Empty> {
    Account::debit(&tx, body.from, body.amount).await?;
    Account::credit(&tx, body.to, body.amount).await?;
    tx.enqueue(NotifyTransferJob { id: body.id }).await?;   // job commits with the tx
    Ok(Empty)                                               // commit happens here
}
```

Rules:
- Opt-in per handler; there is no implicit global transaction (that pattern causes long-held
  connections and surprising deadlocks, and we say so in the docs).
- Commit occurs **after** the handler returns and **before** the response is written, so a commit
  failure becomes a 500 rather than a lie to the client.
- Any error, or a status ≥ 400, rolls back.
- Nested `Depends<RequestTx>` in sub-dependencies gets the same transaction (memoisation).
- A `SELECT`-only handler that never touches `tx` never opens a transaction (laziness).

## Interaction with middleware

Middleware runs before extraction and cannot use `Depends`. Values a middleware computes are passed
via `Extension<T>`; a `Dependency` can read them:

```rust
// example
impl Dependency for Locale {
    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        if let Some(l) = ctx.extension::<Locale>() { return Ok(l.clone()); }
        Ok(Locale::from_accept_language(ctx.headers()))
    }
}
```

## Overriding dependencies in tests

Directly modelled on FastAPI's `dependency_overrides`, which is one of its best features:

```rust
// example — tests/users.rs
let app = shop::app().await?
    .override_dependency::<CurrentUser>(|_| async { Ok(CurrentUser(User::fixture_admin())) })
    .override_provider::<dyn Mailer>(Arc::new(CapturingMailer::default()))
    .spawn_test().await?;
```

`override_dependency` is compiled out of release builds (`#[cfg(any(test, feature = "test"))]`), so
there is no production footgun.

## Performance notes

- The provider map is a `HashMap<TypeId, Arc<dyn Any + Send + Sync>>` built once, wrapped in `Arc`,
  never mutated. Lookup is a hash + downcast, ~15 ns.
- The per-request dependency cache is a `SmallVec<[(TypeId, Box<dyn Any>); 4]>` — linear scan beats
  hashing at these sizes and avoids an allocation for the common 0–2 dependency case.
- Total DI overhead target: **< 200 ns per request** for a handler with 2 injects and 1 dependency.
  Benchmarked in `examples/bench`.

## Why not compile-time DI (Pavex-style)?

Pavex resolves the graph at compile time via a codegen step, yielding zero runtime lookup and
excellent diagnostics. We reject it for M1 on three grounds (ADR-0003):

1. It requires a non-standard build step (`pavex build`), which breaks `cargo build`,
   rust-analyzer, `cargo install`, and every CI template. That is a large Loop-1 tax.
2. The runtime cost we avoid is ~15 ns against a request that will spend 2 ms in Postgres.
3. Our boot-time validation captures ~90% of the diagnostic value at ~5% of the complexity.

We keep the door open: `ProviderReq` is deliberately const-evaluable, so a future `moso build`
could perform the same analysis ahead of time without changing user-facing API.

## Acceptance criteria (WP-08)

1. Handler using `Inject<T>` without a provider ⇒ boot error listing every route, with file:line.
2. `Depends<T>` resolved twice in one request ⇒ one execution (asserted with a counter).
3. `RequestTx` commits on 2xx, rolls back on error and on a 4xx return, and never opens a
   transaction when unused.
4. `override_dependency`/`override_provider` do not exist in a release build (symbol check).
5. DI overhead benchmark meets the 200 ns target.
6. A provider cycle produces the cycle error naming the full path.
