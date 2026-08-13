# 17 - Middleware

> ✅ **Status: implemented**, with two reserved slots. The stack has 14 named slots in the documented
> order, `#[middleware]`, `Guard`, per-route `layer` and `timeout`.
> ⛔ Nothing is installed into `Slot::RateLimit` (it needs a KV backend) or `Slot::Session` (it needs
> auth) - the slots exist and are empty, so the ordering is right when they land.
> ⛔ `moso middleware`, the CLI view of the composed stack, is not implemented even though
> `MiddlewareStack` can already render itself.
> `Guard::describe` takes **`&self`**, not an associated function: the route table stores
> `Arc<dyn DynGuard>`, so the trait must be dyn-compatible - and it is strictly better, because a
> guard configured with a permission can document that permission.

## Position

Moso does not invent a middleware abstraction. **Tower's `Service`/`Layer` is the middleware
abstraction**, and the entire `tower-http` ecosystem works unmodified. What Moso adds is:

1. A **default stack** that is correct, ordered, and configurable without knowing Tower.
2. A **named, reorderable** stack so "add CORS before auth" is a one-liner rather than a rewrite.
3. `#[middleware]`, a function-shaped sugar for the 90% case, because writing a `Layer` +
   `Service` + `Future` by hand is the single most-cited Tower papercut.
4. **Guards** - middleware that also documents itself in OpenAPI.

## The default stack

Order is outermost-first. Every entry is a named slot.

| # | Slot | Default | Notes |
| --- | --- | --- | --- |
| 1 | `catch_panic` | on | 500 problem, logged, counter |
| 2 | `request_id` | on | reads `x-request-id` or generates a ULID |
| 3 | `trace` | on | opens the span; all later logs inherit it |
| 4 | `sensitive_headers` | on | marks auth/cookie headers redacted for tracing |
| 5 | `catch_error` | on | converts `Error` → problem+json; the logging boundary |
| 6 | `timeout` | 30 s | returns 504 problem |
| 7 | `body_limit` | 2 MiB | 413 problem |
| 8 | `normalize_path` | trim trailing slash | configurable, incl. "off" |
| 9 | `cors` | **off** | must be configured explicitly; permissive CORS is never a default |
| 10 | `security_headers` | on | HSTS, X-Content-Type-Options, Referrer-Policy, frame-ancestors |
| 11 | `compression` | on | br > zstd > gzip; skips already-compressed types and SSE |
| 12 | `rate_limit` | off | KV-backed; on by default only for auth routes |
| 13 | `session` | on if `auth` | lazily loads; does not touch the store unless read |
| 14 | `metrics` | on if `metrics` | records after routing so the `route` label is the pattern |

Rationale for two ordering choices that people get wrong:
- **`catch_error` inside `trace`** so the error log carries the span, and outside `timeout` so a
  timeout renders as a problem document.
- **`metrics` after routing** so the label is `/users/{id}` and not a million distinct paths - the
  classic cardinality explosion.

## Configuring the stack

```rust
// example
App::new(cfg)
    .with_middleware(|s| {
        s.cors(CorsConfig::allow_origins(["https://shop.example"])
                 .allow_credentials(true));
        s.timeout(Duration::from_secs(10));
        s.disable(Slot::Compression);
        s.insert_after(Slot::Trace, "tenant", TenantLayer::new());
        s.security_headers(|h| h.csp("default-src 'self'"));
    })
```

```rust
// spec
impl MiddlewareStack {
    pub fn disable(&mut self, slot: Slot) -> &mut Self;
    pub fn insert_before<L>(&mut self, slot: Slot, name: &'static str, layer: L) -> &mut Self;
    pub fn insert_after<L>(&mut self, slot: Slot, name: &'static str, layer: L) -> &mut Self;
    pub fn replace<L>(&mut self, slot: Slot, layer: L) -> &mut Self;
    /// Print the composed stack - used by `moso middleware`.
    pub fn describe(&self) -> Vec<StackEntry>;
    /* one typed setter per configurable slot: .cors(..), .timeout(..), … */
}
```

`moso middleware` prints the resolved stack so nobody has to guess:

```
$ moso middleware
GLOBAL
  1 catch_panic
  2 request_id           header=x-request-id generator=ulid
  3 trace                level=info
  4 sensitive_headers    authorization, cookie, x-api-key
  5 catch_error
  6 timeout              30s
  7 body_limit           2 MiB
  8 normalize_path       trim_trailing_slash
  9 tenant               (custom, src/middleware/tenant.rs:12)
 10 security_headers     hsts=63072000 csp="default-src 'self'"
 11 compression          br,zstd,gzip
 12 session              store=redis ttl=14d

/api/v1/auth/*  (router-scoped)
  + rate_limit           10/min per ip
```

## `#[middleware]` - the ergonomic form

```rust
// example
#[moso::middleware]
async fn tenant(mut req: Request, next: Next) -> Result<Response> {
    let host = req.headers().get(HOST).and_then(|v| v.to_str().ok()).unwrap_or_default();
    let tenant = Tenant::from_host(host).ok_or_else(|| Error::not_found("tenant"))?;
    req.extensions_mut().insert(tenant);
    let mut res = next.run(req).await;
    res.headers_mut().insert("x-tenant", HeaderValue::from_static("…"));
    Ok(res)
}
```

Expands to a `Layer` + `Service` + a named `Clone` type. Properties:
- Returning `Err(Error)` short-circuits with a problem response - no `IntoResponse` juggling.
- The generated `Service` is `Clone` and boxed once at registration, so it does not monomorphise
  per route.
- Middleware may `Inject<T>` by taking a typed parameter:
  ```rust
  #[moso::middleware]
  async fn tenant(Inject(db): Inject<Db>, req: Request, next: Next) -> Result<Response> { … }
  ```
  Its `PROVIDER_REQ` participates in boot validation exactly like a handler's.

Middleware may **not** use `Depends<T>` - dependencies are resolved during extraction, after
middleware. The compile error says so:

```
error: `Depends<CurrentUser>` cannot be used in middleware
  = note: middleware runs before extractors, so request dependencies are not yet available
  = help: read a middleware-inserted value with `req.extensions()`, or move this logic into
          a `Dependency` impl and use it in the handler
```

## Guards - middleware that documents itself

The gap in every framework: a middleware that can return 403 makes the OpenAPI wrong, because
nothing tells the document about it. A `Guard` fixes that.

```rust
// spec
pub trait Guard: Clone + Send + Sync + 'static {
    fn describe(op: &mut OperationBuilder);
    fn check(&self, parts: &Parts, ctx: &RequestCtx) -> impl Future<Output = Result<()>> + Send;
}
```

```rust
// example
Router::new()
    .merge(admin_routes())
    .guard(RequirePermission::new(Perm::AdminAccess))   // adds 401/403 + security to every op
    .guard(RequireHeader::new("x-internal"))
```

Guards run after routing and before extraction, so they can see path parameters. Every built-in
security middleware (auth, rate limit, CSRF, permission) is a `Guard`, not a bare `Layer`.

## Per-route and per-router layers

```rust
// example
Router::new()
    .post("/auth/login", login)
    .layer(RateLimit::per_ip(10, Duration::from_secs(60)))   // applies to routes registered above
    .post("/auth/logout", logout)                            // not rate limited
```

The "applies to routes registered *before* the call" semantic is inherited from Axum/Tower and is a
known confusion. Moso mitigates it three ways: it is stated in the doc comment, `moso middleware`
prints the effective per-route stack, and `moso check` warns when a `.layer()` call is the last
statement in a router function (almost always a mistake - the author expected it to apply to
everything).

## Writing a Tower layer directly

Fully supported, documented with a complete worked example, and the escape hatch is explicit:

```rust
// example
use tower_http::set_header::SetResponseHeaderLayer;
Router::new().layer(SetResponseHeaderLayer::overriding(
    HeaderName::from_static("x-powered-by"), HeaderValue::from_static("moso")));
```

Everything in `tower-http` works: `ServeDir`, `CompressionLayer`, `CorsLayer`, `AuthLayer`,
`ValidateRequestHeaderLayer`, `RequestBodyLimitLayer`, `TimeoutLayer`, `FollowRedirect`,
`LimitLayer`, etc. The docs include a table mapping each Moso slot to the `tower-http` layer it
wraps, so users can drop to it when they need an option we did not surface.

## Performance rules

- The default stack is composed **once** at boot into a single `BoxCloneService`. Per-request cost
  of the stack is measured: target **< 3 µs** total for the default configuration on the reference
  machine (dominated by tracing span creation and header work).
- Disabling `trace` and `metrics` must be measurable and documented - some users run behind a
  service mesh that already does it.
- No layer allocates per request unless it must. `request_id` generation uses a thread-local ULID
  generator; `security_headers` uses pre-built `HeaderValue`s.

## Acceptance criteria (WP-09)

1. Default stack produces the exact ordering in the table; asserted by a test over
   `MiddlewareStack::describe()`.
2. `insert_after` / `disable` / `replace` behave as specified and are reflected in
   `moso middleware`.
3. `#[middleware]` returning `Err` yields a problem response with the right status.
4. `Depends` in middleware is a compile error with the message above (UI test).
5. A `Guard` contributes its responses to every operation on the router it is applied to.
6. Middleware overhead benchmark meets the < 3 µs target; a per-layer breakdown is published.
7. `metrics` labels use route patterns, verified by asserting the label set after hitting
   `/users/1`, `/users/2`.
