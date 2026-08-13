# 11 - Routing, `#[endpoint]`, `routes!` and `ep!`

> **Status: implemented.** This document describes the shipped API. Where it once described a
> registration model Rust cannot express, see [ADR-0013](../adr/0013-handler-registration.md).

## The problem being solved

In Axum you write the handler, then you write `#[utoipa::path(get, path = "/users", tag = "users",
params(...), responses((status = 200, body = Vec<UserOut>), (status = 422, body = ...)))]`, then you
write `.route("/users", get(list))`. Three declarations of the same fact, kept in sync by hand.
FastAPI has one. Moso has one and a half: the route registration, plus a bare attribute that exists
only because Rust has no runtime introspection.

## Public API

```rust
// spec - moso-core/src/router.rs

pub struct Router { /* opaque - NOT generic over state */ }

impl Router {
    pub fn new() -> Self;

    // method shorthands - the common case
    pub fn get<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn post<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn put<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn patch<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn delete<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn head<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn options<H, M>(self, path: &'static str, handler: H) -> Self where H: Handler<M>, M: 'static;

    /// The method-as-a-value form the shorthands lower to.
    pub fn method<H, M>(self, method: HttpMethod, path: &'static str, handler: H) -> Self
        where H: Handler<M>, M: 'static;

    /// The explicit generic form `routes!` and `ep!` lower to.
    pub fn endpoint<E>(self, method: HttpMethod, path: &'static str) -> Self
        where E: Endpoint + HandlerFn + Clone + Default;

    /// Multiple methods on one path.
    pub fn route(self, path: &'static str, methods: MethodRouter) -> Self;

    /// Nest a sub-router under a prefix. Paths compose; OpenAPI merges.
    pub fn nest(self, prefix: &'static str, router: Router) -> Self;
    /// Merge a sub-router at the same level.
    pub fn merge(self, router: Router) -> Self;

    /// Serve static files from a directory or an embedded bundle.
    pub fn static_files(self, path: &'static str, source: StaticSource) -> Self;

    // ── OpenAPI metadata applied to everything registered so far ───────────
    pub fn tag(self, tag: &'static str) -> Self;
    pub fn security(self, scheme: SecurityRequirement) -> Self;
    pub fn deprecated(self) -> Self;
    /// Drop everything registered so far from the document, while still serving it.
    pub fn hidden(self) -> Self;
    /// Additional response documented on every route (e.g. a 429 from a rate limiter).
    pub fn responds(self, status: u16, spec: ResponseSpec) -> Self;

    // ── middleware ─────────────────────────────────────────────────────────
    /// Apply to routes registered *before* this call (Tower ordering, made explicit in docs).
    pub fn layer<L>(self, layer: L) -> Self
        where L: tower::Layer<Route> + Clone + Send + Sync + 'static,
              L::Service: tower::Service<Request, Error = Infallible> + Clone + Send + Sync + 'static,
              <L::Service as Service<Request>>::Response: IntoResponse + 'static,
              <L::Service as Service<Request>>::Future: Send + 'static;
    /// Guard: reject before the handler runs; contributes a documented error response.
    pub fn guard<G: Guard>(self, guard: G) -> Self;
    /// Per-route timeout, applied to the routes registered so far.
    pub fn timeout(self, timeout: Duration) -> Self;

    // ── fallbacks ──────────────────────────────────────────────────────────
    pub fn fallback<H, M>(self, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn method_not_allowed<H, M>(self, handler: H) -> Self where H: Handler<M>, M: 'static;

    // ── escape hatches ─────────────────────────────────────────────────────
    pub fn mount_axum(self, prefix: &'static str, router: axum::Router<()>) -> Self;
    pub fn into_axum(self) -> axum::Router<()>;

    // ── introspection (`moso routes`, boot diagnostics, tests) ──────────────
    pub fn entries(&self) -> &[RouteEntry];
    pub fn into_entries(self) -> Vec<RouteEntry>;
    pub fn metadata(&self) -> &RouteMetadata;
    pub fn describe(&self) -> Vec<RouteInfo>;
    pub fn conflicts(&self) -> Vec<BootError>;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
    pub fn fallback_handler(&self) -> Option<&BoxedHandler>;
    pub fn method_not_allowed_handler(&self) -> Option<&BoxedHandler>;
    pub fn axum_mounts(&self) -> &[(String, axum::Router<()>)];
    pub fn static_mounts(&self) -> &[(String, StaticSource)];
}
```

**`Router` is not generic.** No `Router<S>`, no `FromRef`. State comes from the provider map via
`Inject<T>`. This single decision removes the largest source of Axum trait-bound errors and the
largest source of monomorphisation in typical apps.

**Paths are `&'static str`, not `&str.`** That is what lets `validate_path` be a `const fn` and the
legacy-syntax check happen at compile time rather than at boot. A path computed at runtime is not
supported and is not wanted: a route table that varies per process cannot be documented.

**`layer` takes an ordinary `tower::Layer<Route>`** - the same bounds `axum::Router::layer` uses, so
anything that works there works here - and wraps it in `CustomLayer`, the erased form the stack
stores. `CustomLayer` is Moso's own trait (`name`, `apply(Route) -> Route`, `summary`) rather than an
alias, because `tower::Layer<S>` is generic over the service it wraps and so is not usefully
object-safe; erasing to one concrete service type is what lets the stack be a runtime-composable list
and what lets `moso middleware` name each entry.

`Route` is `tower::util::BoxCloneSyncService<Request, Response, Infallible>` - Moso's own alias,
because `axum::routing::Route::new` is `pub(crate)` and a Moso layer could never construct one.

## Path syntax

`/{param}` and `/{*rest}`, matching Axum 0.8+ and OpenAPI. `:param` and `*rest` are **rejected at
compile time** - the path is a `&'static str` checked by the `const fn`
`moso_core::router::validate_path`, which `routes!` reaches through the `route_path!` macro and
which `Router::get(..)` and friends re-check at registration:

```
error: legacy path parameter syntax
  --> src/routes/users.rs:8:18
   |
 8 |         .get("/users/:id", show)
   |                      ^^^ use `{id}` instead of `:id`
   |
   = note: Moso uses OpenAPI-style path parameters throughout
```

Path parameter names MUST match the field names of the `Path<T>` extractor's struct. Mismatch is a
**boot** error naming both sides (Axum gives you a runtime 500 or a silently-missing param).

## `#[endpoint]` - the one annotation

```rust
// example
/// Create a user.
///
/// Sends a welcome email asynchronously. Emails are unique; conflicts return 409.
#[endpoint]
async fn create(
    Inject(db): Inject<Db>,
    Depends(actor): Depends<CurrentUser>,
    Json(body): Json<CreateUser>,
) -> Result<Created<UserOut>> {
    /* ... */
}
```

From this the macro derives, with **no further annotation**:

| OpenAPI field | Source |
| --- | --- |
| `summary` | first line of the doc comment |
| `description` | rest of the doc comment (Markdown) |
| `operationId` | last module segment + fn name, e.g. `users_create`; bare fn name at the crate root |
| `parameters` | each `Path`/`Query`/`Headers`/`Cookie` extractor's `describe()` |
| `requestBody` | the single `ExtractBody` extractor's `describe()`, incl. content type |
| `responses[201]` | the `Ok` type's `describe()` (`Created<T>` sets 201 + `Location`) |
| `responses[422]` | contributed automatically by `Json<T>` when `T` has constraints |
| `responses[4xx/5xx]` | the declared error type's registered variants, via `errors = Type` (`16-errors.md`) |
| `security` | contributed by auth dependencies and by `Router::guard` |
| `deprecated` | `#[deprecated]` on the fn, or `#[endpoint(deprecated)]` |
| `x-source` | file:line, used by boot diagnostics and `moso routes` |

`responses[401/403]` are contributed by whatever `Depends<T>`/`Guard` implements them; nothing in
this build ships an auth battery, so there is no built-in contributor.

### Optional arguments (all rare, all escape hatches)

```rust
#[endpoint(
    operation_id = "users.create",     // override the derived id
    tag = "users",                     // usually set on the router instead
    hidden,                            // exclude from OpenAPI
    deprecated,                        // without deprecating the Rust item
    response(409, "Email already registered"),   // document an extra status
    example(request = "...json...", response = "...json..."),
    errors = ShopError,                // fold a `#[derive(moso::Error)]` taxonomy in
)]
```

Anything beyond this belongs in the types, not the attribute. If a user needs an attribute to
describe a response, that is a signal the response type is under-modelled - the docs say so.

### What `#[endpoint]` generates

Rust cannot attach an associated type to a `fn` item, so the metadata lives on a **companion unit
struct** the macro emits beside the untouched function. This is the single most consequential shape
decision in the routing layer; the reasoning and the rejected alternatives are in
[ADR-0013](../adr/0013-handler-registration.md). Full expansion in
`06-reference/62-macro-reference.md`; the summary:

```rust
// generated (abridged)
async fn create(/* unchanged */) -> Result<Created<UserOut>> { /* unchanged body */ }

/// The [`Endpoint`] generated for `create` by `#[endpoint]`.
#[doc(hidden)]
#[allow(non_camel_case_types, non_snake_case, unreachable_pub, dead_code)]
#[derive(Clone, Copy, Default)]
pub struct __moso_op_create;

impl ::moso::__private::Endpoint for __moso_op_create {
    const NAME: &'static str = "create";
    fn spec(__moso_b: &mut ::moso::__private::OperationBuilder) {
        __moso_b.summary("Create a user.");
        __moso_b.description("Sends a welcome email asynchronously. …");
        __moso_b.operation_id(/* module_path!() tail + "_create" */);
        __moso_b.source(::core::file!(), ::core::line!());
        // one call per parameter, in declaration order
        <Inject<Db>               as ::moso::__private::Extract>::describe(__moso_b);
        <Depends<CurrentUser>     as ::moso::__private::Extract>::describe(__moso_b);
        <Json<CreateUser>         as ::moso::__private::ExtractBody>::describe(__moso_b);
        <Result<Created<UserOut>> as ::moso::__private::Describe>::describe(__moso_b);
    }
    fn required_providers() -> &'static [::moso::__private::ProviderReq] {
        ::moso::__private::concat_reqs!(
            <Inject<Db>           as ::moso::__private::Extract>::PROVIDER_REQ,
            /* … one per parameter … */
        )
    }
}

impl ::moso::__private::HandlerFn for __moso_op_create { /* the extraction glue */ }

// diagnostics: these exist solely to make errors point at the user's code
#[allow(dead_code, non_snake_case)]
const _: () = {
    fn __moso_assert_extract<T: ::moso::__private::Extract>() {}
    fn __moso_assert_body<T: ::moso::__private::ExtractBody>() {}
    fn __moso_assert_describe<T: ::moso::__private::Describe>() {}
    fn __moso_assert_response<
        T: ::moso::__private::IntoResponse + ::moso::__private::Describe,
    >() {}
    fn __moso_check() {
        __moso_assert_extract::<Inject<Db>>();       // ← span points at the param, not into axum
        __moso_assert_extract::<Depends<CurrentUser>>();
        __moso_assert_body::<Json<CreateUser>>();
        __moso_assert_response::<Result<Created<UserOut>>>();
    }
};
```

The assertion block is what `#[axum::debug_handler]` does opt-in. Moso does it **always**, because
"the error is bad unless you remembered to add an attribute" is not a developer experience. Cost is
measured: see `04-devex/42-compile-times.md` (budget: ≤ 6% of endpoint compile time).

Three details of the generated struct are part of the contract, not incidental:

- It derives `Clone, Copy, Default` - `Handler: Clone`, and `Router::endpoint::<E>` needs `E: Default`.
- Every `#[cfg(..)]` on the handler is copied onto it, so `#[cfg(feature = "admin")]` does not leave
  a companion type referring to a function that was compiled out. `cfg_attr` is deliberately **not**
  copied.
- When the handler fails to expand, a *well-typed placeholder* companion type is still emitted, so a
  `routes!` table naming it produces no second, derived error.

### Body extractor must be last - enforced at macro time

The macro knows which parameter types are body extractors by a name heuristic over the outermost
path segment, and the trait bound in the assertion block is what actually enforces it. A body
extractor in a non-final position produces:

```
error: request body extractor must be the last parameter
  --> src/routes/users.rs:12:5
   |
11 |     Json(body): Json<CreateUser>,
   |                 ---------------- this extractor consumes the request body
12 |     Inject(db): Inject<Db>,
   |     ^^^^^^^^^^^^^^^^^^^^^^ ...so no parameter may follow it
   |
   = note: only one body extractor is allowed per handler
   = help: move `Json<CreateUser>` to the end of the parameter list
```

This is the single most-reported Axum papercut and Moso must make it impossible to hit blind. There
is deliberately **no** blanket `impl<T: Extract> ExtractBody for T` that would make the rule a
type-system property - it collides under coherence with the real body extractors. See
`12-extractors-responses.md § The traits`.

## `routes!` - the primary registration API

```rust
// example
pub fn router() -> Router {
    moso::routes! {
        GET    "/users"       => list,
        POST   "/users"       => create,
        GET    "/users/{id}"  => show,
        PATCH  "/users/{id}"  => update,
        DELETE "/users/{id}"  => destroy,
    }
    .tag("users")
}
```

Rules:

- Methods are `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, `TRACE`, and `ANY`.
  Case-insensitive. `ANY` is not an HTTP method: it registers the same endpoint under
  `GET PUT POST DELETE OPTIONS HEAD PATCH`, in that order, which is what a proxy, a webhook receiver
  or a legacy shim actually needs.
- A handler in another module keeps its path: `users::list`, `crate::routes::posts::publish`.
  Only the **last segment** is rewritten.
- A malformed table yields exactly one `compile_error!` plus `Router::new()` as a well-typed
  placeholder, so a trailing `.tag("users")` does not produce a second error.

It expands to the builder chain - literally, so the two cannot diverge:

```rust
::moso::__private::Router::new()
    .endpoint::<__moso_op_list>(
        ::moso::__private::HttpMethod::Get,
        ::moso::__private::route_path!("/users"),
    )
    .endpoint::<__moso_op_create>(
        ::moso::__private::HttpMethod::Post,
        ::moso::__private::route_path!("/users"),
    )
```

## `ep!` - one route, where a table would be noise

```rust
// example
Router::new()
    .get("/healthz", moso::ep!(healthz))
    .get("/users", moso::ep!(users::list))
```

`ep!(name)` is a one-token proc macro expanding to `__moso_op_name` (preserving any module path).
Because the companion type is a unit struct, its path is also an expression, so it reaches the same
`Handler<EndpointMarker>` impl `routes!` uses and produces the same document.

`ep!(GET "/healthz" => healthz)` is the predictable mistake and is answered specifically:

```
error: `ep!` takes a handler name, not a whole route
  = note: `ep!` names the type `#[endpoint]` generated; the path belongs to the router
  = help: write `Router::new().get("/healthz", ep!(healthz))`
  = help: for several routes use the table: `routes! { GET "/healthz" => healthz }`
```

## Registering a plain `async fn`

`Router::get("/users", list)` where `list` has **no** `#[endpoint]` compiles and serves. It resolves
through a different `Handler` family whose `type Endpoint = UndocumentedEndpoint`, which contributes
no summary, no parameters, no responses and no provider requirements, and writes
`x-moso-undocumented: true` into the operation so `moso routes` can print `<undocumented>`.

This is deliberate and it is honest: a plain `async fn` genuinely carries no metadata, and inventing
some would be worse than admitting it. What it costs is the OpenAPI operation and the boot-time
provider check. The fix is one line: add `#[endpoint]` and register it with `routes!` or `ep!`.

## Nesting, prefixes, and versioning

```rust
// example - src/routes/mod.rs
pub fn router() -> Router {
    Router::new()
        .merge(health::router())
        .nest("/api/v1", v1())
        .nest("/api/v2", v2())
}

fn v1() -> Router {
    Router::new()
        .merge(users::router())
        .merge(posts::router())
        .security(SecurityRequirement::bearer("jwt"))
        .responds(429, ResponseSpec::problem("Rate limited"))
}
```

Rules:
- Nesting composes paths **and** OpenAPI metadata (tags, security, extra responses) downward.
- `nest` on a path containing a parameter is allowed; the parameter is available to nested handlers.
- Conflicting routes are a **boot** error with both source locations (see `10-app-lifecycle.md`).
  `Router::conflicts()` computes them; `App::build()` reports them.
- Router-level metadata is overlaid **after** `Endpoint::spec`, and the merge rule is
  first-writer-wins, so a router can only ever *add* to what an endpoint said about itself.
- API versioning has no special machinery - `nest` plus separate modules is the whole story, and
  the docs say so explicitly to stop people building a versioning DSL.

## `MethodRouter`

```rust
// spec
pub fn get<H, M>(handler: H) -> MethodRouter where H: Handler<M>, M: 'static;
impl MethodRouter {
    pub fn on<H, M>(self, method: HttpMethod, handler: H) -> Self where H: Handler<M>, M: 'static;
    pub fn post<H, M>(self, handler: H) -> Self where H: Handler<M>, M: 'static;
    /* ... put, patch, delete ... */
    pub fn methods(&self) -> Vec<HttpMethod>;
}

// example
router.route("/users", get(ep!(list)).post(ep!(create)))
```

Provided for Axum familiarity. `routes!` is documented as preferred.

## The `Handler` trait

```rust
// spec - moso-core/src/handler.rs
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a valid Moso handler",
    label = "not a handler",
    note = "a handler must be an `async fn` whose parameters all implement `Extract` \
            (or `ExtractBody`, at most one, last) and whose return type implements `IntoResponse`",
    /* … */
)]
pub trait Handler<M>: Clone + Send + Sync + 'static {
    /// Compile-time description, supplied by `#[endpoint]`.
    type Endpoint: Endpoint;
    fn call(self, req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
}
```

`M` is the arity/marker type parameter. There are **three** non-overlapping families of impls:

| `M` | Implementor | `type Endpoint` |
| --- | --- | --- |
| `EndpointMarker` | any `E: Endpoint + HandlerFn + Clone` - i.e. every `__moso_op_*` | `Self` |
| `(PartsOnly, T1..Tn)`, n = 0..=16 | a plain `async fn` whose params are all `Extract` | `UndocumentedEndpoint` |
| `(WithBody, T1..Tn, TB)`, n = 0..=15 | a plain `async fn` ending in an `ExtractBody` | `UndocumentedEndpoint` |

All 34 of them (1 + 17 + 16) carry `#[diagnostic::do_not_recommend]`, so a handler that fails to
compile gets the hand-written message rather than a list of candidate impls.

Handler-taking methods bound `M: 'static`, not `M: Send + Sync + 'static`: `HandlerAdapter` holds
`PhantomData<fn() -> M>`, so `M`'s auto traits are irrelevant, and requiring them would have
rejected `Handler<(WithBody, RawBody)>` - `axum::body::Body` is not `Sync`.

Beyond 16 parameters the message is:

```
note: handlers support at most 16 parameters; group related parameters into a struct
      deriving `Dependency` or `Schema`
```

The bound is a named constant, `moso_core::handler::MAX_HANDLER_PARAMS`, mirrored in the macro crate
(which may not depend on a runtime crate) and checked against it by a unit test.

## The `Endpoint` and `HandlerFn` traits

```rust
// spec
pub trait Endpoint: 'static {
    const NAME: &'static str;                                  // the fn name, not the operationId
    fn spec(b: &mut OperationBuilder);                         // runs once, at App::build()
    fn required_providers() -> &'static [ProviderReq];         // the boot-time DI check
}

pub trait HandlerFn: Send + Sync + 'static {
    fn invoke(req: Request, ctx: RequestCtx) -> BoxFuture<'static, Response>;
}
```

`HandlerFn::invoke` is **one concrete, non-generic async block** per handler. It is monomorphised
once however many times the handler is registered, which is rule A2 of the compile-time
architecture: erase early.

## Route introspection

`Router::describe()` yields `Vec<RouteInfo>`; `App::router_info()` exposes it after boot; the CLI
renders it:

```
$ moso routes
METHOD  PATH                     HANDLER               AUTH        TAGS    SOURCE
GET     /api/v1/users            users::list           session     users   src/routes/users.rs:14
POST    /api/v1/users            users::create         session     users   src/routes/users.rs:31
GET     /api/v1/users/{id}       users::show           session     users   src/routes/users.rs:47
GET     /healthz                 <builtin>             -           -       -
GET     /docs                    <builtin>             -           -       -

6 routes, 4 documented, 0 conflicts
```

`moso routes --json` feeds editor tooling. The command shells out to the application binary with
`--dump-routes` - the route table is ordinary Rust and cannot be read without running it (ADR-0004).
`moso openapi check` compares the exported document against the committed one.

## Acceptance criteria (WP-03, WP-04)

1. ✅ A handler with `Json` not last fails to compile with the exact message above. Asserted on the
   macro's token output in `moso-macros`; the `trybuild` corpus lives in `crates/moso-ui-tests`.
2. ✅ `/users/:id` fails to compile with the legacy-syntax message, via the `const fn`
   `validate_path`. Same two layers of coverage.
3. ✅ `Path<T>` field names not matching the route's parameters is a boot error naming both
   (`BootError::PathParameterMismatch`, driven by `extract::path::path_shape`).
4. ✅ Two routers mounted with conflicting paths produce one boot error listing both sources.
5. ✅ `routes!` and the builder chain produce byte-identical OpenAPI documents for the same routes -
   guaranteed structurally, because `routes!` *expands to* the builder chain.
6. ✅ `Router::into_axum()` round-trips: the same requests get the same responses.
7. ⛔ Not measured: registering 200 routes adds < 1 ms to boot and < 400 KB to the binary. There is
   no `examples/bench` in this build and no `xtask` harness to measure with. See
   `06-reference/63-implementation-status.md`.
