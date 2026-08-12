---
title: Errors
description: Return errors from handlers as RFC 9457 problem documents, attach field pointers and context, derive an application taxonomy, and control what a 5xx is allowed to say.
order: 7
status: shipped
---

Every Moso handler returns `moso::Result<T>`, which is `Result<T, moso::Error>`. One concrete error
type sits at the boundary, so `?` works from an extractor, from the ORM, from a battery and from your
own domain code without a `map_err` in sight. That one type carries a taxonomy, and the taxonomy is
what decides the HTTP status, the `type` URI a client branches on, the log level, and whether the
detail is allowed to reach the wire at all.

Every error becomes an RFC 9457 `application/problem+json` document. Framework errors and your errors
have the same shape, so a client writes one error parser. This page covers constructing errors,
refining them, pointing at the field that broke, converting errors from other crates, deriving your
own taxonomy, the disclosure rule that keeps a 500 from naming your database host, and the HTML page
a browser gets instead of a wall of JSON.

Nothing here needs a cargo feature and nothing here needs a service running. The whole subsystem
lives in `moso-core`, which the facade always depends on. One hole is marked inline and listed again
under [failure modes and gaps](#failure-modes-and-gaps): the documents the router builds without the
middleware in play carry no `request_id`, `trace_id` or `instance`.

## The smallest working example

A handler returns `Result<T>` and uses `?` or an explicit `Err`.

```rust title="src/routes/posts.rs"
#[endpoint(errors = BlogError)]
async fn show(
    Inject(store): Inject<Store>,
    Depends(actor): Depends<Actor>,
    Path(id): Path<Id<Post>>,
) -> Result<Json<PostOut>> {
    let post = store.get(id)?;
    if !may_read(&post, &actor) {
        return Err(BlogError::post_not_found(id).into());
    }
    Ok(Json(post.into()))
}
```

Or with the framework taxonomy directly, no derive involved:

```rust
use moso::Error;

let error = Error::conflict("A user with this email already exists")
    .with_field("/email", "unique", "already taken");

assert_eq!(error.status(), 409);

// On the wire it is an RFC 9457 `application/problem+json` document.
let response = moso::IntoResponse::into_response(error);
assert_eq!(response.headers()["content-type"], "application/problem+json");
```

`Error` and `Result` are in the prelude. `ErrorKind` and `Problem` sit on the facade root
(`moso::ErrorKind`, `moso::Problem`) but not in the prelude, so import those where you use them. The
rest of the surface lives under `moso::error`, `moso::error::problem` and `moso::error::boot`.

## What the client receives

That error, rendered inside a request, produces this body with
`Content-Type: application/problem+json`:

```json
{
  "type": "https://moso.rs/errors/conflict",
  "title": "Conflict",
  "status": 409,
  "detail": "A user with this email already exists",
  "instance": "/api/v1/users",
  "errors": [{ "pointer": "/email", "code": "unique", "message": "already taken" }],
  "request_id": "01J8XG7K3RQZ4B0N2Y6M9C5V1T",
  "trace_id": "4bf92f3577b34da6a3ce929d0e0e4736"
}
```

| Member | Type | When it is present |
| --- | --- | --- |
| `type` | string | Always. A stable URI. This is what a client should branch on, not the status. |
| `title` | string | Always. The human name of the kind, or the title you set. |
| `status` | integer | Always. Repeated in the body, as RFC 9457 requires. |
| `detail` | string | On every 4xx. On a 5xx only when disclosure is turned on. |
| `instance` | string | When rendered inside the `catch_error` layer. The request path. |
| `errors` | array of `{pointer, code, message, params}` | When field errors exist, and the status is not a suppressed 5xx. |
| `request_id` | string | When `x-request-id` was present or assigned. |
| `trace_id` | string | When a valid W3C `traceparent` was present. |
| `chain` | string | Only on a 5xx with disclosure on. The source chain joined into one line. |
| anything else | any | Extension members you added with `with_extension`, flattened into the object. |

The wire type is `moso::Problem`, built by `Problem::from_error(&error, &options)`. Unlike
`moso::schema::ValidationErrors` it is both `Serialize` and `Deserialize`, so a document round-trips
and a test or a client can parse one back into a typed value:

```rust
use moso::prelude::*;
use moso::error::problem::Problem;

let error = Error::not_found("post");
let problem = Problem::from_error(&error, &Default::default());

assert_eq!(problem.status, 404);
assert!(problem.type_uri.starts_with("https://"));

// It round-trips, which is what lets a test or a client parse one back.
let json = serde_json::to_string(&problem).unwrap();
let parsed: Problem = serde_json::from_str(&json).unwrap();
assert_eq!(parsed, problem);
```

`Problem::to_bytes()` is infallible. If an extension member somehow refuses to serialise it falls
back to a hand-written document rather than an empty body, because the answer to "we could not render
the error" still has to be a valid problem document. `Problem::status_code()` falls back to 500 when
a deserialised document carries a status that is not a valid code.

## The response kinds

`ErrorKind` is the taxonomy. Every kind fixes a status, a slug, a `type` URI under
`https://moso.rs/errors/`, whether the client may retry, and the level the boundary layer logs at.

| Kind | Status | `type` suffix | Constructor | Retryable | Log level |
| --- | --- | --- | --- | --- | --- |
| `BadRequest` | 400 | `bad-request` | `Error::bad_request(detail)` | no | DEBUG |
| `Unauthenticated` | 401 | `unauthenticated` | `Error::unauthenticated()` | no | WARN |
| `Forbidden` | 403 | `forbidden` | `Error::forbidden(detail)` | no | WARN |
| `NotFound` | 404 | `not-found` | `Error::not_found(resource)` | no | DEBUG |
| `MethodNotAllowed` | 405 | `method-not-allowed` | `Error::method_not_allowed(&[..])` | no | DEBUG |
| `NotAcceptable` | 406 | `not-acceptable` | `Error::new(..)` | no | DEBUG |
| `Conflict` | 409 | `conflict` | `Error::conflict(detail)` | no | WARN |
| `Gone` | 410 | `gone` | `Error::new(..)` | no | WARN |
| `PreconditionFailed` | 412 | `precondition-failed` | `Error::new(..)` | no | DEBUG |
| `PayloadTooLarge` | 413 | `payload-too-large` | `Error::payload_too_large(limit)` | no | DEBUG |
| `UriTooLong` | 414 | `uri-too-long` | `Error::uri_too_long(limit)` | no | DEBUG |
| `UnsupportedMedia` | 415 | `unsupported-media` | `Error::unsupported_media(ct)` | no | DEBUG |
| `RangeNotSatisfiable` | 416 | `range-not-satisfiable` | `Error::new(..)` | no | DEBUG |
| `Validation` | 422 | `validation` | `Error::validation(errors)` | no | DEBUG |
| `Locked` | 423 | `locked` | `Error::new(..)` | no | WARN |
| `TooManyRequests` | 429 | `too-many-requests` | `Error::too_many(retry_after)` | yes | WARN |
| `HeaderFieldsTooLarge` | 431 | `header-fields-too-large` | `Error::too_many_headers(limit)`, `Error::headers_too_large(limit)` | no | DEBUG |
| `Internal` | 500 | `internal` | `Error::internal(source)`, `Error::internal_msg(detail)` | no | ERROR |
| `NotImplemented` | 501 | `not-implemented` | `Error::new(..)` | no | ERROR |
| `BadGateway` | 502 | `bad-gateway` | `Error::new(..)` | yes | ERROR |
| `Unavailable` | 503 | `unavailable` | `Error::unavailable(detail)` | yes | ERROR |
| `GatewayTimeout` | 504 | `gateway-timeout` | `Error::new(..)` | yes | ERROR |
| `Timeout` | 504 | `timeout` | `Error::timeout(after)` | yes | ERROR |

Eight kinds have no named constructor, because they take no interesting argument. Build those with
`Error::new` and a detail:

```rust
use moso::{Error, ErrorKind};

let error = Error::new(ErrorKind::Gone).with_detail("This export expired on 2026-01-01");
```

A twenty-fourth variant, `ErrorKind::Boot(BootErrors)`, exists for the boot report and also maps to
500. It never reaches a listener in practice, because the process exits before the socket binds.
`ErrorKind::RESPONSE_KINDS` is the 23-entry slice without it.

A few constructors do more than set a detail:

- `Error::not_found("user")` renders `"User not found"`, capitalising the resource with Unicode
  rules. An empty string gives `"Not found"`.
- `Error::method_not_allowed(&[Method::GET, Method::POST])` sets `Allow: GET, POST` and names them in
  the detail. An empty slice sets no header.
- `Error::payload_too_large(1024)` adds an extension member `max_bytes` of `1024`.
- `Error::uri_too_long(2048)` and `Error::headers_too_large(16384)` report their limit the same way,
  as `max_bytes`; `Error::too_many_headers(64)` reports `max_count`. All three are what the
  [request-limits layer](./middleware.md) answers with, and the number in the document is the
  operator's own `http.uri_max`, `http.header_max_bytes` or `http.header_max_count`. A client
  cannot discover it any other way, and "too long, by an amount I will not tell you" is not
  actionable. `too_many_headers` and `headers_too_large` share the 431 kind but stay separate
  constructors, because one says "send fewer headers" and the other "send smaller ones".
- `Error::too_many(Duration::from_millis(1500))` rounds up to whole seconds, because rounding down
  invites the client back before the window closes. You get `Retry-After: 2`, an extension
  `retry_after` of `2`, and the detail `"Rate limit exceeded; retry in 2s"`.
- `Error::validation(errors)` writes the detail from the count: `"1 field failed validation"`,
  `"3 fields failed validation"`, and `"The request did not pass validation"` for an empty set.
- `Error::internal(source)` stores `source.to_string()` as the detail even though a 5xx suppresses
  it, so the log line and the developer page have something to print. `Error::internal_msg(detail)`
  is the same kind with a message and no source.
- `Error::timeout(Duration::from_secs(30))` renders `"The request exceeded the 30s timeout"`.
- `Error::unauthenticated()` takes no argument and sets no `WWW-Authenticate` header. Add the
  challenge yourself with `with_header` if the scheme calls for one.
- `Error::new` captures a backtrace only for `ErrorKind::Internal`, and only when `RUST_BACKTRACE` is
  set. Every other kind is a decision the code made on purpose, and its stack says nothing the route
  and the detail do not already say.

> [!NOTE]
> `ErrorKind` is `#[non_exhaustive]`. New kinds can arrive in a minor release, so any `match` over it
> needs a `_` arm.

## Refining an error

Every builder consumes and returns `Self`, so they chain.

| Builder | Effect |
| --- | --- |
| `with_type(&'static str)` | Replaces the `type` URI. Use it to publish your own URI space. |
| `with_title(impl Into<Cow<'static, str>>)` | Replaces the title. |
| `with_detail(impl Into<Cow<'static, str>>)` | Sets the detail. Still suppressed on a 5xx. |
| `with_extension(&'static str, impl Serialize)` | Adds a flattened member to the document. |
| `with_field(&str, &'static str, &str)` | Adds one field error: pointer, code, message. |
| `with_fields(ValidationErrors)` | Merges a whole set of field errors into the existing ones. |
| `with_header(HeaderName, HeaderValue)` | Appends a response header. Appending twice keeps both values. |
| `with_source(impl Into<BoxError>)` | Sets `std::error::Error::source`, which is what `chain()` walks. |

Reading an error back is symmetrical: `kind()`, `status()`, `type_uri()`, `title()`, `detail()`,
`fields()`, `extensions()`, `headers()`, `backtrace()`, plus `is_client_error()`,
`is_server_error()`, `retryable()` and `chain()`. `Display for Error` writes the title, then the
detail after a colon. `Debug for Error` is operator-facing and prints the whole structure, except for
`ErrorKind::Boot`, which prints the grouped boot report instead.

### Three shapes you will write often

A quota refusal, with the machine-readable numbers a client needs to back off correctly:

```rust
use moso::{Error, ErrorKind};

let error = Error::new(ErrorKind::TooManyRequests)
    .with_type("https://shop.example/errors/quota-exhausted")
    .with_title("Monthly quota exhausted")
    .with_detail("This key has used all 10000 calls for January")
    .with_extension("quota", 10_000u32)
    .with_extension("resets_at", "2026-02-01T00:00:00Z");
```

A 401 that tells the client which schemes it may try. `with_header` appends rather than replaces, so
two challenges both survive into the response:

```rust
use http::header::{HeaderValue, WWW_AUTHENTICATE};
use moso::Error;

let error = Error::unauthenticated()
    .with_header(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"api\""))
    .with_header(WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"api\""));
```

A conflict that carries the identity of the thing that collided, so the client can link to it:

```rust
use moso::Error;

let error = Error::conflict("A post already uses this slug")
    .with_field("/slug", "unique", "already taken")
    .with_extension("conflicting_id", existing.id.to_string());
```

### Constraints that will bite you

- `with_type` takes `&'static str` only. There is no owned overload, so a `type` URI cannot be
  computed at run time. `with_title` and `with_detail` do accept owned strings.
- `with_extension` keys and `with_field` codes are `&'static str` too. A computed code has to go
  through `ValidationErrors` and `FieldError::new`, which accept `impl Into<Cow<'static, str>>`.
- `with_extension` drops the member if the value fails to serialise, rather than panicking or failing
  the whole response.
- `retryable()` reads the kind, not the individual error. A 409 your domain considers retryable still
  answers `false`. Publish an extension member if you need to say otherwise, which is what the ORM
  does for a serialization failure. The extension and `Error::retryable()` are different things and
  documentation that conflates them is wrong.

> [!WARNING]
> Extension members are flattened into the top level of the document. An extension named `type`,
> `title`, `status`, `detail`, `instance`, `errors`, `request_id`, `trace_id` or `chain` collides with
> a reserved member, and nothing guards against it. Prefix your extension keys if you are unsure.

## Field pointers

A field error is `{pointer, code, message, params}`. The pointer is an RFC 6901 JSON Pointer into the
request body, so a client can highlight the exact input that broke. The code is a machine token from
`moso::schema::codes` (`required`, `type`, `len`, `range`, `pattern`, `format`, `enum`, `unique`,
`multiple_of`, `custom:*`) or one of your own. `params` carries whatever numbers the check wants to
publish, such as the limit a length check enforced, and is omitted when empty.

For a single field, `with_field` is enough. For anything a helper produces, build `ValidationErrors`:

```rust title="src/routes/posts.rs"
use moso::schema::{ValidationErrors, codes};

fn decode_cursor(cursor: &Cursor) -> Result<PostKey> {
    PostKey::from_cursor(cursor).ok_or_else(|| {
        Error::validation(ValidationErrors::one(
            CURSOR_POINTER,
            codes::FORMAT,
            "this is not a cursor this API issued; start from the first page",
        ))
    })
}
```

`ValidationErrors` and `codes` live in `moso::schema`, not the prelude. A set holds at most
`moso::schema::DEFAULT_MAX_ERRORS` (50) retained errors; pushes past the cap are counted by
`dropped()` and discarded, so a pathological body cannot turn one response into a megabyte. The
dropped count does not appear in the problem document, only the retained errors do.

Deserialisation failures get their pointer for free. The `Json` and `Form` extractors run through
`serde_path_to_error`, and `Error::from_json_path` turns the path into a pointer, including through
arrays and maps, with RFC 6901 escaping:

```rust
#[derive(Debug, serde::Deserialize)]
struct Line { quantity: u32 }
#[derive(Debug, serde::Deserialize)]
struct Order { items: Vec<Line> }

let json = br#"{"items":[{"quantity":1},{"quantity":2},{"quantity":"three"}]}"#;
let deserializer = &mut serde_json::Deserializer::from_slice(json);
let path_error = serde_path_to_error::deserialize::<_, Order>(deserializer).unwrap_err();

let error = Error::from_json_path(path_error);
assert_eq!(error.status(), StatusCode::BAD_REQUEST);
let fields = error.fields().expect("field errors");
assert_eq!(fields.as_slice()[0].pointer, "/items/2/quantity");
assert_eq!(fields.as_slice()[0].code, "type");
// The `at line N column M` suffix is noise beside a JSON Pointer.
assert!(!fields.as_slice()[0].message.contains("at line"));
```

`Error::from_form_path` is the same thing for `serde::de::value::Error`, which is what form decoding
produces. In both cases the trailing `at line N column M` is stripped from the message, because the
pointer already says where.

Three classification rules apply on top of that:

1. A missing required field is reported at the field's own pointer with code `required`, not at the
   containing object. `serde_path_to_error` stops at the container because the member was never
   visited, so Moso reads the name out of serde's message and extends the pointer itself. If serde
   ever rewords that message the fallback keeps the honest container pointer rather than guessing.
2. A failure raised by a constrained type (`Email`, `Slug`, and the rest) becomes a **422** carrying
   the constraint's own code rather than a generic 400. Being caught during deserialisation instead
   of after it is an implementation detail the client should not see.
3. A `serde_json` failure in the I/O category becomes a **500**, because reading the socket failed
   rather than the client sending nonsense.

Everything else is a 400 with code `type`. See [validation](./validation.md) for how a
`#[derive(Schema)]` type produces these without you writing any of it.

## Attaching context

`instance`, `request_id` and `trace_id` are filled in for you. The `catch_error` layer reads
`x-request-id` and the W3C `traceparent` header off the request, builds an `ErrorContext`, and runs
the whole inner stack inside it. That context is a `tokio::task_local!`, not a thread-local, so a
future that migrates between worker threads keeps its own ids rather than borrowing another
request's. Only a 32-character hex trace field is accepted from `traceparent`; anything else is
dropped rather than echoed.

Any `Error` rendered inside that scope, whether it came from an extractor, a guard, a dependency or
your handler, picks the context up automatically. You never call anything.

Outside a request, or in a unit test that calls a handler function directly,
`current_error_context()` returns `None` and rendering falls back to production options with no ids.
Install one by hand when a test needs them:

```rust
use moso::error::problem::{ErrorContext, ProblemOptions, with_error_context};

let context = ErrorContext::new(ProblemOptions::default())
    .with_request_id("01J8XG7K3RQZ4B0N2Y6M9C5V1T")
    .with_trace_id("4bf92f3577b34da6a3ce929d0e0e4736");

// `body_string` here is a local test helper that drains the response body.
let response =
    with_error_context(context, async { Error::not_found("user").into_response() }).await;

let json: serde_json::Value =
    serde_json::from_str(&body_string(response).await).expect("valid json");
assert_eq!(json["request_id"], "01J8XG7K3RQZ4B0N2Y6M9C5V1T");
assert_eq!(json["trace_id"], "4bf92f3577b34da6a3ce929d0e0e4736");
```

`ErrorContext::with_parts(&parts)` fills `instance` from the request path and `prefers_html` from
`Accept` in one call, which is what the layer itself uses.

There is exactly one rendering pass. `IntoResponse for Error` renders under whatever context is in
scope and stores an `Arc<Error>` in the response extensions; `catch_error` reads that `Arc` on the
way out to log it, and does not re-render. Re-rendering would discard headers an inner layer had
already set. `IntoResponse` also takes no arguments and is called directly by macro-generated handler
glue on an extraction failure, so it has to be correct in isolation. Making it conservative in
isolation and complete inside the layer is the arrangement where a missing context can only ever
disclose less, never more.

## Converting foreign errors

`?` reaches `moso::Error` from all of these without a `map_err`:

| From | Becomes | Notes |
| --- | --- | --- |
| `ErrorKind` | that kind | |
| `moso::schema::ValidationErrors` | 422 | |
| `serde_json::Error` | 400, or 500 in the I/O category | no pointer |
| `serde_path_to_error::Error<serde_json::Error>` | 400 or 422 | with pointer |
| `serde_path_to_error::Error<serde::de::value::Error>` | 400 or 422 | form decoding |
| `moso::schema::ConstraintError` | 422 at pointer `""` | |
| `std::io::Error` | 500 | |
| `http::Error` | 500 | |
| `axum::Error` | 400 | a body stream that failed mid-read |
| `std::str::Utf8Error`, `std::string::FromUtf8Error` | 400 | |
| `std::num::ParseIntError`, `std::num::ParseFloatError` | 400 | |
| `moso::error::boot::BootErrors` | 500 boot | |

The batteries own their own direction. `moso-core` has no `sqlx` dependency at all; instead
`moso-orm` implements `From<moso_orm::Error> for moso::Error`, and the mapping is richer than a
status table:

| Data-layer failure | Becomes |
| --- | --- |
| `NotFound` | 404 naming the entity |
| unique violation | 409, plus a field error at the offending pointer with code `unique` |
| foreign key violation | 422, code `foreign_key` |
| not null violation | 422, code `required` |
| check violation | 422, code `invalid` |
| stale write, serialization failure, deadlock | 409 with an extension `retryable: true` |
| pool timeout | 503 with `Retry-After: 1` and `retryable: true` |
| statement timeout | 504 |
| connection failure | 503 |
| cursor decode failure | 400, since the client sent the cursor |
| a column that will not decode, a missing tenant, anything else | 500 |

A foreign key, not null or check violation is a 422 rather than a 400 on purpose: the request parsed,
and the value it named does not exist. A 400 would claim the syntax was wrong, which it was not.

Other batteries follow the same pattern. `moso-kv` passes an embedded HTTP error straight through, so
a 404 that went in comes back out as a 404. `moso-mail` turns a suppressed or invalid address into a
422 with a `/to` pointer and a bad webhook signature into a 401. `moso-storage` turns a missing object
into a 404, a rejected content type into a 422 with a `/file` pointer, and an oversize upload into a
413. `moso-jobs` converts in both directions and branches on `Error::retryable()` to decide whether a
failed job is worth another attempt.

For an error that has no mapping, wrap it. `Error::internal(source)` takes anything that is
`Into<BoxError>` and keeps it as the source; `with_source` attaches one to an error you already
built. `chain()` walks the whole source chain into one string, which is what the 5xx log line
carries.

### Authentication failures

`moso-auth` converts in this direction too, so `?` on a `moso_auth::Result` works inside a handler
returning `moso::Result`. The crate is behind the facade's `auth` feature, which is off by default.

Every conversion runs `Error::client_facing()` first. That fold collapses `InvalidCredentials`,
`Expired`, `Revoked` and `Ceremony` into one variant, so no response distinguishes "no such account"
from "wrong password" from "your session was revoked" from "your `state` parameter did not match".
The specific variant survives for the log, which is the only place it is allowed to exist.

| Authentication failure | Becomes |
| --- | --- |
| `InvalidCredentials`, `Unauthenticated`, `Expired`, `Revoked`, `Ceremony` | 401 with `WWW-Authenticate: Bearer` |
| `SecondFactorRequired` | the same 401 plus a `challenge` extension member holding the partial-authentication token |
| `RateLimited` | 429 through `Error::too_many`, so `Retry-After` is rounded up to whole seconds |
| `PasswordPolicy` | 422 with one field error at pointer `/password`, and the explanation as the `detail` |
| `Unavailable` | 503, retryable, with the transport error kept as the source |
| `Config` | 500 whose detail names the misconfiguration and is suppressed on the wire |

`Bearer` is the challenge on every 401, deliberately: a session cookie is not an HTTP authentication
scheme, and `Basic` would make a browser open its own credential dialog over an API response.

A `PasswordPolicy` carries a stable code, and that code becomes the field error's code directly when
`moso::schema::codes` already spells it. `len` is the only one that does. Everything the auth crate
invents is namespaced, so `breached`, `weak`, `reused` and `banned` arrive as `custom:breached` and
so on, rather than squatting on a token a later `moso-schema` release might define.

`Unavailable` is the one arm that bypasses the collapse. It is consumed by value rather than cloned,
because `client_facing()` cannot clone a boxed source and the source is the only record of *why* a
store was unreachable. Nothing is disclosed by keeping it: a 5xx renders neither its detail nor its
`chain` without `expose_internal_errors`, so the transport's message reaches the log and not the
client.

The errors reaching `moso-auth` are mapped with the same care. `From<moso_kv::Error>` is always
`Unavailable`, so a session-store outage is a 503 rather than a silent logout, and
`From<moso_orm::Error>` turns a `NotFound` into `InvalidCredentials` rather than a 404 that would
confirm the account does not exist. Every other data-layer failure becomes `Unavailable`.

## An application taxonomy

`#[derive(moso::Error)]` maps an enum onto problem documents. One `type_base` on the container gives
every variant a URI derived from its kebab-case name.

```rust title="src/error.rs"
use moso::prelude::*;

/// Everything the blog can refuse to do.
#[derive(Debug, moso::Error)]
#[error(type_base = "https://moso.example/errors/")]
pub enum BlogError {
    /// No post has that identifier.
    #[error(status = 404, detail = "No post with id {id}")]
    PostNotFound {
        /// The identifier that was asked for.
        id: String,
    },

    /// Two posts would end up with the same slug.
    #[error(status = 409, detail = "The slug `{slug}` is already taken")]
    SlugTaken {
        /// The slug that collided.
        slug: String,
    },

    /// A PATCH that asked for nothing.
    #[error(status = 422, detail = "Provide at least one field to change")]
    NothingToUpdate,
}
```

`PostNotFound` serialises with `"type": "https://moso.example/errors/post-not-found"`.

Attribute keys on the container:

| Key | Value | Meaning |
| --- | --- | --- |
| `status` | integer literal | Fallback status for every variant that does not set one. |
| `type` | string literal | The `type` URI. Only meaningful on a struct. |
| `title` | string literal | The title. |
| `detail` | string literal | The detail template. |
| `type_base` | string literal | Prefix. Each variant without a `type` gets this plus its kebab-case name. |

On a variant: `status`, `type`, `title`, `detail`. Multiple `#[error(...)]` attributes on one variant
are merged, so you can split a long list over lines. On a field, `#[from]` generates `From<FieldType>`
and makes the field the source (the variant must have exactly one field), and `#[source]` makes the
field the source without generating the conversion (at most one per variant).

```rust
/// The failures this application's domain can produce.
#[derive(Debug, moso::Error)]
pub enum ShopError {
    /// Not enough stock to satisfy the order.
    #[error(status = 409, type = "https://shop.example/errors/out-of-stock")]
    #[error(detail = "Only {available} left in stock")]
    OutOfStock {
        /// How many remain.
        available: u32,
    },

    /// The payment could not be taken.
    #[error(status = 500)]
    Payment(#[from] PaymentError),
}

let error: Error = ShopError::OutOfStock { available: 2 }.into();
assert_eq!(error.status(), 409);
// `?` works from any handler, because `From` reaches `moso::Error`.
let from_source: ShopError = PaymentError.into();
assert_eq!(Error::from(from_source).status(), 500);
```

Detail templates interpolate variant fields: `{field}` for named fields, `{0}` for tuple indices,
`{value:.2}` for a format spec, `{{` and `}}` for literal braces. An unknown placeholder is a compile
error with a "did you mean" suggestion, and an unbalanced brace is a single compile error rather than
a cascade.

The derive generates `Display`, `core::error::Error` with `source()`, one `From<Inner>` per `#[from]`
field, `From<MyError> for moso::Error`, a `Describe` impl for the OpenAPI document, and a `VARIANTS`
const plus a `variants()` accessor holding `(name, status, type URI, title)` for tooling.

Things to know about it:

- A variant with no `status` gets **500**. A variant with no `title` gets the sentence-cased variant
  name. A variant with no `type` and no `type_base` gets `https://moso.rs/errors/<kind-slug>`.
- Twenty-one statuses are accepted: 400, 401, 403, 404, 405, 406, 409, 410, 412, 413, 414, 415, 416,
  422, 423, 429, 500, 501, 502, 503, 504. Anything else, including 402, is a compile error naming the
  nearest supported status and listing the whole set. There is no `Error::with_status`, so a status
  the kind cannot express cannot be produced at all, and rounding it down would lie on the wire.
  431 is the one kind in the taxonomy this list omits: it is produced by the request-limits layer
  from the operator's configuration, never by an application error type, so there is nothing for a
  variant to say with it.
- For a 5xx variant the derive does not attach the detail template at all, rather than attaching it
  and suppressing it at render time. A detail naming an internal host cannot then be reached by an
  operator turning disclosure on to debug something unrelated. The text still reaches the log, as the
  error's source.
- Generic types, empty enums, two `#[source]` fields on one variant, and `#[from]` on a multi-field
  variant are all rejected. Every rejection still emits placeholder impls, so a `?` elsewhere in your
  crate does not produce a second cascade of errors on top of the first.

Declare the taxonomy on the handler with `#[endpoint(errors = BlogError)]` and its statuses appear in
the generated [OpenAPI](./openapi.md) operation. The argument may be repeated for a handler that can
produce more than one taxonomy. Rust cannot infer which variants a function body constructs, so this
one word is the honest limit of zero annotation.

## What a 5xx will not say

`Problem::from_error` is the only place the disclosure rule is applied, and it is the same rule in
every profile: when the status is 5xx and `expose_internal_errors` is off, the `detail`, the field
errors and the `chain` member are all dropped. Extension members survive, because those are values
you explicitly chose to publish, and suppressing them would make `with_extension` silently useless on
the one class of error people most want to annotate.

```rust
const CANARY: &str = "SECRET_TABLE_users_password_hash";

// Default: nothing leaks.
let error = Error::internal(std::io::Error::other(CANARY));
let body = body_string(error.into_response()).await;
assert!(!body.contains(CANARY));
let json: serde_json::Value = serde_json::from_str(&body).expect("valid json");
assert_eq!(json["status"], 500);
assert_eq!(json["title"], "Internal Server Error");
assert!(json.get("detail").is_none());
assert!(json.get(CHAIN_MEMBER).is_none());
```

No profile flips this on. `HttpConfig::expose_internal_errors` is `false` in `dev`, `test` and
`production` alike, and it is the one switch a profile is not allowed to set for you. Turning it on
is a deliberate act at the composition root:

```rust title="src/main.rs"
use moso::http_config::HttpConfig;

let app = App::new(config).http_config(HttpConfig {
    expose_internal_errors: true,
    ..HttpConfig::default()
});
```

With the flag on, the same error renders `"detail": "SECRET_TABLE_users_password_hash"` plus a
`chain` member containing the whole source chain.

`HttpConfig::warn_if_disclosing` logs a `tracing::warn!` at target `moso::config` when the flag is on
outside `dev`. It warns rather than refusing, on the grounds that a framework which refuses is a
framework people patch out, and a staging environment during an incident is a legitimate use. See
[configuration](./configuration.md) for how the key is loaded.

To override the policy in code, past both the config key and the profile, reach for the middleware
slot:

```rust
let app = App::new(config).with_middleware(|s| {
    s.catch_error(|c| {
        c.problem.expose_internal_errors = true;
        c.log_headers = true;
    });
});
```

A setting made through `catch_error` is recorded as explicit, and `MiddlewareStack::configure` (which
runs later, inside `build()`) will not overwrite it.

> [!IMPORTANT]
> `Error::detail()` returns the pre-disclosure value. It hands you the detail even for a 500. A test
> asserting "the client cannot see this" has to assert against the rendered bytes, not against
> `error.detail()`.

A `SecretString` redacts itself at the `Debug` boundary, so even with disclosure on, a secret held in
a config value does not reach the body. See [security](./security.md).

## The developer error page

When a client's `Accept` header genuinely prefers `text/html`, the same error renders as an HTML page
with `Content-Type: text/html; charset=utf-8` instead of JSON. A wall of JSON in a browser is a bad
first impression.

Negotiation is strict. `text/html` has to *strictly* outrank the better of `application/json` and
`application/problem+json`, following RFC 9110 specificity so that a more specific range wins even
when a broader range carries a higher `q`. That makes `text/html, */*;q=0.9` mean what a browser
means by it. A tie goes to JSON, which is what stops an API client sending `*/*` from ever getting
HTML, and a missing or unparsable `Accept` gives JSON. `prefers_html(&request)` and
`parts_prefer_html(&parts)` answer the question directly if your own code needs it.

Every page carries the status, the title, the detail (or a generic sentence and the request id when
the detail was suppressed), a definition list of path, request id, trace id and `type`, and a table of
field errors when there are any.

The page gets its verbose half only when the profile is `dev` **and** disclosure is on for that
status. `dev` alone is not enough for a 5xx, because "an internal error's source never reaches the
body without the flag" includes `dev`. The verbose half adds:

- the error chain,
- the backtrace, with your own crate's frames emboldened and framework, runtime and `std` frames left
  plain,
- the pretty-printed problem document,
- a footer saying the page is rendered because the profile is `dev`.

The page is self-contained: the stylesheet is inlined, there is no script, no `<link>`, no font and
no external URL, and every interpolated value is HTML-escaped. It renders with no network at all,
which matters because it is the last thing that runs before a client sees a failure. It also carries
`<meta name="robots" content="noindex">`.

Turn it off, and serve JSON to browsers too, through the same slot:

```rust
let app = App::new(config).with_middleware(|s| {
    s.catch_error(|c| c.problem.html_errors = false);
});
```

> [!NOTE]
> The design documents promised the dev page would also list the resolved dependencies, the last 20
> SQL statements from the request, and a `file:line` link to the matched route. None of that is
> built. The page shows the request path, not the matched route pattern.

## Logging, counting and observing

`catch_error` emits exactly one structured line per request, at target `moso::http` with message
`request`, whatever the outcome. An error logged at construction, again where it is wrapped, and
again at the boundary produces three lines that read like three incidents. `Error` is a value; the
layer is the event. Because the line is emitted for a 201 as well as a 500, an access log is a filter
over these records rather than a second layer. The fields are fixed: `status`, `method`, `route`,
`path`, `duration_ms`, `request_id`, `error`, `headers`. You do not write a `tracing::error!` for a
request failure.

The level comes from the kind: 5xx at ERROR, 401, 403, 409, 410, 423 and 429 at WARN because they are
worth noticing in aggregate, every other 4xx at DEBUG because 404 and 422 are routine and at INFO
they drown the log, and everything else at INFO. When a response carries no `Error` behind it, the
layer falls back to `level_for(status)`, which applies the same split to the bare status.

The `error` field is `Error::chain()` for a 5xx and `Error::to_string()` otherwise. Field errors are
never logged at all, because a validation message can quote the value it rejected and the value is
exactly what must not be logged.

| Knob | What it does |
| --- | --- |
| `CatchErrorConfig::log_headers` | Adds a redacted header dump. Filtered to 5xx only. Off by default. |
| `CatchErrorConfig::count` | Whether failures increment the counter. On by default. |
| `CatchErrorConfig::problem` | The `ProblemOptions` this layer renders with. |
| `MiddlewareStack::silence(path)` | Skips the line entirely for that path prefix. |

Redaction is structural: a header is redacted by **name**, against a fixed list, and replaced with
`[redacted]`. Never by pattern-matching the value, because a regex over a value is both slower and
wrong, since it misses the secret that does not look like one.

`configure` silences `health_path`, `ready_path`, `docs_path` and `openapi_path` for you. A silenced
path logs nothing at all, not just no errors, and matching is by prefix.

Read the counters with `moso::middleware::catch_error::failed_requests_total()` (published under the
metric name `moso_requests_failed_total`) and `moso::middleware::catch_panic::panics_total()`
(`moso_panics_total`).

> [!WARNING]
> `failed_requests_total` counts 4xx as well as 5xx: the condition is
> `is_client_error() || is_server_error()`, so a burst of 404s moves it. An alert built on it needs a
> rate comparison, not an absolute threshold.

Your own middleware should read the error out of the response extensions rather than parsing the
body:

```rust
use moso::error::problem::ErrorRef;

// Inside a layer, on the way out:
if let Some(error) = response.extensions().get::<ErrorRef>() {
    // `error` is an `Arc<moso::Error>`; read the taxonomy rather than the body.
    metrics.record(error.kind().slug(), error.status().as_u16());
}
```

See [observability](./observability.md) for the tracing setup this line lands in, and
[middleware](./middleware.md) for where `catch_error` sits in the slot order.

## Panics

`catch_panic` is the outermost slot. A panic in a handler becomes a 500 problem document carrying
whatever request id the inner stack managed to assign, logged at ERROR with the payload and the path,
counted behind `moso_panics_total`, and the connection stays up.

The panic message reaches the response body only when `render_details` is on, which
`CatchPanicConfig::for_profile` sets in `dev` and nowhere else. The body is built directly as a
`Problem`, not through `Error`, because at that point the inner stack has already unwound.

The layer deliberately does not capture a backtrace. By the time `catch_unwind` returns, the stack
between the panic and the layer has unwound, so a backtrace taken at the catch site describes the
wrong frames. The useful one is what the panic hook prints at the panic site, which still happens.

`MiddlewareStack::bare()` removes both `catch_error` and `catch_panic`. A handler's `Error` still
renders through `IntoResponse`, conservatively and with no ids, but nothing logs it, nothing counts
it, and a panic takes the connection with it.

## Boot errors

`AppBuilder::build()` collects every boot problem into one `BootErrors` rather than stopping at the
first, sorts them (missing providers first, then provider cycles and failures, then route problems,
then configuration, then document problems, then middleware ordering, then anything else), and
returns an `Error` whose kind is `ErrorKind::Boot`. Because `fn main() -> Result<(), Error>` prints
its error with `Debug`, and `Debug for Error` special-cases the boot kind, what you see is a report:

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

The variants cover missing, cyclic and failing providers, route conflicts, path parameter mismatches,
legacy path syntax, duplicate operation ids, schema collisions, missing and invalid configuration
keys, document problems, and middleware ordering. `MiddlewareStack::validate` is what produces the
last of those, when `catch_error` is not inside `trace`, or not outside `timeout`, or `metrics` is
not innermost.

Colour and box drawing appear on a terminal and not in a CI log, honouring `NO_COLOR` and
`MOSO_NO_COLOR`. Near-miss names get a "did you mean" suggestion that compares trailing type segments
rather than whole module paths, ranks "the same type name in another module" above any edit-distance
match, and refuses to suggest for names short enough that the suggestion would be a guess. Push your
own problem with `BootError::Other { message, notes, fix }`. `BootError` is `#[non_exhaustive]`, so a
`match` over it needs a `_` arm.

## Errors in the OpenAPI document

`impl Describe for Error` contributes exactly two responses to every operation whose handler returns
`Result<T>`: a 500 and a 503. That is deliberately conservative, so a document does not claim every
endpoint can return a 409. Operation-specific errors come from the extractors that raise them (a
`Json<T>` or `Form<T>` body contributes the 422) and from `#[endpoint(errors = T)]`.

The derive's `Describe` impl emits one response per distinct status across the taxonomy's variants.
Variants that share a status share one response entry, and their descriptions are joined with `"; "`.
Every one of those responses is `application/problem+json` and references the shared `Problem`
component schema; the 422 a body extractor contributes references `ValidationProblem` instead, which
is the same document with the `errors` array. See [OpenAPI](./openapi.md).

## Testing errors

`moso-test` asserts against the parsed document rather than the raw string:

```rust
response
    .assert_problem("validation")            // or the full type URI
    .assert_field_error("/title", "len");
```

`assert_problem` accepts the full `type` URI or just its last segment, which is what
`ErrorKind::slug()` returns. `.problem()` gives you the parsed `Problem` when you need something the
helpers do not cover, and `assert_json_at` reaches individual pointers:

```rust
let response = app
    .post(&format!("{API}/posts"))
    .json(&json!({ "title": "ab", "body": "The body." }))
    .send()
    .await
    .assert_status(422)
    .assert_json_at("/errors/0/pointer", Value::from("/title"))
    .assert_json_at("/errors/0/code", Value::from("len"));
```

See [testing](./testing.md) for the rest of the assertion surface.

## Failure modes and gaps

**The router's hand-built documents carry no ids.** `problem_response` builds a document directly
for the router's fallback 404, for its default 405, for its "no application state" 500, for the 504
a per-route `Router::timeout` produces, and for the 404 and 405 a static mount answers with, because
all of those have to render with no configuration, no provider map and no `catch_error` layer in
play. They get the right `status`, `type` and `title` (`ErrorKind::type_uri()`, the same URI the
taxonomy uses, so `assert_problem("not-found")` matches a fallback 404) and a `detail`. What they
do not get is `request_id`, `trace_id` or `instance`, because those come from the ambient
`ErrorContext` the middleware installs and these responses are produced outside it. A client
correlating a failure by `request_id` gets nothing back from a 404 for a path that does not exist.

**`Error` is neither `Clone` nor `Serialize`.** It holds a boxed source, which is neither. Go through
`Problem`, which is both. `Error` itself is one pointer wide: its members add up to more than 250
bytes, and `Result<T, Error>` is the return type of every handler, extractor and dependency in the
program, so it is boxed rather than making every success path carry that width.

**Testing a handler by calling it directly gives no ids.** Without the layer there is no ambient
context, so the document has no `request_id` and no `instance`. Go through the service with
`moso-test`, or install a context with `with_error_context`.

**`log_headers` produces nothing on a 4xx.** The dump is filtered by `is_server_error()` before the
line is built, so a 401 you are trying to debug will not show you its headers.

**`ErrorKind` and `BootError` are both `#[non_exhaustive]`.** Match arms need a `_`.

**Keeping `#[endpoint(errors = ...)]` in step with the body is a lint, not a compile error.**
`moso check`'s `unhandled_error_variant` catches "a handler constructing a 4xx it does not declare",
at `warn` by default, and it reads the OpenAPI document, so it needs an application it can run.
Nothing in `rustc` will tell you: adding an `Err(Error::conflict(..))` to a handler whose annotation
does not list a 409 compiles, ships, and leaves the document wrong until something runs the check.
Put it in CI.

## See also

- [Validation](./validation.md) for how field errors are produced from a schema.
- [Middleware](./middleware.md) for the slot order and where `catch_error` sits in it.
- [Observability](./observability.md) for the request log line and the metrics.
- [Security](./security.md) for redaction, secrets and the disclosure posture.
- [Testing](./testing.md) for the full assertion surface.
