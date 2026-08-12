---
title: Middleware
description: Edit the default middleware stack by slot name, write your own middleware as one async fn, and scope layers, guards and timeouts to individual routers.
order: 9
status: shipped
---

Every Moso application serves through a middleware stack it did not have to write. Fourteen named
positions run in a fixed order: panics are caught, every request gets a correlation id, errors become
problem documents, bodies are capped, security headers go out. You get that by doing nothing.

When you do want to change it, you edit it by name rather than by rebuilding a `ServiceBuilder`
chain. `s.disable(Slot::Compression)` says what it means, and `s.insert_after(Slot::Trace, "tenant",
TenantLayer::new())` puts your layer somewhere you can point at. Moso does not invent a middleware
abstraction: Tower's `Service` and `Layer` are the abstraction and everything in `tower-http` works
unmodified. What is added here is the default stack, the named slots, the `#[middleware]` attribute
and guards.

> [!IMPORTANT]
> `Slot::RateLimit` and `Slot::Session` are reserved positions, empty by design and *fillable*:
> `replace_custom` takes the `CustomLayer` a battery ships, and you fill the slot with the layer your
> app chooses. See [inserting and replacing](#inserting-and-replacing) and
> [failure modes](#failure-modes-and-sharp-edges).

## The default stack

`MiddlewareStack::standard()` is what `App::new` gives you, and `MiddlewareStack::default()` is the
same thing. Position 1 is outermost: it sees the request first and the response last.

| # | Slot | On by default | What it does |
| --- | --- | --- | --- |
| 1 | `catch_panic` | yes | Turns a panic into a 500 problem document instead of a dropped connection. |
| 2 | `request_id` | yes | Adopts a well-formed client `x-request-id` or generates a ULID, echoes it back. |
| 3 | `trace` | with the `tracing` feature | Opens the `http.request` span. Records the outcome, does not log. |
| 4 | `sensitive_headers` | yes | Marks `Authorization` and friends sensitive on request and response. |
| 5 | `catch_error` | yes | The one place an `Error` becomes a problem document, and the one log line per request. |
| 6 | `request_limits` | yes | Refuses an oversized request *head*: a 414 past `http.uri_max`, a 431 past `http.header_max_count` or `http.header_max_bytes`. |
| 7 | `timeout` | yes | 30 seconds, then a 504 problem document. |
| 8 | `body_limit` | yes | 2 MiB, refused up front when `Content-Length` declares it, cut off mid-stream otherwise. |
| 9 | `normalize_path` | yes | Trims trailing slashes internally, with no redirect. |
| 10 | `cors` | no | Needs the `cors` feature and a configured origin list. |
| 11 | `security_headers` | yes | HSTS, `nosniff`, `Referrer-Policy`, `frame-ancestors 'none'`, `X-Frame-Options: DENY`. |
| 12 | `compression` | with the `compression` feature | Brotli then gzip, over 1024 bytes, quality 4. |
| 13 | `rate_limit` | no | A position. No built-in implementation. |
| 14 | `session` | no | A position. No built-in implementation. |
| 15 | `metrics` | no | On as soon as you set a recorder. |

`request_limits` sits inside `catch_error`, so its refusal is logged like any other error, and
outside `timeout`, because there is no sense starting a thirty-second budget for a request that is
about to be refused in a microsecond. It reads the same `Limits` snapshot the extractors read from
`RequestCtx::limits`, so the layer and the extractors cannot disagree about a number. Note what it is
*not*: hyper has already framed and parsed the head before any Rust web framework sees it, under
limits of its own. What this position adds is policy: an RFC 9457 document naming the operator's
own configured limit, instead of a bare connection-level refusal a client cannot parse. Keep a
reverse proxy enforcing the same bounds further out; the tighter one wins.

The snake_case names in that table are what `Slot::as_str` returns and what `MiddlewareStack::render`
prints. The enum spellings are `Slot::CatchPanic`, `Slot::RequestId`, `Slot::Trace`,
`Slot::SensitiveHeaders`, `Slot::CatchError`, `Slot::RequestLimits`, `Slot::Timeout`,
`Slot::BodyLimit`, `Slot::NormalizePath`, `Slot::Cors`, `Slot::SecurityHeaders`,
`Slot::Compression`, `Slot::RateLimit`, `Slot::Session` and `Slot::Metrics`. `Slot::ORDER` is the
array, outermost first, and `Slot::has_builtin()` tells you whether a position ships with an
implementation.

Every per-slot configuration type exists whether or not its Cargo feature is on, so turning
`compression` or `cors` on never changes the shape of your `with_middleware` block. Enabling a slot
whose feature is off is a boot error, not a silent no-op.

Composition happens once, at boot. `MiddlewareStack::compose_routed` folds the enabled entries around
the router from the inside out and the result is the service the listener calls. Nothing in that path
runs per request.

> [!NOTE]
> `/healthz`, `/readyz`, `/openapi.json` and `/docs` are mounted on an outer router and run outside
> the stack entirely: no access log, no compression, no request-id span, no timeout, no security
> headers. See [health and shutdown](./health-and-shutdown.md).

### The stack runs outside routing, and still knows the route

The composed stack is the *fallback service* of that outer router, which puts it outside Axum's
routing. It has to be there: `normalize_path` rewrites the URI, and a rewrite after matching would
change nothing, because `/users/` would already have failed to match `/users`.

That would leave the three slots that want the matched **pattern** (`trace`'s `route` span field,
`timeout`'s exemption list, `metrics`' `route` label) with nothing to read, because Axum publishes
the pattern only during routing. So the stack resolves it itself. `App::build` reduces the route
table to a `RoutePatterns` matcher and installs one step outside the whole stack that resolves the
request path and publishes the answer as a request extension; every slot inside then reads the same
value.

```text
outer router
  ├── /healthz  /readyz  /openapi.json  /docs      outside the stack entirely
  └── fallback → resolve route pattern             ← one lookup, before slot 1
                   → catch_panic → … → metrics
                     → application router → route → handler
```

Three things are worth knowing about it:

- The matcher is `matchit`, the same crate at the same version Axum routes with, given the same
  strings in the same registration order. The label therefore cannot disagree with the router,
  including where a hand-written matcher would, such as `/users/me` winning over `/users/{id}`.
- It resolves through whatever `normalize_path` will do: `/users/` resolves to `/users` under the
  default `Trim`, and to nothing under `Redirect`, because a redirected request never reaches the
  route table at all.
- Anything Moso does not know the pattern of resolves to `<unmatched>`, never to the raw path. That
  covers a 404, a `Router::mount_axum` mount and a `Router::static_files` mount, the same set that
  is absent from the OpenAPI document, for the same reason. They all fold into one series.

If you compose a stack by hand, `compose_routed(patterns, service)` is the call that installs the
resolver; `compose(service)` still exists and resolves nothing, so its slots see `<unmatched>`.

## Editing the stack

`AppBuilder::with_middleware` hands you `&mut MiddlewareStack`. Everything on it is a chainable
setter.

```rust title="src/main.rs"
use moso::prelude::*;
use moso::middleware::Slot;
use std::time::Duration;

let app = App::new(AppConfig { name: "shop".to_owned() })
    .with_middleware(|s| {
        s.timeout(Duration::from_secs(10));
        s.body_limit(1 << 20);
        s.disable(Slot::Compression);
    });
```

The typed setters, one per configurable slot:

| Call | Effect |
| --- | --- |
| `s.timeout(Duration)` | The request budget. Default 30 s. |
| `s.timeout_exempt(pattern)` | Exempt a route pattern from the budget. Give it the pattern, `/events/{id}`, never a raw path. |
| `s.body_limit(bytes)` | The request body cap. Default 2 MiB. |
| `s.request_limits(\|l\| ..)` | The whole `Limits` snapshot by field, as in `l.uri_max = 2048`. This slot enforces `uri_max`, `header_max_count` and `header_max_bytes`; the extractors read the rest of it. |
| `s.cors(CorsConfig)` | Configure CORS. Implies `enable(Slot::Cors)`. |
| `s.normalize_path(TrailingSlash)` | `Trim` (default), `Append`, `Redirect` (308), `Off`. |
| `s.security_headers(\|h\| ..)` | HSTS, CSP, referrer policy, frame options, permissions policy, arbitrary headers. |
| `s.request_id(\|r\| ..)` | `header(..)`, `always_generate()`, `no_echo()`. |
| `s.compression(\|c\| ..)` | `encodings(..)`, `min_size(..)`, `skip(..)`. |
| `s.catch_panic(\|p\| ..)` | `render_details()` to put the panic message in the body. |
| `s.catch_error(\|e\| ..)` | `expose_internal_errors()`, `log_headers()`. |
| `s.trace(\|t\| ..)` | `level(..)`, `with_user_agent()`. |
| `s.sensitive_headers(\|h\| ..)` | `add(..)`, `set(..)`. |
| `s.metrics(Arc<dyn MetricsRecorder>)` | Install a recorder. Implies `enable(Slot::Metrics)`. |
| `s.silence(path)` | Keep a path prefix out of the log and the metrics. |
| `s.disable(Slot)` and `s.enable(Slot)` | Turn a position off, or back on at its canonical index. |

`enable` reinserts at the canonical position, so you cannot reorder the stack by accident when you
turn something back on. Two setters have behaviour worth knowing before you reach for them.
`normalize_path` defaults to `TrailingSlash::Trim`, which rewrites the path internally and never
redirects; `Redirect` answers 308 so the method survives, and the query string survives either way.
Collapsing repeated slashes is a separate switch and is off. `request_id` adopts an inbound
`x-request-id` only when it is non-empty, ASCII-graphic, within 128 characters **and** parses as a
ULID; anything else is replaced with a fresh one rather than refused.

### Inserting and replacing

Four methods take a real `tower::Layer`, and all four carry the same bounds `axum::Router::layer`
uses. The `&'static str` you pass is the name that shows up in `describe()` and `render()`.

```rust
let mut stack = MiddlewareStack::default();
stack.timeout(Duration::from_secs(10));
stack.body_limit(1 << 20);
stack.disable(Slot::Compression);
stack.insert_after(Slot::Trace, "tenant", TenantLayer::new());
stack.security_headers(|h| { h.csp("default-src 'self'"); });
```

- `insert_before(slot, name, layer)` puts it immediately outside a named position.
- `insert_after(slot, name, layer)` puts it immediately inside.
- `append(name, layer)` puts it innermost, next to the router.
- `replace(slot, layer)` swaps a built-in for yours, keeping the slot's position and printed name.

Inserting relative to a name never renumbers anything, which is the point of the design.

#### The same four, for a battery layer

A battery layer usually implements `moso::middleware::CustomLayer` rather than
`tower::Layer<Route>`, because `CustomLayer::apply` is already `Route -> Route`, which is exactly
what the stack folds, and because a name and a `render()` summary come with it. `moso-auth`'s
`SessionLayer` and `moso-orm`'s `RequestTxLayer` are both that shape. Each installer has a `_custom`
sibling that takes one:

| Sibling | Same as |
| --- | --- |
| `insert_before_custom(slot, layer)` | `insert_before` |
| `insert_after_custom(slot, layer)` | `insert_after` |
| `append_custom(layer)` | `append` |
| `replace_custom(slot, layer)` | `replace` |

They take no name: `CustomLayer::name()` already has one, and asking for it twice is a second place
for it to be wrong. `replace_custom` still prints the *slot's* name, so a filled `Slot::Session` is
listed as `session`.

They are siblings rather than one widened method for a coherence reason, not a taste one: "either a
`tower::Layer<Route>` or a `CustomLayer`" needs a trait with a blanket impl for each, nothing stops a
type from implementing both, and the compiler rejects the overlapping pair (E0119). Siblings also
leave every existing call site inferring exactly what it inferred before.

This is how you fill one of the two empty positions:

```rust
use moso_auth::SessionLayer;

App::new(cfg).with_middleware(|s| {
    // A `CustomLayer`: what a battery ships. Keeps the slot's position and name.
    s.replace_custom(Slot::Session, SessionLayer::new(store, config));
    // A plain `tower::Layer<Route>` uses `replace`.
    s.replace(Slot::RateLimit, MyRateLimitLayer::new());
    // ...or say you do not want it, and the boot error goes away.
    s.disable(Slot::Session);
});
```

### Turning CORS on

CORS is off until you configure it, and it needs the `cors` Cargo feature, which pulls
`tower-http/cors`. There is deliberately no `CorsConfig::permissive()`.

```rust title="src/main.rs"
use moso::middleware::CorsConfig;

let app = App::new(cfg).with_middleware(|s| {
    s.cors(
        CorsConfig::allow_origins(["https://app.example"])
            .allow_credentials(true)
            .max_age(Duration::from_secs(600)),
    );
});
```

`allow_origins` and `any_origin()` are the two constructors, and both put `x-request-id` into
`expose_headers` so a browser client can read the correlation id. When you set no allowed request
headers the layer mirrors whatever the preflight asked for. Two mistakes are boot errors rather than
runtime surprises: `any_origin()` together with `allow_credentials(true)`, which a browser would
reject on every response, and an origin string that is not scheme, host and port (a trailing slash or
a path is the usual one, and it silently matches nothing).

### Starting from nothing

`MiddlewareStack::bare()` is genuinely empty: no `catch_panic`, no `catch_error`, nothing. A
handler's `Error` will not become a problem document until you enable `Slot::CatchError`.

```rust
let mut stack = MiddlewareStack::bare();
stack.enable(Slot::CatchPanic);
stack.enable(Slot::CatchError);
stack.enable(Slot::Timeout);
// enable() reinserts at the canonical position, so the order is still
// catch_panic, catch_error, timeout.
```

Hand a stack you built yourself to `App::new(cfg).middleware(my_stack)`, which replaces the default
wholesale rather than editing it.

### What configuration reaches the stack

`MiddlewareStack::configure` runs inside `App::build`, after your `with_middleware` closure, and
fills in whatever you did not claim from `HttpConfig` and the active `Profile`.

| `HttpConfig` field | Default | What it sets |
| --- | --- | --- |
| `timeout` | 30 s | `TimeoutConfig::timeout` |
| `body_max` | 2 MiB | `BodyLimitConfig::max_bytes` |
| `expose_internal_errors` | `false` | The problem document disclosure policy |
| `health_path` | `/healthz` | Added to the silent list |
| `ready_path` | `/readyz` | Added to the silent list |
| `docs_path` | `/docs` | Added to the silent list |
| `openapi_path` | `/openapi.json` | Added to the silent list |

`Profile::Dev` additionally turns on panic detail rendering and drops HSTS. `Profile::Test` and
`Profile::Production` do neither. `expose_internal_errors` is false in every profile and no profile
is allowed to flip it.

An explicit setter always wins over the derived value even though `configure` runs later. Five
setters record the fact that they were called for exactly this reason: `catch_panic`, `catch_error`,
`timeout`, `body_limit` and `security_headers`. Note that `HttpConfig` is a plain struct passed to
`AppBuilder::http_config`, not a TOML `[http]` table. See [configuration](./configuration.md).

## Writing middleware

`#[moso::middleware]` turns one `async fn` into a `Layer` and `Service` pair named after the
function.

```rust title="src/middleware.rs"
use moso::prelude::*;
use moso::deps::http::header::HOST;
use moso::middleware::Next;
use moso::{Request, Response};

/// Resolve the tenant from the `Host` header.
#[moso::middleware]
async fn tenant(mut req: Request, next: Next) -> Result<Response> {
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let tenant = Tenant::from_host(host).ok_or_else(|| Error::not_found("tenant"))?;
    req.extensions_mut().insert(tenant);
    Ok(next.run(req).await)
}
// TenantLayer::NAME == "tenant"
```

You get `TenantLayer` (a unit struct, `Clone + Copy + Debug + Default`, with `TenantLayer::new()` and
`TenantLayer::NAME`) and `TenantService<S>`. Register the layer anywhere a layer is accepted: a stack
slot, or `Router::layer`. The generated `Service` is generic, but every registration point erases the
inner service to `Route` first, so exactly one instantiation is compiled no matter how many routes it
covers.

Returning `Err(Error)` short-circuits: the error renders as a problem document with the right status
through the same path as every other error, so `?` works and there is no way to return a 200 with an
error body. `next.run(req)` yields a `Response`, not a `Result<Response>`, because a handler failure
has already been rendered by the time it reaches you.

`moso generate middleware observe` writes this skeleton for you and registers the module.

### Extractors in a middleware

Parameters before `req` are extracted before the body of your function runs, so `Inject<T>` works.

```rust title="src/middleware.rs"
/// Count every request and stamp every response.
#[moso::middleware]
pub async fn observe(
    Inject(metrics): Inject<Metrics>,
    req: Request,
    next: Next,
) -> Result<Response> {
    metrics.requests.fetch_add(1, Ordering::Relaxed);

    let mut response = next.run(req).await;

    if response.status().is_client_error() || response.status().is_server_error() {
        metrics.failures.fetch_add(1, Ordering::Relaxed);
    }
    response
        .headers_mut()
        .insert(APP_HEADER, HeaderValue::from_static("moso-crud"));

    Ok(response)
}
```

Two parameter kinds are compile errors, with the fix in the message:

```text
error: `Depends<CurrentUser>` cannot be used in middleware
  = note: middleware runs before extractors, so request dependencies are not yet available
  = help: read a middleware-inserted value with `req.extensions()`, or move this logic into
          a `Dependency` impl and use it in the handler
```

```text
error: `Json<Audit>` cannot be used in middleware
  = note: middleware runs before the body is read, and taking it here would consume it
  = help: take the body in the handler, or read it from the request: `let body = req.into_body();`
```

The body extractors rejected by name are `Json`, `Form`, `Multipart`, `RawBody` and `BodyStream`. The
macro also rejects a non-`async` function, a generic function or one with a where-clause, a method
with `self`, a variadic, fewer than two parameters, a last parameter that is not `Next`, a
second-to-last that is not `Request`, and a missing return type. When the signature is wrong it still
emits the layer and service types as a pass-through, so the registration site does not fail a second
time with "cannot find type".

Values a middleware computes travel to handlers as request extensions, read back with `Extension<T>`
or inside a `Dependency` impl. That is the only supported channel, because middleware runs before
extraction. See [dependency injection](./dependency-injection.md).

### Renaming what the macro generates

| Key | Value | Effect |
| --- | --- | --- |
| `name` | string literal | The name the stack prints. Defaults to the function's name. |
| `vis` | string literal parsed as a visibility | Visibility of the generated types. Defaults to the function's, widened to `pub(crate)` when the function is private. |
| `layer` | string literal parsed as an ident | Renames the `...Layer` type. |
| `service` | string literal parsed as an ident | Renames the `...Service` type. |

An unknown key is an error with a "did you mean" suggestion.

### Without the macro

`moso::middleware::from_fn` takes any `Fn(Request, Next) -> impl Future<Output = Result<Response>>`
and returns a `FromFn<F>` that implements `tower::Layer<Route>`. The macro is sugar over it, so a
closure works everywhere a generated layer works.

## Guards

A guard is middleware that also writes itself into the OpenAPI document. A bare layer that returns
403 makes the document quietly wrong, because nothing tells the document about it. A guard says both
halves: the runtime check, and the contract change.

```rust
use moso::prelude::*;
use moso::deps::http::request::Parts;
use moso::{BoxFuture, Guard};

/// Only let a request through when it carries an internal marker.
#[derive(Clone)]
pub struct RequireInternal;

impl Guard for RequireInternal {
    fn describe(&self, op: &mut OperationBuilder) {
        op.response(403, ResponseSpec::problem("Internal callers only"));
    }

    fn check<'a>(&'a self, parts: &'a Parts, _ctx: &'a RequestCtx)
        -> BoxFuture<'a, Result<()>>
    {
        Box::pin(async move {
            if parts.headers.contains_key("x-internal") {
                Ok(())
            } else {
                Err(Error::forbidden("internal callers only"))
            }
        })
    }
}

let router = Router::new()
    .get("/healthz", moso::ep!(healthz))
    .guard(RequireInternal);
```

`describe` runs once, at registration, against every operation on the router, which is why the 401 or
403 shows up in `moso routes` and in the document. `check` runs after routing and before extraction,
so it can read path parameters and reach the provider map and the per-request dependency cache
through `RequestCtx`. Guards run in registration order and the first `Err` short-circuits.

`describe` takes `&self` so a guard configured with a permission name or a header name can document
*that* configuration, and `check` returns a `BoxFuture` because the route table stores
`Arc<dyn DynGuard>` and the trait therefore has to be dyn-compatible. You never implement `DynGuard`
yourself: a blanket impl covers every `Guard`.

A production-shaped guard reads its configuration out of the context rather than being constructed
with it:

```rust title="src/auth.rs"
use moso::openapi::SecurityRequirement;

/// Rejects any request that does not present the shared API key.
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

Every built-in security check in the framework is a guard rather than a bare layer for this reason:
the auth battery's session and CSRF checks, the permission checks in
[permissions and roles](./permissions.md), and the rate limiter in
[rate limiting and locks](./rate-limiting.md).

### The shipped guard

`RequireHeader` ships as the worked example, small enough to read end to end.

```rust
use moso::prelude::*;
use moso::middleware::guard::RequireHeader;

let router = Router::new()
    .get("/_internal/state", moso::ep!(debug_state))
    .guard(RequireHeader::new("x-internal"));
```

`RequireHeader::with_value("x-internal", "value")` also checks the value, and `.described("...")`
sets the parameter description that lands in the document. A missing header is a 400 with pointer
`/headers/{name}` and code `required`; a wrong value is a 400 with code `enum`, and the expected
value is never echoed back. Both constructors panic on an invalid header name or value: they take
`&'static str` written in your composition root, so an invalid one is a typo you see the first time
you run the program.

> [!WARNING]
> `RequireHeader` is not authentication. A header a client can send is a header an attacker can send,
> and its comparison is variable-time on purpose because it is not comparing a secret. For real
> checks use the auth battery's guards, documented in [authentication](./authentication.md).

## Per route layers and timeouts

`Router::layer`, `Router::guard` and `Router::timeout` all apply to the routes registered **before**
the call. Where you put the call is what scopes it.

```rust
let router = Router::new()
    .post("/auth/login", moso::ep!(login))
    .layer(ThrottleLayer::new())          // login only
    .post("/auth/logout", moso::ep!(logout)); // not throttled

assert_eq!(router.entries()[0].layers.len(), 1);
assert_eq!(router.entries()[1].layers.len(), 0);
```

That rule is easy to get wrong when the `.layer()` is the last line of a router function, where it
reads as if it covered everything. Nothing warns you about it today. The readable way to express
"these four routes and not those two" is to build two routers and merge them:

```rust title="src/routes/posts.rs"
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

`Router::timeout(Duration)` is the same mechanism with a 504 problem document built in. It is how you
give one router a different budget. For a single long-lived route, `stack.timeout_exempt("/events/{id}")`
is the other way, and it names the pattern rather than the path so a crafted URL cannot widen it.

`nest` and `merge` move whole route entries, so layers and guards travel with their routes. Per-route
layers apply innermost first and see Axum's own `MatchedPath`, which agrees with what the global
stack resolved. See [routing](./routing.md).

> [!NOTE]
> `Router::layer` names the erased layer with `core::any::type_name::<L>()`, so `moso routes` prints
> a full Rust path such as `example_crud::middleware::ObserveLayer`. Only `MiddlewareStack` edits
> take a human-chosen name.

## Using Tower layers directly

Every registration point takes `L: tower::Layer<Route>` with the bounds `axum::Router::layer` uses,
so anything from `tower-http` drops in unmodified. The one concrete type every layer wraps is
`Route = BoxCloneSyncService<Request, Response, Infallible>`.

`MiddlewareStack` additionally takes an already-erased `CustomLayer` through the four `_custom`
siblings above. `Router::layer` does **not**. It still needs a real `tower::Layer<Route>`, so a
battery layer scoped to one router needs a newtype that implements `tower::Layer<Route>` and
delegates to `CustomLayer::apply`.

```rust
use moso::middleware::{CustomLayer, layer_fn};
use std::time::Duration;
use tower_http::timeout::TimeoutLayer;

// `layer_fn` erases any real `tower::Layer<moso::Route>` into one of these.
let erased = layer_fn("timeout", TimeoutLayer::new(Duration::from_secs(5)));

assert_eq!(erased.name(), "timeout");
```

A hand-written `Layer` and `Service` pair works too, and is what you want when you need `poll_ready`
or per-connection state the macro cannot express. It goes into the stack the same way:

```rust
let app = App::new(cfg).with_middleware(|s| {
    s.append("stamp", Stamp);
});
```

Because `CustomLayer::apply` is `Route -> Route`, every layer boundary re-boxes and each layer's
future is a `BoxFuture`. That is one small allocation per enabled layer per request, and it is the
price of a stack you can reorder at runtime and print.

Three slots use `tower-http` verbatim: `sensitive_headers`, `cors` and `compression`. Four do not,
because the `tower-http` version cannot meet Moso's contract: `timeout` (empty body, no exemptions),
`body_limit` (`RequestBodyLimitLayer` rewrites the request body type, which would break the erased
`Route`), `trace` (every callback would have to be `()` to keep one log line per request) and
`normalize_path` (it covers two of the four policies). `TraceLayer` is still one
`s.replace(Slot::Trace, ..)` away if you want its callbacks.

## Ordering rules that are enforced

`MiddlewareStack::validate` runs during `App::build` and every problem it finds becomes part of one
boot error. Three orderings are refused, because all three failures are subtle enough to survive
review:

| Rule | Why |
| --- | --- |
| `catch_error` must be inside `trace` | So the error log carries the span. Reversed, you get logs with no span. |
| `catch_error` must be outside `timeout` | So an expiry renders as a problem document with a request id, not a dropped connection. |
| `metrics` must be innermost | So the `route` label is `/users/{id}` and not one time series per user id. |

It also refuses a slot that is enabled and empty, `cors` or `compression` enabled without its Cargo
feature, CORS allowing any origin together with credentials, and a CORS origin string that is not a
valid origin. Each one carries a `fix` line:

```text
middleware slot `session` is enabled but empty
  `session` has no built-in implementation; it is a position a battery fills
  fix: s.replace(Slot::Session, YourLayer::new())   // or s.disable(Slot::Session)
```

`build_unchecked()` skips validation. You can also call `stack.validate()` yourself and get the same
`Vec<BootError>`.

## Inspecting the stack

The stack is data, so you can assert on it in a test or print it at startup.

```rust title="tests/middleware.rs"
use moso::middleware::{MiddlewareStack, Slot};

let stack = MiddlewareStack::standard();

assert!(stack.is_enabled(Slot::CatchError));
assert!(!stack.is_enabled(Slot::RateLimit));
assert!(stack.validate().is_empty());

for entry in stack.describe() {
    println!("{} {}", entry.name, entry.summary);
}

print!("{}", stack.render());
```

`render()` produces a block starting with `GLOBAL`, listing only what is enabled:

```text
GLOBAL
  1 catch_panic          render_details=false
  2 request_id           header=x-request-id generator=ulid
```

`describe()` returns `Vec<StackEntry>` with `slot`, `name`, `enabled`, `summary` and the erased
`layer`; `entry(slot)` fetches one. Each summary is re-rendered from the live configuration during
boot, so `describe()` and the behaviour cannot disagree.

Two accessors reach the live stack, and the difference between them matters:

| Accessor | What it shows |
| --- | --- |
| `AppBuilder::middleware_stack()` | The stack your code has edited so far. `configure` has **not** run, so the timeout and body limit are the ones you set, not the ones derived from `HttpConfig`. |
| `App::middleware_stack()` | The stack the built application is serving with, after `configure`. This is the one to print. |

### `moso middleware`

The CLI subcommand exists, and it is the answer to a question `render()` cannot reach on its own:
**is this route actually covered**.

```text
$ moso middleware

GLOBAL
#   MIDDLEWARE    ON   SUMMARY
1   catch_panic   yes  render_details=false
2   request_id    yes  header=x-request-id generator=ulid
…

PER ROUTE
METHOD  PATH             LAYERS              GUARDS
POST    /posts/{id}      Audit → Throttle    1
```

The global table is the stack the built application is serving with. The per-route table is what
`.layer()` and `.guard()` attached to individual entries, listed outermost first, in the order a
request meets them, which is the reverse of the order they were pushed. Routes carrying nothing but
the global stack are summarised as a count rather than listed, because a table of every route with
two empty columns is a table nobody reads.

| Flag | Effect |
| --- | --- |
| `--all` | include slots that are present but disabled. `compression` off and `compression` absent are different facts. |
| `--route <PATH>` | one list per matching route, global entries then per-route layers then the handler, numbered from outermost. An exact path wins; otherwise the filter matches any part of one. |
| `--json` | the structured form: `global[]` with `position`, `name`, `enabled`, `summary`, `builtin`, and `routes[]` with `layers` already in outermost-first order. |

A `--route` that matches nothing exits 1 rather than printing an empty stack, because an empty stack
for a path that does not exist reads like an answer.

The CLI does not link your crate. It runs your binary with `--dump-middleware` and reads one JSON
document: the structured entries, not `render()`'s text, because `--json` needs the fields and the
per-route table has to interleave data the stack does not carry. The application's half is one
function in `src/dump.rs`, which `moso new` writes:

```rust title="src/dump.rs"
fn middleware(app: &App) -> Value {
    let entries: Vec<Value> = app
        .middleware_stack()
        .describe()
        .iter()
        .enumerate()
        .map(|(position, entry)| json!({
            "position": position,
            "name": entry.name,
            "enabled": entry.enabled,
            "summary": entry.summary,
            "builtin": entry.slot.is_some(),
        }))
        .collect();
    json!({ "middleware": entries })
}
```

Disabled entries are sent and flagged rather than filtered out, so the CLI can decide; a project
generated before this existed answers without a `middleware` array and the command says which
function to copy.

`moso check` has the other half of the story: `stale_layer` reports a `.layer()` or `.guard()` that
is the last call in a router function, which is the shape that surprises people.

## Metrics and silencing

`Slot::Metrics` takes any recorder you implement. Moso does not depend on a metrics facade, so the
choice of backend stays out of the framework's dependency tree.

```rust title="src/metrics.rs"
use moso_core::middleware::metrics::{MetricsRecorder, RequestSample};
use std::sync::Mutex;

/// Keeps every sample, so a test can assert on them.
#[derive(Default)]
pub struct Collected(Mutex<Vec<String>>);

impl MetricsRecorder for Collected {
    fn record(&self, sample: &RequestSample<'_>) {
        self.0.lock().unwrap().push(format!(
            "{} {} {}",
            sample.method.as_str(),
            sample.route,
            sample.status.as_u16(),
        ));
    }
}
```

`RequestSample` carries `method`, `route`, `status`, `duration` and `in_flight`. Install the recorder
with `s.metrics(Arc::new(Collected::default()))`, which enables the slot. `record` runs on the
request's own task, so it must not block. Distinct route labels are capped at 2000 and everything
past the cap becomes `<other>`, with one warning ever, not one per request.

`s.silence("/internal/metrics")` keeps that path prefix out of the access log and the metrics. The
probes and the docs paths are silenced for you.

Process-wide counters are readable without an exporter: `catch_panic::panics_total()`,
`catch_error::failed_requests_total()`, `metrics::requests_total()` and `metrics::in_flight()`. They
are statics and are never reset, so assert on `>` rather than `==`. More in
[observability](./observability.md).

## Failure modes and sharp edges

**Only the route *table* has patterns.** The stack resolves the matched pattern for itself
([above](#the-stack-runs-outside-routing-and-still-knows-the-route)), but it can only resolve what
Moso registered. A `Router::mount_axum` mount, a `Router::static_files` mount and a plain 404 all
record `<unmatched>` on the span field, on the metric label and for `timeout_exempt`. That is
deliberate (Moso does not know those patterns and will not invent one, which is the same reason they
are absent from the OpenAPI document), but it means a mounted Axum sub-application is one series in
your dashboards, not one per route. `Router::layer` will not split it either: that applies to the
Moso route entries registered before the call, and a mount has none. Put an `axum::Router::layer` on
the sub-application before you mount it, where Axum's own routing publishes its patterns.

**`Slot::RateLimit` and `Slot::Session` are empty positions.** Neither has a built-in and
`Slot::has_builtin()` returns false for both, so enabling one and leaving it empty is a boot error.
They are fillable (`replace_custom` takes the `CustomLayer` a battery ships), but no battery in the
workspace installs itself into one for you, and rate limiting is deliberately shaped as a `Guard` you
apply with `Router::guard` rather than as a stack slot, because a guard also writes the 429 into the
document: see [rate limiting and locks](./rate-limiting.md).

**`PROVIDER_REQ` on a middleware is generated but never checked at boot.** The macro emits
`TenantLayer::PROVIDER_REQ` and `TenantLayer::required_providers()`, but nothing consumes them: boot
validation walks handler requirements only. A `#[middleware]` taking `Inject<Db>` with no
`.provide(Db)` fails at *request* time with a missing-provider problem document, not at boot. Declare
the provider, and assert on the constant yourself if you want the check.

**A `#[middleware]` with leading extractor parameters returns a 500 when it runs outside an
application.** It needs the `AppState` that `App::build` inserts. The message names
`App::with_middleware` and `Router::layer` as the fix.

**`stack.body_limit(n)` above `http.body_max` does not raise what a handler observes.** The
extractors read `http.body_max` from the request context and the smaller of the two wins.

**A client `x-request-id` that is not a well-formed ULID is silently replaced**, not rejected. Two
different ids in front of the same operator was judged worse than replacing one.

**`security_headers` uses `insert`, not `append`.** A handler that sets its own CSP for one response
has it overwritten. A deliberate per-response override belongs in a `Router::layer`.

**An unrepresentable header value is dropped with a `tracing::warn!`, not a boot failure.** True for
a bad CSP string, a bad sensitive-header name, and a bad request-id header name, which falls back to
`x-request-id`.

**`Encoding::Deflate` is never negotiated.** It exists in `Encoding::PREFERENCE` for completeness but
`is_available` returns false for it unconditionally, so `CompressionConfig::default().summary()` is
`br,gzip min=1024`. There is no zstd support anywhere in the workspace, whatever older design notes
say. Compression also does not solve BREACH: the mitigation is keeping a CSRF token out of a
compressible response body, which is why Moso's CSRF token travels in a header.

**There is no `CorsConfig::permissive()`, and there will not be one.** `any_origin()` exists, is
documented as credentials-incompatible, and the combination is refused at boot. The reasoning is in
[security](./security.md).

**The per-request cost of the stack is a design budget, not a measurement.** There is no middleware
benchmark in the workspace yet.

## See also

- [Errors](./errors.md) for what `catch_error` renders and how the log levels are chosen.
- [Routing](./routing.md) for `Router::layer`, `nest`, `merge` and what `moso routes` prints.
- [OpenAPI](./openapi.md) for the `OperationBuilder` vocabulary a guard's `describe` writes into.
- [Dependency injection](./dependency-injection.md) for why `Inject<T>` works in middleware and
  `Depends<T>` cannot.
- [Security](./security.md) for the defaults that are deliberately not defaults.
- [Testing](./testing.md), because `TestApp` boots the real application and therefore exercises the
  real stack.
