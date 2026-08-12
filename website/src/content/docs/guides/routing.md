---
title: Routing
description: Map paths to handlers with the routes! macro, compose routers, and see what Moso checks at compile time and at boot.
order: 2
status: shipped
---

A Moso route is one line in a table. `#[endpoint]` turns an `async fn` into an operation that
carries its own OpenAPI description, `routes!` maps methods and paths onto those operations,
`Router` composes the tables, and `App` mounts the result. Nothing registers itself: every route
reaches the application through a statement someone wrote, so the whole route table is readable
without running anything.

What you get for that: path templates checked at compile time on your own literal, route conflicts
reported at boot naming both source locations rather than panicking inside `matchit`, and an API
document derived from the same signature that serves the request. `Router` has no state type
parameter, so the trait errors that state generic routers produce do not exist here. State is a
provider and you read it with `Inject<T>`, covered in
[dependency injection](./dependency-injection.md).

One thing on this page is not finished, and it is called out again where you would meet it.
Wildcard shadowing has a declared reason code but no check that produces it, so a catch-all
registered before a more specific route swallows it with nothing said at boot. Everything else here
is built and exercised by the framework's own tests, including `moso check`, which the compiler
notes on this page point at.

## The smallest application

This is `examples/minimal`. One handler, one route, one composition root.

```rust title="src/lib.rs"
use moso::AppBuilder;
use moso::prelude::*;

/// The body `GET /hello/{name}` returns.
#[derive(Schema)]
pub struct Greeting {
    /// Who was greeted.
    pub name: String,
    /// The greeting itself.
    pub message: String,
}

/// Greet someone by name.
#[endpoint]
async fn hello(
    Path(name): Path<String>,
    Inject(config): Inject<AppConfig>,
) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, {name}!", config.greeting),
        name,
    }))
}

/// The composition root: everything the application is, in one expression.
pub fn app() -> Result<AppBuilder> {
    Ok(App::new(AppConfig::load()?).mount(moso::routes! { GET "/hello/{name}" => hello }))
}
```

`AppConfig` is a `#[derive(Config)]` struct, elided here; `examples/minimal` has the file in full.
Returning the builder rather than a built `App` is deliberate: a test can override a provider before
`build()` runs the boot checks. See [testing](./testing.md).

Nothing on this page needs a Cargo feature. `Router`, `#[endpoint]`, `routes!` and `ep!` are in the
default build, and `moso::prelude::*` brings in `App`, `Router` and every macro, so
`routes! { .. }` without the `moso::` prefix works too. The examples in this repository write the
prefix because a route table is often read out of context. `Handler`, `Endpoint`, `HandlerFn`,
`MethodRouter`, `Route` and `Guard` are **not** in the prelude, which has a hard item budget; import
them from the crate root when you name one, as in `use moso::Guard;`.

## Declaring an operation with `#[endpoint]`

`#[endpoint]` re-emits your `async fn` byte for byte and adds a companion unit struct beside it,
named `__moso_op_<fn_name>`. The metadata lives on that type, because Rust has no way to attach an
associated type to a function item. You normally never type the name: `routes!` and `ep!` write it
for you.

From the signature and the doc comment, the macro derives:

- the `summary` from the first line of the doc comment, and the `description` from the rest, keeping
  Markdown and indentation;
- the `operationId` from the last module path segment plus the function name, so `users::create`
  becomes `users_create` (at the crate root it is just the function name);
- a `file:line` source location, pinned to the handler's own identifier, which is what the boot
  report and `moso routes` quote;
- one description per parameter, by calling that type's `Extract::describe` or
  `ExtractBody::describe`, and one for the return type through `HandlerReturn`;
- the list of providers the handler needs, transitively through its extractors, which is what the
  boot time dependency check reads.

The function stays callable. A test, another handler or a `#[job]` can call it directly.

### Arguments

Everything `#[endpoint(..)]` accepts:

| Key | Form | Effect | Repeatable |
| --- | --- | --- | --- |
| `operation_id` | `operation_id = "users.create"` | overrides the derived id | last wins |
| `tag` | `tag = "users"` | adds a tag | yes |
| `hidden` | bare word | serves the route, excludes it from the document | flag |
| `deprecated` | bare word | marks the operation deprecated without deprecating the Rust item | flag |
| `response` | `response(409, "Email already registered")` | documents an extra status. `>= 400` gets a problem schema, `< 400` gets an empty response. Status must be in `100..=599` | yes |
| `example` | `example(request = "...", response = "...")` | attaches examples. Any Rust expression producing a `&str` works, so `include_str!("create.json")` does | `request` and `response` once each |
| `errors` | `errors = BlogError` | folds an error taxonomy in by calling its `Describe::describe` | yes |

A value that parses as JSON becomes that JSON; anything else becomes a JSON string. An example never
overwrites one a type already declared. Putting a plain `#[deprecated]` on the function works too:
the macro notices either spelling. `#[cfg(..)]` on the handler is copied onto the generated type and
both impls, so a feature gated handler gates cleanly.

An unknown key is a compile error with a suggestion and the full list, not a silent no-op.

### What it generates

The unchanged `async fn`, the `__moso_op_*` unit struct, an `impl Endpoint` whose `spec` writes the
operation, an `impl HandlerFn` holding one concrete non-generic future that runs the extractors and
calls you, and an always-on assertion block. The assertion block is what `#[axum::debug_handler]`
does opt in; Moso does it unconditionally, so a parameter that is not an extractor underlines the
parameter rather than a trait bound on the attribute.

> [!NOTE]
> That assertion block is not free. The expansion currently runs 152 to 179 lines per endpoint
> against an internal 60-line budget, and the workspace's own `xtask expand-size` check fails on it.

## Route tables

`routes!` takes rows of `METHOD "path" => handler`, comma separated, trailing comma optional. An
empty table is legal and expands to `Router::new()`.

```rust
fn router() -> Router {
    moso::routes! {
        GET    "/users"      => list,
        POST   "/users"      => create,
        GET    "/users/{id}" => show,
        DELETE "/users/{id}" => destroy,
    }
    .tag("users")
}
```

The methods are `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, `TRACE` and `ANY`, and
they are case insensitive, so `get "/users" => list` works. `ANY` expands to seven registrations:
`GET`, `PUT`, `POST`, `DELETE`, `OPTIONS`, `HEAD` and `PATCH`. `TRACE` is deliberately not among
them, so register it explicitly if you want it.

A handler may live in another module. `GET "/posts" => posts::list`,
`crate::routes::posts::publish`, `super::posts::publish` and `::blog::routes::list` all work, because
only the last segment is rewritten. Two modules each defining a `list` do not collide.

The table lowers to an ordinary builder chain, which you can see with `cargo expand`:

```rust
::moso::__private::Router::new()
    .endpoint::<__moso_op_list>(
        ::moso::__private::HttpMethod::Get,
        ::moso::__private::route_path!("/users"),
    )
```

`moso::__private` is not public API and its contents change in patch releases. Write the macro, not
the expansion.

## One route at a time

A table is better at ten routes and worse at one. `ep!` names the companion type so the familiar
builder chain keeps the full metadata:

```rust
use moso::prelude::*;
use moso::extract::RequestId;
use moso::response::NoContent;

/// Liveness, documented.
#[endpoint]
async fn healthz() -> Result<NoContent> { Ok(NoContent) }

/// Liveness, undocumented. A plain `async fn` is a handler too.
async fn ping(_id: RequestId) -> Result<NoContent> { Ok(NoContent) }

let router = Router::new()
    .get("/healthz", moso::ep!(healthz))   // full OpenAPI metadata
    .get("/ping", ping);                   // registered, but undocumented

assert_eq!(router.entries()[0].spec.summary.as_deref(), Some("Liveness, documented."));
assert!(router.entries()[1].spec.summary.is_none());
```

`Router` has `get`, `post`, `put`, `patch`, `delete`, `head`, `options` and the general
`method(HttpMethod, path, handler)`. You rarely need `head`, because a `GET` route answers `HEAD`
automatically, or `options`, because CORS preflight is handled by the `cors` layer. The fully
explicit form, which the other two lower to, names the generated type itself:

```rust
use moso::openapi::HttpMethod;

let router = Router::new().endpoint::<__moso_op_list>(HttpMethod::Get, "/users");
```

To put several methods on one path in one statement, use `route` with a `MethodRouter`:

```rust
use moso::prelude::*;
use moso::router::{get, post};

let router = Router::new()
    .route("/users", get(moso::ep!(list)).post(moso::ep!(create)));
assert_eq!(router.len(), 2);
```

`MethodRouter` is deliberately smaller than `Router`: the free constructors are `get`, `post`, `put`,
`patch` and `delete`, and there is no `head`, `options` or free `on`. Reach for
`MethodRouter::new().on(HttpMethod::Head, handler)` when you need one of those.

## The three handler families

`Handler<M>` is implemented three times, and the marker `M` is always inferred, never written:

| What you register | `M` | What it documents |
| --- | --- | --- |
| `moso::ep!(list)` or a `routes!` row | `EndpointMarker` | the full operation from `#[endpoint]` |
| a plain `async fn`, every parameter an `Extract` | `(PartsOnly, T1..Tn)`, n up to 16 | nothing |
| a plain `async fn` ending in a body extractor | `(WithBody, T1..Tn, TB)`, n up to 15 | nothing |

Registering a plain `async fn` is legal and lossy. It compiles, it serves, and it contributes
nothing to the document. What it costs is visible in three places: the operation carries
`x-moso-undocumented: true` so it survives into an exported `openapi.json` and can be grepped for in
review, `moso routes` prints the handler as `<undocumented>`, and the route is skipped by the boot
time provider check because it declares no requirements.

Handler rules that hold for every family:

- 16 parameters maximum, or 15 plus a body extractor.
- At most one body extractor, and it must be last, because a body can only be read once.
- Extractors run left to right against the request parts, then the body extractor runs against the
  reassembled request, then your function is called. The first extractor that fails short circuits
  and becomes the response; no later extractor runs.
- The handler must be a non-generic `async fn` with no `where` clause, no `impl Trait` in argument or
  return position, and no `self` receiver.

Which parameter consumes the body is decided at expansion time by a name heuristic over the
outermost type: `Bytes`, `Form`, `Json`, `Multipart`, `Raw`, `RawBody`, `Stream`, `String`, `Text`,
`Upload`, `Xml`, plus any name longer than four characters starting or ending in `Body`, plus any
name ending in `Multipart` or `Upload`. A bare `String` parameter is therefore a body extractor,
while `Path<String>` is not.

> [!WARNING]
> If you write your own body extractor and its type name is not in that list, the macro treats it as
> a parts extractor and the error you get is `` `YourType` cannot be used as a handler parameter ``,
> which is true but about the wrong trait. Name the type so it ends in `Body`.

## Path syntax

Moso uses OpenAPI syntax everywhere, `{name}` and `{*rest}`, so the route table and the published
document spell a parameter the same way. Anything else is a compile error on your literal:

| Rejected | Why |
| --- | --- |
| `""`, `"users"` | a template must start with `/` |
| `/users/:id` | pre-0.8 Axum, Actix and Rocket syntax; write `{id}` |
| `/files/*rest` | pre-0.8 wildcard syntax; write `{*rest}` |
| `/users/{id`, `/users/id}` | unbalanced braces |
| `/users/{}` | a parameter must have a name |
| `/users/{a}{b}` | one parameter per segment |
| `/users/{id}x`, `/files/v{version}` | static text may not sit beside a parameter in a segment |
| `/{*rest}/more` | a catch-all must be the last segment |
| `/{id}/posts/{id}` | duplicate parameter name |

Parameter names are `[A-Za-z0-9_]+`, because the name must also work as a field on the `Path<T>`
struct that reads it.

"Beside" means either side. `/users/{id}x` and `/files/v{version}` are refused alike, because a
segment that holds a parameter holds nothing else. `matchit` would accept the second and capture
`v1` rather than `1`, so the router and the OpenAPI path template would quietly disagree about what
`{version}` is worth. Give the prefix its own segment: `/files/v1/{name}`.

Every literal is checked three times, on purpose. `routes!` checks it in the macro, where it has your
span and can print your own path corrected. The expansion wraps it in `route_path!`, which binds it
to a named `const` so `cargo check` cannot let a bad path through in silence. `Router::method` checks
it again at registration, which catches a `&'static str` that was never a literal. Both checkers are
public: `moso::router::validate_path` is `const` and returns the path unchanged or panics, and
`moso::router::path_parameters` returns the names a template declares, so `/files/{*rest}` yields
`["rest"]`.

The three checks answer the same question in three places and must never answer it differently.
`validate_path` used to be the looser of them (it accepted `/files/v{version}` that `routes!`
rejected), so `Router::new().get("/files/v{version}", ep!(f))` compiled and
`routes! { GET "/files/v{version}" => f }` did not. `validate_path` now carries the stricter rule,
and both reject it.

## Composing routers

`nest` mounts a router under a prefix and rewrites the inner paths. `merge` splices a router in as a
sibling with no prefix.

```rust title="src/routes/mod.rs"
/// Every route this application serves.
pub fn router() -> Router {
    Router::new()
        .merge(health::router())
        .nest("/api/v1", api_v1())
}

/// Version 1 of the API.
fn api_v1() -> Router {
    posts::router().responds(429, ResponseSpec::problem("Too many requests."))
}
```

The difference that matters is metadata. `nest` pushes the outer router's accumulated tags, security
requirements, extra responses, deprecation, hidden flag and `x-*` extensions down onto the routes it
absorbs, because a nested router is a section of the API. `merge` does not, because a merged router
is a sibling that already described itself. Metadata applied *after* a merge does reach the merged
routes, like any other route registered so far.

A prefix may itself contain a path parameter, and the parameter is available to the nested handlers.
There is no versioning DSL: `nest` plus separate modules is the versioning story.

At the top, `App::mount(router)` merges at the root and `App::mount_at(prefix, router)` nests.

Three composition rules worth knowing before they surprise you:

- **One composed router serves one fallback.** An absorbed router's `fallback` and
  `method_not_allowed` are adopted only if the outer router has none.
- **You cannot nest under a catch-all.** `Router::nest("/files/{*rest}", inner)` records a boot
  problem, returns the outer router unchanged and silently drops the inner routes. The problem
  surfaces through `Router::conflicts()` and at `App::build()`, so you will see it, but the routes
  are gone until you fix the prefix.
- **A composed router forgets the inner router's *pending* metadata.** Both `nest` and `merge`
  discard it. Every absorbed route already carries its own metadata individually, so no route loses
  anything, but the outer router's `metadata()` will not list the inner router's tags afterwards.

## Tags and document metadata

Five builder methods attach OpenAPI metadata to a group of routes:

| Method | What it does |
| --- | --- |
| `tag("users")` | adds a tag |
| `security(SecurityRequirement::bearer("jwt"))` | requires a scheme |
| `deprecated()` | marks the operations deprecated |
| `responds(429, ResponseSpec::problem("Rate limited"))` | documents an extra response |
| `hidden()` | removes the operations from the document, still serving them |

All five apply to the routes registered **before** the call, not after. That is Tower's ordering,
inherited rather than invented, and it is what lets one router file split public reads from guarded
writes:

```rust title="src/routes/posts.rs"
/// Every posts route. Two tables rather than one, because the second is
/// guarded and `.guard(..)` applies to the routes registered so far.
pub fn router() -> Router {
    let public = moso::routes! {
        GET "/posts"      => list,
        GET "/posts/{id}" => show,
    };

    let protected = moso::routes! {
        POST   "/posts"              => create,
        PATCH  "/posts/{id}"         => update,
        DELETE "/posts/{id}"         => destroy,
        POST   "/posts/{id}/publish" => publish,
    }
    .guard(ApiKeyGuard);

    public.merge(protected).tag("posts")
}
```

`hidden()` is for internal endpoints, not for security. The route is still served.

## Layers, guards and timeouts

`Router::layer` takes any `tower::Layer<Route>`, the same bounds `axum::Router::layer` uses, and
applies it to the routes registered so far. `Route` is Moso's alias for
`BoxCloneSyncService<Request, Response, Infallible>`.

```rust
let router = Router::new()
    .post("/auth/login", moso::ep!(login))
    .layer(ThrottleLayer::new())              // login only
    .post("/auth/logout", moso::ep!(logout)); // not throttled

assert_eq!(router.entries()[0].layers.len(), 1);
assert_eq!(router.entries()[1].layers.len(), 0);
```

`Router::guard` looks similar and does something a layer cannot: a guard contributes to the
document. The 401 or 403 it can return and the security requirement it implies appear on every
operation it protects. A bare Tower layer that rejects requests makes the document quietly wrong,
which is the gap guards exist to close.

```rust title="src/auth.rs"
#[derive(Debug, Clone, Copy)]
pub struct ApiKeyGuard;

impl Guard for ApiKeyGuard {
    fn describe(&self, op: &mut OperationBuilder) {
        op.security(SecurityRequirement::scheme(API_KEY_SCHEME));
        op.response(
            401,
            ResponseSpec::problem("The `x-api-key` header is absent or wrong."),
        );
    }

    fn check<'a>(&'a self, parts: &'a Parts, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let config = ctx.config::<AppConfig>()?;
            let presented = parts
                .headers
                .get(API_KEY_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or_default();

            if secret_eq(presented, config.api_key.expose()) {
                Ok(())
            } else {
                Err(Error::unauthenticated()
                    .with_detail("Present the shared key in the `x-api-key` header"))
            }
        })
    }
}
```

Guards run after routing and before extraction, in registration order. The first to return `Err`
short circuits and its error is rendered as the response. `Guard` is the trait you write;
`moso-core` ships exactly one implementation, `middleware::RequireHeader`, which is for gating a
mesh internal or debug surface and is explicitly not authentication.

`Router::timeout(Duration)` installs a per route timeout on the routes registered so far. Expiry is
a 504 problem document, not a dropped connection. The global default lives in the middleware stack,
covered in [middleware](./middleware.md).

Layers apply innermost first: the first layer pushed onto an entry is the one closest to the
handler. Per route layers sit inside the global stack.

> [!NOTE]
> The `.layer()` and `.guard()` scoping rule is the most common source of confusion in this API, and
> both mitigations are now built. `moso middleware --route /users` prints the effective stack for one
> route as a single numbered list, outermost slot through per route layers to the handler; and
> `moso check`'s `stale_layer` lint reports a `.layer()` or `.guard()` that is the last call in a
> router function, which is the shape that reads as "cover everything" and covers nothing.

## Fallbacks, static files and foreign routers

`Router::fallback(handler)` replaces the default 404 problem document.
`Router::method_not_allowed(handler)` replaces the 405 body. Both take any handler, so
`moso::ep!(not_found)` documents the fallback like any other operation.

Left alone, both defaults are RFC 9457 problem documents with the same `type` URIs an `Error` of the
same kind would carry (`https://moso.rs/errors/not-found` and
`https://moso.rs/errors/method-not-allowed`), so a test written against a `type` passes whether
the response came from a handler or from the router itself. The 405 keeps its `Allow` header
either way: Axum attaches it to whatever the method-router fallback returned, and it lists exactly
the methods Axum will route, which is why the body does not repeat them.

`Router::static_files(path, source)` serves files from `StaticSource::dir("public")`,
`StaticSource::spa("dist", "index.html")` for a single page application, or
`StaticSource::embedded(FILES)` for files compiled into the binary. `dir` and `spa` both default the
index to `index.html`. The server refuses path traversal (percent encoded `..`, backslashes, NULs
and symlinks escaping the root), answers only `GET` and `HEAD`, emits `ETag` and `Cache-Control`,
answers `If-None-Match` with 304, prefers precompressed `.br` and `.gz` siblings when the client
accepts them, and marks fingerprinted names such as `app.4f3a9c1e.js` as `immutable`.

Two end-to-end tests drive a mount through the composed Axum router rather than through the internal
helpers: one serves an embedded file, its `ETag` and a bodiless `HEAD`, and one points a directory
mount at this crate's own `src` and proves that `/assets/../Cargo.toml` (a file that genuinely
exists one level outside the root, in plain and percent-encoded spellings) is a 404 that leaks
nothing.

`Router::mount_axum(prefix, axum_router)` mounts an arbitrary `axum::Router<()>`. It is a real escape
hatch with real costs: routes mounted this way contribute nothing to the document, are invisible to
conflict detection, to the provider check and to `moso routes`, and run outside the adapter that
installs the `RequestCtx`, so Moso extractors used inside them fail at runtime.

`Router::into_axum()` hands back the plain `axum::Router<()>` at the edge of the application. It
drops OpenAPI metadata and attaches no application state, so every matched route answers a 500
problem document saying the router was mounted without an application. `App::into_service()` is the
version that keeps state, and is what tests use.

## What boot checks

`Router` accumulates a `Vec<RouteEntry>` and builds nothing. The `axum::Router` is constructed once,
at `App::build()`, after the checks have run. That ordering is what turns a route conflict from a
`matchit` panic naming neither location into a report naming both.

`App::build()` never fails fast. It runs every check and returns all the problems at once:

1. **Route conflicts.** Identical paths, or paths differing only in a parameter name, since
   `matchit` cannot tell `/users/{id}` from `/users/{user_id}`.
2. **Reserved paths.** An application route at `http.health_path`, `http.ready_path`,
   `http.docs_path` or `http.openapi_path` (by default `/healthz`, `/readyz`, `/docs` and
   `/openapi.json`) would be shadowed by the framework's own outer router. The fix in the error text
   is to move the framework's route in configuration.
3. **Providers.** Every `Inject<T>` a route reaches, transitively through its extractors, checked
   against the frozen provider map and grouped by provider, so a missing `Db` is one problem with
   nine route lines rather than nine problems.
4. **The document.** Duplicate operation ids, schema name collisions, and `Path<T>` field names that
   do not match the route template.

Here is a build with two problems, rendered exactly as it prints when stderr is not a terminal. On a
terminal you get colour and a `✗` bullet instead of the `x`:

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

You can run the conflict check yourself, without building anything, with `Router::conflicts()`.

Two limits to know. Wildcard shadowing (a catch-all swallowing routes registered after it) has a
declared reason code but is **not detected**: no check constructs it. And when routes do conflict,
`into_axum` keeps the first registration of each method within a path shape and drops the rest. That
is safe under `App::build()`, which reported the conflict first, but `build_unchecked()` and
`into_axum()` skip the report and the later route simply does not exist.

### Where a route sits at runtime

The `axum::Router` is composed once, at the end of boot. Your routes live under a fallback, behind
the middleware stack; the framework's own four sit on the outer router, outside it:

```text
outer router
  GET /healthz          answered outside the middleware stack: no access log,
  GET /readyz           no compression, no request-id span, no timeout
  GET /openapi.json
  GET /docs
  fallback -> middleware stack -> application router -> route -> handler
```

Within a matched route, the guards run in registration order, then the extractors, then your
function, then the return value is converted. Nothing else happens per request: no schema
generation, no `Endpoint::spec`, no provider lookup by name, and no walk of the route table beyond
`matchit`. `/docs` and `/openapi.json` are only mounted when the `openapi` feature is on, which is a
default; the document itself is assembled either way, so `App::openapi()` works in every build.

## Compiler errors you will actually hit

These are the committed snapshots from the framework's own `trybuild` corpus, so this is the text
rustc prints.

A path in Axum 0.7, Actix or Rocket syntax, the single most likely first mistake:

```text
error: legacy path parameter syntax: write `{id}`, not `:id`

       note: a route and the operation it documents spell a parameter the same way
       help: write "/users/{id}"
  --> tests/ui/routing/legacy_path_syntax.rs
   |
   |         GET "/users/:id" => show,
   |             ^^^^^^^^^^^^
```

A mistyped method in a table, answered with the nearest match and the full list:

```text
error: unknown HTTP method `PSOT`

       help: did you mean `PUT`?
       help: methods are `GET`, `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, `TRACE` and `ANY`
```

A body extractor that is not last. The caret lands on the parameter that should not be there, not on
a trait bound:

```text
error: request body extractor must be the last parameter

       note: `Json<CreateUser>` consumes the request body, so no parameter may follow it
       note: only one body extractor is allowed per handler
       help: move `Json<CreateUser>` to the end of the parameter list
```

A return type that is neither a `Schema` nor a `Responder` gets `` `Report` cannot be returned from
a handler ``, with the caret on the return type and notes offering `#[derive(moso::Schema)]`,
`#[derive(moso::Responder)]` and the wrappers. [Responses](./responses.md) covers that one.

The rest of the family, with what each one means:

| Message | Cause | Fix |
| --- | --- | --- |
| unterminated `{` in a route path | unbalanced braces in a template | the `help:` line prints your own path corrected |
| only one body extractor is allowed per handler | two body extractors in one signature | model the payload as one type and take it once |
| handlers must be `async fn` | a synchronous handler | make it `async` |
| handlers may not be generic | a type parameter on the handler | name the concrete type, or take a trait object: `Inject<Arc<dyn Mailer>>` |
| handlers support at most 16 parameters | too many parameters | group them into a `#[derive(Dependency)]` struct and take it as one |
| unknown `#[endpoint]` argument `tags` | a misspelt attribute key | the error prints the nearest match and the full list |
| future cannot be sent between threads safely | a non-`Send` value held across an `.await` | rustc names the binding, its type and the offending `.await` |
| cannot find type `__moso_op_lst` in this scope | a mistyped handler name in a `routes!` row | the underline is on your token, `lst`; fix the spelling |
| `` `ep!` takes a handler name, not a whole route `` | `ep!(GET "/healthz" => healthz)` | the error prints `Router::new().get("/healthz", ep!(healthz))` and the table spelling |
| expected a handler name | a `routes!` row whose right hand side is not a path | write the plain name; a handler in another module keeps its path, as in `users::list` |
| a handler name may not carry generic arguments | `ep!(list::<T>)` | name a concrete handler |

Two more that are worth stating before you meet them:

- Several of these notes end with "run `moso check` for a diagnosis of this specific handler".
  **That command exists**, and runs ten lints over the assembled router, the generated document and a
  lexical scan of `src/`. It will not diagnose *this* handler's trait bound, though: it never
  compiles your crate for the answer, so what rustc has already told you is the whole of that story.
- `#[requires]` and `#[public]`, the authorization macros, must sit **above** `#[endpoint]`, because
  Rust expands the outermost attribute first and `#[endpoint]` builds its glue from the signature it
  sees. They refuse to expand otherwise and say so. See [permissions](./permissions.md).

One structural failure worth avoiding: `#[endpoint]` on a method with a `self` receiver produces a
cascade rather than one error, because the generated struct cannot live inside an `impl` block and
no placeholder can be emitted. Handlers are free functions.

## Seeing the route table

```bash
moso routes
```

```text
METHOD  PATH                HANDLER        AUTH      TAGS    SOURCE
GET     /api/v1/users       users::list    session   users   src/routes/users.rs:14
```

`AUTH` lists the security schemes the route requires, or `-` for a public one. `TAGS` is the same,
`-` when there are none. `SOURCE` is where you wrote `#[endpoint]`, with `(deprecated)` and
`(hidden)` appended when they apply. Undocumented routes show `<undocumented>` in the handler column,
and the command counts them under the table:

```text
  ⚠ 2 of 9 routes are undocumented  (registered without `#[endpoint]`)
      → put `#[endpoint]` on the handler and register it with `routes!`
```

| Flag | Effect |
| --- | --- |
| `--tag <TAG>` | show only routes carrying that tag. A tag matching nothing is an error listing the tags in use, not an empty table |
| `--all` | include routes hidden from the OpenAPI document |
| `--json` | emit the rows as JSON, with a `total` count |
| `--bin`, `--features`, `--release`, `--manifest-path` | how to build and reach the application |

The rows come from the application, not from parsing source: the command builds your binary and runs
it with `--dump-routes`. That is the consequence of having no link time registry. A route registered
by a loop, by a `nest`, or by a function in a dependency shows up exactly as it will be served, and
`moso routes` cannot work on a project that does not build.

The same data is available in process, before anything becomes a service:

| Call | Gives you |
| --- | --- |
| `Router::describe()` | one `RouteInfo` per route: method, path, handler name, operation id, summary, tags, security scheme names, source location, the `documented`, `hidden` and `deprecated` flags, the names of the layers applied and the guard count |
| `App::router_info()` | the same rows, after boot |
| `Router::entries()`, `into_entries()` | the raw `RouteEntry` registrations, including the preview `OperationSpec`, the provider requirements, the layers and the guards |
| `Router::conflicts()` | the conflicts, computed without building anything |
| `Router::len()`, `is_empty()`, `metadata()`, `fallback_handler()`, `method_not_allowed_handler()`, `static_mounts()`, `axum_mounts()` | the rest of the accumulated state |

One subtlety if you read `entries()`: `RouteEntry::spec` is a **preview**, built at registration time
with a throwaway schema generator, and its `$ref`s only resolve against the document `App::build()`
assembles. It is what `moso routes` prints and what the conflict report reads the source location
from. The authoritative description is `RouteEntry::describe`, which re-runs the handler's
`Endpoint::spec`, then the positional path parameter naming, then each guard's `describe`, then the
router metadata, against the document's own generator. That order is what makes first-writer-wins
mean "the handler's own words win". See [OpenAPI](./openapi.md).

## See also

- [Extractors](./extractors.md) and [responses](./responses.md), for what a handler parameter and a
  return type may be.
- [Middleware](./middleware.md), for the global stack per route layers sit inside.
- [OpenAPI](./openapi.md), for what the document does with everything on this page.
- [Errors](./errors.md), for what a guard or extractor failure becomes on the wire.
