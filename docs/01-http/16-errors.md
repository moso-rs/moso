# 16 — Errors

> ✅ **Status: implemented.** One concrete `Error`, 23 response kinds, RFC 9457 `problem+json`,
> field pointers, 5xx detail suppression unless `http.expose_internal_errors`, the developer error
> page for `Accept: text/html`, and `#[derive(Error)]`.
> One representation note: `Error` is `Box<ErrorInner>`. Unboxed it is 264 bytes, and clippy's
> `result_large_err` then fires on `Result<T, Error>` — that is, on every handler in every program.
> One allocation on the failure path; the same trade `anyhow` makes.
> `Problem::errors` is `Option<Vec<ProblemField>>`, a new owned type, because `Problem` must
> round-trip through `Deserialize` and `moso_schema::ValidationErrors` is `Serialize`-only.

## Principles

1. **One concrete error type at the boundary.** Handlers return `moso::Result<T>` so `?` works
   across every battery without `map_err`.
2. **The wire format is RFC 9457** (`application/problem+json`). One shape, always, including for
   framework-generated errors (413, 404, 405, 500).
3. **Client-safe by default.** An error's `detail` is only sent to the client if its variant is
   marked safe. Everything else becomes "Internal server error" plus a `request_id` the operator
   can grep. No accidental leaking of SQL strings or file paths.
4. **Every error is logged exactly once**, at the boundary, with the full chain and a span context.
   No `tracing::error!` sprinkled through the call stack producing triplicate logs.
5. **Errors are documented.** A handler's declared error variants appear in the OpenAPI responses.

## The type

```rust
// spec — moso-core/src/error.rs

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub struct Error {
    kind: ErrorKind,
    /// Machine-readable type URI. Defaults from `kind`, overridable.
    type_uri: Cow<'static, str>,
    title: Cow<'static, str>,
    detail: Option<Cow<'static, str>>,
    /// Extra members merged into the problem document.
    extensions: BTreeMap<Cow<'static, str>, Value>,
    /// Field-level errors (validation, conflict-on-unique, etc).
    fields: Option<ValidationErrors>,
    /// Underlying cause. Logged, never serialised.
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
    /// Backtrace captured when RUST_BACKTRACE is on and kind is Internal.
    backtrace: Option<Backtrace>,
    headers: HeaderMap,
}

#[non_exhaustive]
pub enum ErrorKind {
    // 4xx — client-safe detail
    BadRequest,          // 400  malformed syntax
    Unauthenticated,     // 401
    Forbidden,           // 403
    NotFound,            // 404
    MethodNotAllowed,    // 405
    NotAcceptable,       // 406
    Conflict,            // 409  unique violation, optimistic lock
    Gone,                // 410
    PreconditionFailed,  // 412
    PayloadTooLarge,     // 413
    UriTooLong,          // 414
    UnsupportedMedia,    // 415
    RangeNotSatisfiable, // 416
    Validation,          // 422  carries `fields`
    Locked,              // 423
    TooManyRequests,     // 429  carries Retry-After
    HeaderFieldsTooLarge,// 431  too many header fields, or too many header bytes
    // 5xx — detail suppressed unless `expose_internal_errors`
    Internal,            // 500
    NotImplemented,      // 501
    BadGateway,          // 502
    Unavailable,         // 503  shutting down / dependency down
    GatewayTimeout,      // 504
    Timeout,             // 504 for our own timeout layer
    // build-time only, never a response
    Boot(BootErrors),
}
```

### Constructors (the ergonomic surface)

```rust
// spec
impl Error {
    pub fn bad_request(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn unauthenticated() -> Self;
    pub fn forbidden(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn not_found(resource: impl Into<Cow<'static, str>>) -> Self;   // "user" → "User not found"
    pub fn conflict(detail: impl Into<Cow<'static, str>>) -> Self;
    pub fn validation(errs: ValidationErrors) -> Self;
    pub fn too_many(retry_after: Duration) -> Self;
    pub fn internal(source: impl Into<BoxError>) -> Self;
    pub fn unavailable(detail: impl Into<Cow<'static, str>>) -> Self;

    // builders
    pub fn with_type(self, uri: &'static str) -> Self;
    pub fn with_title(self, t: impl Into<Cow<'static, str>>) -> Self;
    pub fn with_detail(self, d: impl Into<Cow<'static, str>>) -> Self;
    pub fn with_extension(self, k: &'static str, v: impl Serialize) -> Self;
    pub fn with_field(self, pointer: &str, code: &'static str, msg: &str) -> Self;
    pub fn with_header(self, k: HeaderName, v: HeaderValue) -> Self;
    pub fn with_source(self, e: impl Into<BoxError>) -> Self;

    pub fn kind(&self) -> &ErrorKind;
    pub fn status(&self) -> StatusCode;
    pub fn is_client_error(&self) -> bool;
}
```

## The wire format

```json
{
  "type": "https://moso.rs/errors/conflict",
  "title": "Conflict",
  "status": 409,
  "detail": "A user with this email already exists",
  "instance": "/api/v1/users",
  "errors": [ { "pointer": "/email", "code": "unique", "message": "already taken" } ],
  "request_id": "01J8XG7K3RQZ4B0N2Y6M9C5V1T",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```

- `type` defaults to `https://moso.rs/errors/{kind}` and is documented; apps override it with their
  own URI space via `Error::with_type` or `#[derive(Error)]`.
- `instance` is the request path. `request_id` and `trace_id` are always present.
- `errors` appears only when field-level detail exists.
- Content type is `application/problem+json`. For `Accept: text/html` requests (a browser hitting an
  API by accident), Moso renders a small HTML page in dev and a plain page in production — because
  a wall of JSON in a browser is a bad first impression.

## Converting your errors

### The `?` path
`Error` implements `From` for the error types of every dependency Moso owns:

| Source | Becomes | Notes |
| --- | --- | --- |
| `sqlx::Error::RowNotFound` | 404 | detail: "Not found" |
| unique violation (23505) | 409 | field pointer derived from the constraint name |
| FK violation (23503) | 409 | detail names the relationship |
| check violation (23514) | 422 | |
| serialization failure (40001) | retried by `Db::transaction`, then 409 | |
| `sqlx::Error` (other) | 500 | detail suppressed, full error logged |
| `moso_kv::Error` | 500 / 503 | |
| `serde_json::Error` | 400 | with pointer when via `serde_path_to_error` |
| `ValidationErrors` | 422 | |
| `moso_auth::Error` | 401 / 403 | |
| `std::io::Error` | 500 | |

**Important:** the sqlx-error mapping is *deliberately conservative*. Mapping `RowNotFound` to 404
is convenient and right ~90% of the time and wrong the rest; the docs show `Option`-returning query
methods (`.optional()`) as the explicit path and reserve `?`-to-404 for `find_or_404`-style helpers
whose name says what they do.

### Your own error enum

```rust
// example — src/error.rs
#[derive(Debug, moso::Error)]
pub enum ShopError {
    #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
    #[error(detail = "Only {available} left in stock")]
    OutOfStock { sku: String, available: u32 },

    #[error(status = 402, type = "https://shop.example/errors/payment-required")]
    PaymentRequired,

    #[error(status = 500)]              // detail suppressed automatically
    Gateway(#[from] reqwest::Error),
}
```

The derive generates `Display`, `std::error::Error`, `From<ShopError> for moso::Error`, **and** a
registration so `#[endpoint]` can document these responses when the handler mentions the type:

```rust
// example
#[endpoint(errors = ShopError)]     // the one place an attribute argument is genuinely needed
async fn checkout(...) -> Result<OrderOut> { … }
```

Rust cannot infer which error variants a function body can produce, so this is the honest limit of
zero-annotation. We make it one word. `moso check` warns when a handler's body constructs a
`ShopError` variant but the handler lacks `errors = ShopError` — closing most of the gap with a
lint rather than a type-system feature we don't have.

## Panics

The `CatchPanic` layer is **on by default**:
- Converts to a 500 problem response with the request id.
- Logs at `error` with the panic payload, the backtrace, and the full request context.
- Increments a `moso_panics_total` counter (labelled by route) — an alertable signal.
- In the `dev` profile, additionally renders the panic message and backtrace in the response body,
  because hunting for it in the terminal is a waste of the developer's afternoon.

We do not treat panics as normal control flow, and the docs say: a panic is a bug; the layer exists
so one bad request does not kill the connection, not so you can `unwrap()`.

## Logging policy

```
ERROR moso::http  request failed
  status=500 method=POST path=/api/v1/checkout request_id=01J8… trace_id=4bf9…
  error=ShopError::Gateway
  chain="reqwest::Error: connection refused → hyper: tcp connect error"
  user_id=usr_123 tenant=acme duration_ms=48
```

- 5xx → `ERROR` with source chain and backtrace.
- 4xx → `WARN` for 401/403/409/429, `DEBUG` for 404/422 (they are routine and would otherwise
  drown the log).
- Never log at the construction site. `Error` is a value; only the boundary logs it.
- `#[schema(secret)]` fields and any header in the redaction list (`authorization`, `cookie`,
  `x-api-key`, `set-cookie`) are redacted in every error log path. Tested.

## Retry semantics

`Error` carries `retryable()` derived from the kind (429, 502, 503, 504, and serialization
failures). `moso-jobs` uses it directly for retry decisions, and the generated clients map it to a
`retryable` boolean, so the semantic is consistent across the stack.

## The dev-mode error page

In `dev`, an unhandled 500 renders an HTML page with: the problem JSON, the error chain, the
backtrace with the user's frames highlighted, the matched route with a link to `file:line`, the
resolved dependencies, and the last 20 SQL statements from this request. This is the single
highest-leverage DX feature we can ship cheaply, and no Rust framework has it.

## Acceptance criteria (WP-06)

1. Every `ErrorKind` maps to the correct status and a stable `type` URI; snapshot-tested.
2. An `Internal` error never emits its `detail` or `source` in the response body, in any profile
   except when `http.expose_internal_errors = true`. Test greps the body for a canary string.
3. `?` on each source-error kind in the table produces the documented status.
4. Panic in a handler ⇒ 500 problem, connection stays alive, counter increments, next request OK.
5. Secrets do not appear in error logs: a test constructs an error containing a `Password` and
   asserts the log line is clean.
6. `#[derive(moso::Error)]` output appears in the OpenAPI responses when `errors = ` is set.
7. The dev error page renders for a panicking and a `?`-returning handler; it is absent in release.
