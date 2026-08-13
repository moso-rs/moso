---
title: Extractors
description: Read the request with typed extractors that deserialise, validate and document themselves, and write your own when the built-ins run out.
order: 3
status: shipped
---

An extractor is a handler parameter that knows how to build itself from the request. In Moso it also
knows what its presence means for the API contract, so `Query<ListPosts>` does not only parse the
query string, it writes the `query` parameters into the OpenAPI document at boot. You never annotate
a handler to describe its inputs.

This page covers every built-in extractor, the order the framework runs them in, the exact status
code each one produces when it rejects a request, what makes an extractor self-describing, and how to
write your own.

## The smallest thing that works

```rust
use moso::prelude::*;

/// A user, as the API accepts one.
#[derive(Schema)]
pub struct CreateUser {
    /// Public handle.
    #[schema(len = 3..=32)]
    pub username: String,
}

/// A user, as the API returns one.
#[derive(Schema)]
pub struct UserOut {
    /// Stable identifier.
    pub id: u64,
}

/// Create a user.
#[endpoint]
async fn create(Json(body): Json<CreateUser>) -> Result<Created<Json<UserOut>>> {
    let _ = body.username;
    Ok(Created::at("/users/1", Json(UserOut { id: 1 })))
}
```

A `username` shorter than three characters never reaches the function body. The 422 is produced
during extraction, before your code runs. The generated document carries the request body schema,
400, 413, 415, 422 and a 201 with a `Location` header, with nothing annotated.

## How a handler signature is read

`#[endpoint]` reads your parameter list at macro expansion time and enforces these rules.

| Rule | Why |
| --- | --- |
| The handler is a free `async fn` | Methods and closures are not registered |
| Non-generic, no `impl Trait` in argument position | The macro writes `<Ty as Extract>::describe(..)`, and a path cannot contain `impl Trait` |
| At most 16 parameters | A hard cap on the generated glue |
| At most one body extractor | Only one thing can consume the body |
| The body extractor must be the last parameter | Everything before it runs against the request head |

Parts extractors run in declaration order against a `&mut http::request::Parts`. Because the parts
are mutable, an extractor can take a header out of the map instead of cloning it. A body extractor,
if present, runs last against the reassembled request.

The first extractor that fails short-circuits the request: its `Error` becomes the response, no later
extractor runs, and the handler is never called.

### Which parameter counts as the body

The macro decides body-versus-parts by a **name heuristic** on the outermost path segment of the
type, because a blanket `impl<T: Extract> ExtractBody for T` cannot exist under coherence. A
parameter is treated as a body extractor when its type name is one of `Bytes`, `Form`, `Json`,
`Multipart`, `Raw`, `RawBody`, `Stream`, `String`, `Text`, `Upload`, `Xml`, or when the name ends
with `Body`, starts with `Body`, ends with `Multipart`, or ends with `Upload`. Everything else is a
parts extractor.

The heuristic decides which trait is named; the trait bound is what enforces the rule. Get it wrong
and you get a hand-written message about the wrong trait:

```text
error[E0277]: `Lines` cannot be used as a handler parameter
   |                              ^^^^^ not an extractor
   |
help: the trait `Extract` is not implemented for `Lines`
   = note: extractors: `Path<T>`, `Query<T>`, `Headers<T>`, `Cookies`, `Inject<T>`, `Depends<T>`
   = note: a request body is `Json<T>`, `Form<T>`, `Bytes` or `Text`, and must be last
```

`Lines` implements `ExtractBody`, not `Extract`. Renaming it `LinesBody` is the fix.

## Every built-in extractor

| Extractor | Import | Reads | Rejects with | Contributes to the document |
| --- | --- | --- | --- | --- |
| `Path<T>` | prelude | Matched path segments | 422; 500 when the struct and the template disagree | One `in: path` parameter per field or slot |
| `Query<T>` | prelude | The query string | 422; 400 past `http.query_depth_max` | One `in: query` parameter per property |
| `Headers<T>` | `moso::extract::Headers` | Request headers | 422 rooted at `/header` | One `in: header` parameter per non-redacted field |
| `http::HeaderMap` | `moso::deps::http` | All headers, raw | Never | Nothing |
| `Cookies` | `moso::extract::Cookies` | The `Cookie` header | Never | Nothing |
| `Json<T>` | prelude | The body | 415, 413, 400 (malformed, or past `http.json_depth_max`), 422 | `requestBody` plus 400, 413, 415, and 422 when `T` has constraints |
| `Form<T>` | prelude | The body | 415, 413, 400, 422 | `requestBody` as urlencoded, plus the same statuses |
| `Bytes` | `moso::extract::Bytes` | The body, capped | 413 | `requestBody` as binary, plus 413 |
| `Text` | `moso::extract::Text` | The body, capped, UTF-8 checked | 400, 413 | `requestBody` as text, plus 400 and 413 |
| `RawBody` | `moso::extract::RawBody` | The body, uncapped | Never | `requestBody` as `*/*`, not required, marked `x-moso-raw-body` |
| `BodyStream` | `moso::extract::BodyStream` | The body as a chunk stream, uncapped | Never | Marked `x-moso-streaming-body` |
| `Multipart` | `moso::extract::Multipart` | A `multipart/form-data` body | 400, 413 | `requestBody` as multipart, plus 400 and 413 |
| `Inject<T>` | prelude | The application provider map | Cannot fail; boot proved the provider exists | Nothing, but adds a `ProviderReq` to the boot check |
| `Depends<T>` | prelude | A per-request dependency, memoised | Whatever `Dependency::resolve` returns | Whatever `Dependency::describe` writes, typically a security scheme and a 401 |
| `RequestCtx` | prelude | The whole request context | Never | Nothing |
| `RequestId` | `moso::extract::RequestId` | The correlation id as a `Ulid` | Never | Nothing |
| `ClientIp` | `moso::extract::ClientIp` | The proxy-aware client address | 500 when the server was started without connection info | Nothing |
| `ConnectInfo<T>` | `moso::extract::ConnectInfo` | The peer socket | 500 when connection info is absent | Nothing |
| `Extension<T>` | `moso::extract::Extension` | A value a middleware inserted | 500 naming the missing layer | Nothing |
| `MatchedPath` | `moso::extract::MatchedPath` | The route pattern, not the concrete path | 500 on a route with no pattern | Nothing |
| `http::Method`, `http::Uri`, `http::Version` | `moso::deps::http` | The request line | Never | Nothing |
| `Option<T>` | core | Anything, optionally | Never | Inherits `T`'s `describe` and `PROVIDER_REQ` |
| `()` | core | Nothing | Never | Nothing |
| `Opaque<T>` / `OpaqueBody<T>` | `moso::extract` | Any Axum extractor | Whatever the Axum rejection maps to | Nothing, deliberately |

Everything in that table that contributes nothing does so on purpose. "This handler read the request
method" is not a fact a client can act on, so it does not belong in an API contract.

## Path parameters

`Path<T>` takes three shapes.

```rust
use moso::prelude::*;
use moso::response::NoContent;

/// One segment: the type is the parameter.
#[endpoint]
async fn show(Path(slug): Path<Slug>) -> Result<Json<PostOut>> {
    Ok(Json(PostOut { slug }))
}

/// Two segments, matched positionally in declaration order.
#[endpoint]
async fn comment(Path(ids): Path<(u64, u64)>) -> Result<NoContent> {
    let _ = ids;
    Ok(NoContent)
}

/// Where a comment lives.
#[derive(Schema)]
pub struct Target {
    /// Which post.
    pub post: u64,
    /// Which comment on it.
    pub comment: u64,
}

/// Two segments, named, which reads better past two.
#[endpoint]
async fn edit(Path(target): Path<Target>) -> Result<NoContent> {
    let _ = target.post;
    Ok(NoContent)
}
```

For a struct, the field names must match the `{braces}` in the route template. For a scalar or a
tuple there are no names to match, so `describe` emits parameters with empty names and the router
fills them in positionally at `App::build()`.

`T: Schema`, so `T::validate` runs after deserialisation. `Path<Slug>` cannot hand you a string that
is not a slug.

A disagreement between the type and the template is an **application** error, not a client one. Boot
catches most of it: the document builder compares emitted `in: path` parameters against the template
placeholders and refuses to build an `App` when they differ, naming both sides. What reaches the
runtime (a wrong arity, or a field the route does not capture) is a **500** whose detail reads
`the route does not capture a path parameter named 'post_id'; it declares ["id"]`, not a 4xx. The
client did nothing wrong.

## Query strings

```rust
use moso::prelude::*;

/// The query string this listing accepts.
#[derive(Schema)]
pub struct ListPosts {
    /// Free-text filter.
    #[schema(len = ..=100)]
    pub search: Option<String>,
    /// Tags to match, repeated or comma separated.
    #[schema(delimiter = ",")]
    pub tags: Vec<String>,
    /// Where to resume from.
    pub cursor: Option<Cursor>,
    /// How many rows to return.
    #[schema(range = 1..=100, default = 20)]
    pub limit: u32,
}

/// List posts.
#[endpoint]
async fn list(Query(q): Query<ListPosts>) -> Result<Page<PostOut>> {
    let _ = (q.search, q.tags, q.limit);
    Ok(Page::empty())
}
```

The parser understands more than flat key-value pairs.

| Query string | Field type | Result |
| --- | --- | --- |
| `?tag=a&tag=b` | `Vec<String>` | `["a", "b"]` |
| `?tags=a,b` | `Vec<String>` | `["a,b"]`, one element, because a comma is a legal tag character |
| `?tags=a,b` | `Vec<String>` with `#[schema(delimiter = ",")]` | `["a", "b"]`, and the repeated-key form still works |
| `?filter[status]=open` | A nested struct field | `filter.status == "open"` |
| `?a[0]=x&a[1]=y` | `Vec<String>` | `["x", "y"]` |
| `?published` | `bool` | `true` |
| absent | `Option<T>` | `None` |
| absent | a field with `#[schema(default = 20)]` | `20`, and the parameter is documented as not required |

`#[schema(delimiter = ...)]` accepts `","`, `"|"` and `" "` only. It expands to a
`#[serde(deserialize_with = ...)]` pointing at `comma_delimited`, `pipe_delimited` or
`space_delimited` in `moso::extract::query`, which you can also use directly. Declaring a delimiter
widens what a field accepts rather than narrowing it.

Unknown parameters are ignored. Real clients append `utm_source` and `fbclid` to URLs, and rejecting
them breaks live traffic for no benefit. Add `#[schema(deny_unknown)]` to the struct to opt into
rejection. For the same reason a key with unbalanced brackets (`utm[source`) is treated as a literal
name rather than an error.

Nesting depth is bounded by `http.query_depth_max`, default 8. Past it the request is a 400.

In the document, an object-typed property becomes `style: deepObject` and an array-typed one becomes
`style: form, explode: true`. A property with a schema `default` is documented as not required even
when the object lists it in `required`.

> [!NOTE]
> `#[schema(flatten_bracket)]` is reserved for forward compatibility: the derive accepts and stores
> the flag, but no emitter acts on it yet. You do not need it either, because bracket nesting already
> works for any nested struct field: `QueryMap` handles brackets natively before the deserialiser
> ever runs.

## Typed headers

```rust
use moso::prelude::*;
use moso::extract::Headers;
use moso::response::NoContent;

/// The headers this endpoint reads.
#[derive(Schema)]
pub struct ApiHeaders {
    /// Which version of the contract the client expects.
    #[schema(rename = "x-api-version")]
    pub api_version: String,
    /// The client's cached entity tag.
    #[schema(rename = "if-none-match")]
    pub if_none_match: Option<String>,
}

/// Show a post.
#[endpoint]
async fn show(Headers(h): Headers<ApiHeaders>) -> Result<NoContent> {
    let _ = (h.api_version, h.if_none_match);
    Ok(NoContent)
}
```

Field names map to header names by replacing `_` with `-`, so `api_version` reads `api-version`. Use
`#[schema(rename = "...")]` where the header name is not a Rust identifier or does not follow that
rule. A repeated header collects into a `Vec`.

Three behaviours differ from the other extractors:

- **Undeclared headers are always ignored**, even with `#[schema(deny_unknown)]`. Every request
  carries `host`, `user-agent` and a dozen more that no struct will declare.
- **A non-UTF-8 header value is skipped, not rejected.** A declared field then sees it as missing,
  which for a required field is a 422 with code `required`.
- **A field whose header name is in `REDACTED_HEADERS` is omitted from the document** while still
  being extracted. The list is `authorization`, `proxy-authorization`, `cookie`, `set-cookie`,
  `x-api-key`, `x-auth-token`, `x-csrf-token`. Documenting `Authorization` as a plain header
  parameter and also as a security scheme generates broken clients.

For the raw map, take `http::HeaderMap` directly. It never fails and documents nothing.

## Cookies

```rust
use moso::prelude::*;
use moso::extract::{Cookie, Cookies};
use moso::response::NoContent;

/// Remember that this reader visited.
#[endpoint]
async fn visit(cookies: Cookies) -> Result<NoContent> {
    let theme = cookies.get("theme").map(|c| c.value().to_owned());
    let _ = theme;
    cookies.add(Cookie::new("seen", "1"));
    Ok(NoContent)
}
```

That handler sends `Set-Cookie: seen=1; HttpOnly; SameSite=Lax; Secure; Path=/`, without the
`Secure` in the `dev` profile, for the reason in the table below. The jar lives in the
`RequestCtx`, behind a `OnceLock`, and the handler adapter drains it into `Set-Cookie` headers once
the response exists. Everything that can reach a `RequestCtx` reaches the *same* jar: `Cookies` as a
parameter, `ctx.cookies()` from a guard or a dependency, a second `Cookies` parameter on the same
handler. There is no constructor that produces a second one, which matters because a second jar is
not a visible bug: it accepts every write and then throws it away.

A request that never mentions a cookie never creates the jar, so the check after the handler returns
is one atomic load and the response is passed on untouched.

### What Moso fills in

`cookie::Cookie` leaves every attribute absent unless you set it, so a bare `Cookie::new` would reach
the browser script-readable, over plain HTTP, scoped to the directory of the request that set it.
Every write through `Cookies` fills in what you did not say:

| Attribute | Filled in with | Because |
| --- | --- | --- |
| `HttpOnly` | on | a cookie a script can read is a cookie one XSS can steal |
| `SameSite` | `Lax` | the CSRF default current browsers assume anyway |
| `Path` | `/` | a directory-scoped cookie is almost never what was meant |
| `Secure` | on unless the profile is `dev` | `http://localhost` never receives a `Secure` cookie |

**Unset** is the operative word, and it is the escape hatch. An attribute you state is left exactly as
stated, so a cookie that genuinely has to be readable from JavaScript says so out loud, and keeps
saying so in the diff:

```rust
use moso::extract::{Cookie, CookieDefaults, Cookies, SameSite};

fn set(cookies: &Cookies) {
    // Says nothing: gets the four defaults.
    cookies.add(Cookie::new("seen", "1"));

    // Says something: keeps it, in production too.
    cookies.add(Cookie::build(("csrf", "t0ken")).http_only(false).same_site(SameSite::Strict).into());

    // What the framework would have filled in, if you need to ask.
    let _: CookieDefaults = cookies.defaults();
}
```

`HttpOnly`, `SameSite` and `Path` are constants rather than configuration on purpose: making
`HttpOnly` a config key turns "we turned it off once to debug something" into a deployed default
nobody re-reads.

### Writes, and when they are sent

- **`Set-Cookie` is appended, never set.** A response may legitimately carry several, and a
  middleware that writes one of its own (`moso-auth`'s session layer does) keeps it.
- **A name the response already sets wins.** If a handler writes a `Set-Cookie` header directly *and*
  puts the same name in the jar, the header already on the response survives and the jar's value is
  dropped with a `DEBUG` line. Emitting both would leave the outcome to header ordering.
- **A cookie set by a guard that then rejects is still sent.** A guard that clears a stale session
  before answering 401 is the reason: dropping the write would leave the browser presenting a
  credential that can never work again. Every `Set-Cookie` recorded during a request reaches the
  response, whatever the status.
- **`Cookies::remove` always emits an expiring cookie**, including for a name the client did not
  present on this request. Path and domain scoping mean "not sent" and "not held" are different
  questions, and a logout that quietly sends nothing is the worst available answer. This is where
  Moso departs from `cookie::CookieJar::remove`, which suppresses it.

`Cookies::delta()` renders the pending values and `Cookies::apply_to(&mut headers)` appends them; the
adapter calls the second one. Both are public for anyone writing an adapter of their own.

### Signed and private

`Cookies::signed()` gives a tamper-evident view and `Cookies::private()` an encrypted one. Both need a
`CookieKey` provider, which `Cookies` declares as `ProviderReq::optional_of::<CookieKey>()`. Without
one they **fail closed**: reads return `None`, writes are dropped, and an `ERROR` line is logged.
`try_signed()` and `try_private()` are the same views with the failure as a `Result`.
`CookieKey::derive` refuses a secret shorter than 32 bytes, and `CookieKey::generate` exists for
tests.

A value written through `signed()` comes back through `signed()` on the next request and is not
readable through the plain view: the plain view sees the signature and the value, `private()`'s is
ciphertext. Both views apply the same attribute defaults as a plain `add`.

## JSON bodies

`Json<T>` reads the body under `http.body_max`, deserialises it with an RFC 6901 JSON Pointer on
failure, and runs `T::validate` before the handler sees the value.

Content type acceptance is deliberately lax on one axis and strict on another. A missing or empty
`Content-Type` is accepted, because too many clients omit it. `application/json` with any parameters
is accepted, and so is any `application/*+json` suffix, so `application/vnd.api+json` works.
`application/jsonish` is a 415. The check is case-insensitive.

The split between 400 and 422 is the part worth internalising:

| Situation | Status | Pointer | Code |
| --- | --- | --- | --- |
| The document is not JSON | 400 | root | parse failure |
| A required member is missing | 400 | `/title` | `required` |
| A member has the wrong JSON type | 400 | `/count` | `type` |
| A constrained type such as `Email` rejects a value while deserialising | 422 | `/email` | the constraint's own code |
| A `#[schema(len = ...)]` or `range` constraint fails | 422 | `/title` | `len`, `range`, and so on |
| The body exceeds `http.body_max` | 413 | none | none |
| The `Content-Type` is not JSON | 415 | none | none |

A 400 means the payload never became the target type. A 422 means it did and then failed a
constraint. That split is what lets a client tell "my serialiser is wrong" from "my data is wrong".
See [errors](./errors.md) for the response shape and [validation](./validation.md) for the codes.

The 422 and the `op.mark_validated()` flag are contributed only when `T::HAS_CONSTRAINTS`, so a
schema with no constraints does not document a status it can never produce.

`Json<T>` is also a response type. See [responses](./responses.md).

## Form bodies

```rust
use moso::prelude::*;
use moso::response::Redirect;

/// The browser form this endpoint accepts.
#[derive(Schema)]
pub struct LoginForm {
    /// Who is signing in.
    pub email: Email,
    /// Their password.
    #[schema(len = 8..=128)]
    pub password: String,
    /// Whether to stay signed in.
    #[schema(default = false)]
    pub remember: bool,
}

/// Sign in and send the browser onwards.
#[endpoint]
async fn login(Form(creds): Form<LoginForm>) -> Result<Redirect> {
    let _ = (creds.email, creds.remember);
    Ok(Redirect::to("/dashboard"))
}
```

Same contract as `Json<T>`: capped read, pointer on failure, then validation. Three differences.

- An HTML checkbox submits `name=on` when ticked and nothing at all when not, so a `bool` field
  accepts `on`, `true`, `1`, `yes` and `y` as true and treats absence as `false`. Give the field a
  `#[schema(default = false)]` so the unticked case is not a missing-member error. This is one of the
  few places Moso is deliberately lax; the alternative is that every browser form fails validation.
- Pointers are rooted at the **document root**: `/email`, not `/form/email`. A form is the body.
- Form nesting depth is a fixed 8, independent of `http.query_depth_max`.

`Form` is a body extractor only. A `GET` form submission arrives as a query string, so use `Query<T>`
there.

## Raw bodies

| Type | Capped | Fails when | Use it for |
| --- | --- | --- | --- |
| `Bytes` | yes, `http.body_max` | over the cap | a payload you parse yourself |
| `Text` | yes, `http.body_max` | over the cap, or not UTF-8 | plain text |
| `RawBody` | **no** | never | handing `axum::body::Body` to something else |
| `BodyStream` | **no** | per chunk | streaming to disk or to an upstream |

`read_limited` rejects on a `Content-Length` that already exceeds the limit, then enforces the cap
frame by frame while reading. A 100 MiB body against a 1 MiB cap costs about a megabyte and one 413,
not a hundred megabytes: the unit test asserts that at most `limit / chunk + 1` frames are ever
pulled. `Content-Length` is consulted first as a cheap rejection but is not trusted, because a
chunked body has none and a lying one is exactly what an attacker sends.

The read also honours the tighter of its own limit and the `BodyCap` that the `body_limit`
[middleware](./middleware.md) installed, so the 413 names the cap that actually fired.

`RawBody` and `BodyStream` opt out of the limit entirely. The handler owns the bound.

## Multipart uploads

`Multipart` is behind the `multipart` cargo feature, off by default because it pulls extra Axum
surface into every cold compile.

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1", features = ["multipart"] }
```

```rust
use moso::prelude::*;
use moso::extract::Multipart;
use moso::response::NoContent;

/// Accept an upload.
#[endpoint]
async fn upload(mut form: Multipart) -> Result<NoContent> {
    while let Some(field) = form.next_field().await? {
        let name = field.name().unwrap_or_default().to_owned();
        let bytes = field.bytes().await?;
        let _ = (name, bytes.len());
    }
    Ok(NoContent)
}
```

Two independent caps apply, both enforced while reading: `http.multipart_file_max` per field
(16 MiB) and `http.multipart_max` for the whole payload (32 MiB). A per-field cap alone lets a client
send a thousand fields of fifteen megabytes; a total cap alone lets one field consume the budget and
starve the rest of the form. Read the current pair with `form.limits()`, which returns a
`MultipartLimits { total, per_field }`, and the running total with `form.consumed()`.

A `Field` exposes `name()`, `file_name()`, `content_type()` and `headers()`. `Field::bytes()` and
`Field::text()` enforce both caps. `Field::chunk()` enforces **neither**: if you stream a field
yourself, you own the bound.

> [!CAUTION]
> `Field::file_name()` is client-supplied. It is a label, not a path. Never join it onto a directory.
> See [file storage](./file-storage.md) for the supported way to persist an upload.

Multipart is deliberately field-oriented rather than a typed `Upload<T>`: you read each `Field` and
persist it through [file storage](./file-storage.md), which keeps the client-supplied file name a
label rather than something the framework trusts as a path.

## Injected values and dependencies

`Inject<T>` reads an application-scoped provider. It cannot fail at the use site: `App::build()`
refused to produce an `App` unless every `Inject<T>` reachable from the route table had a provider,
which is what `Extract::PROVIDER_REQ` exists for. It holds an `Arc<T>` and derefs to `T`.

`Depends<T>` resolves a per-request value through `Dependency::resolve`, memoised by `TypeId` for the
rest of the request. Two extractors and a guard all asking for `CurrentUser` cause one database
query. Unlike `Inject`, a dependency can fail, and unlike `Inject` it documents itself: the security
scheme and the 401 or 403 it raises appear on every operation that takes it.

```rust
use moso::prelude::*;

/// Who am I?
#[endpoint]
async fn me(Depends(user): Depends<CurrentUser>) -> Result<Json<UserOut>> {
    Ok(Json(UserOut { id: user.id }))
}
```

Take `_: Depends<Guard>` when you want a dependency purely for its side effect. Both types deref to
their inner value and both have `into_inner()`, which gives you an `Arc<T>` for `Inject` and a `T`
for `Depends`. The full model is in [dependency injection](./dependency-injection.md).

## The request context and the small extractors

`RequestCtx` is the whole per-request context, and taking it directly is the escape hatch when a
purpose-built extractor does not exist.

| Method | Returns |
| --- | --- |
| `ctx.request_id()` | `&Ulid` |
| `ctx.method()`, `ctx.uri()`, `ctx.version()`, `ctx.path()` | The request head |
| `ctx.headers()` | `&HeaderMap` |
| `ctx.matched_path()` | `Option<&str>`, the route pattern |
| `ctx.limits()` | The `Limits` snapshot every extractor reads |
| `ctx.provider::<T>()` / `ctx.try_provider::<T>()` | An application provider |
| `ctx.depends::<D>()` | A memoised dependency |
| `ctx.config::<C>()` | A typed configuration section |
| `ctx.extension::<T>()` | A cloned request extension |
| `ctx.shutdown()` | The shutdown `Signal` |
| `ctx.path_params()` | The raw captures |

The narrow extractors are usually clearer. `MatchedPath` is what a metrics label must use: a label
built from the raw path is the classic cardinality explosion, one time series per user id.

`ClientIp` consults `X-Forwarded-For` **only** when `http.trusted_proxies` is non-empty and the peer
matches one of the configured CIDRs, then walks the chain right to left. With no trusted proxies
configured the forwarded header is not consulted at all, by design: an unvalidated
`X-Forwarded-For` is a client-controlled string.

`ClientIp` and `ConnectInfo<T>` need the server to have been started with connection info.
`App::serve` installs it. A hand-rolled `axum::serve` must use
`into_make_service_with_connect_info::<SocketAddr>()`, and without it the extractor answers a 500
that says so.

`Extension<T>` looks in `parts.extensions` first and then in `ctx.extension::<T>()`, so it works
whether the layer that inserted the value ran inside or outside the handler adapter. It is the
supported channel from a middleware to a handler.

## Optional extraction

Wrap any extractor in `Option<T>` to turn a rejection into `None`:

```rust
use moso::prelude::*;
use moso::extract::Extension;
use moso::response::NoContent;

/// Act on a request that may or may not carry a trace context.
#[endpoint]
async fn track(trace: Option<Extension<TraceContext>>) -> Result<NoContent> {
    let _ = trace;
    Ok(NoContent)
}
```

`Option<T>` inherits `T`'s `describe` and `PROVIDER_REQ`, so the parameters still appear in the
document and the boot check still runs.

> [!WARNING]
> `Option<T>` swallows the underlying error. A malformed value and an absent one become the same
> `None`. That is deliberate (`Option<Depends<Session>>` means "if there is a session", and a
> malformed cookie and no cookie are the same answer to that question), but it is a real edge, so use
> it only where the handler genuinely treats the two alike. When it does not, take `T` and let the
> rejection through. Note also that `Option<T>` works in parameter position only; it is not a valid
> return type.

## What makes an extractor self-describing

Two required items, and `describe` has no default body on purpose: a default of "contributes nothing"
would silently produce wrong documentation for exactly the extractor that most needed to speak up.
One line of boilerplate is the price.

```rust
pub trait Extract: Sized + Send {
    fn describe(op: &mut OperationBuilder);
    const PROVIDER_REQ: &'static [ProviderReq] = &[];
    fn extract<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> impl Future<Output = Result<Self>> + Send + 'a;
}
```

`describe` runs once per route at `App::build()`, against a fresh `OperationBuilder`, and writes
parameters, request bodies, responses and headers. Nothing runs it per request, so documentation
costs nothing at runtime. `extract` runs per request, in declaration order.

`PROVIDER_REQ` is what makes the boot-time check possible. Declare in it any provider your `extract`
reaches for through `ctx.provider::<T>()`, and a missing one becomes a boot error naming the type
rather than a 500 on the first request that hits the route.

`ExtractBody` is the same two items with `extract_body(req: Request, ctx: &RequestCtx)` in place of
`extract`, and it may appear at most once, last.

## Writing your own extractor

```rust
use moso::prelude::*;
use moso::openapi::Param;
use moso::deps::http::request::Parts;
use moso::{Extract, ProviderReq};

/// Which customer this request belongs to.
pub struct Tenant(pub String);

impl Extract for Tenant {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) {
        op.parameter(Param::header("x-tenant").required(false).schema_of::<String>());
        op.response(404, ResponseSpec::problem("Unknown tenant"));
    }

    async fn extract(parts: &mut Parts, _ctx: &RequestCtx) -> Result<Self> {
        parts
            .headers
            .get("x-tenant")
            .and_then(|v| v.to_str().ok())
            .map(|v| Tenant(v.to_owned()))
            .ok_or_else(|| Error::not_found("tenant"))
    }
}

/// Show the caller's own tenant.
#[endpoint]
async fn whoami(tenant: Tenant) -> Result<Json<String>> {
    Ok(Json(tenant.0))
}
```

Every operation that takes a `Tenant` now documents the `x-tenant` header and the 404, with nothing
written on the handler. That is the whole contract of self-describing.

If your extractor validates a `T: Schema`, build its context with `ctx.validation(root)` rather than
`ValidationCtx::new()`:

```rust
let mut validation = ctx.validation(moso::extract::BODY_POINTER_ROOT);
value.validate(&mut validation).map_err(Error::validation)?;
```

That one constructor attaches the registered `MessageProvider` and the request's `Accept-Language`
locale, so your extractor gets translated messages for free and cannot drift from the built-ins.
`BODY_POINTER_ROOT`, `QUERY_POINTER_ROOT`, `PATH_POINTER_ROOT` and `HEADER_POINTER_ROOT` are the four
roots the framework uses; see [validation](./validation.md#custom-messages-and-the-message-provider).

For a body extractor, implement `ExtractBody` and reuse the framework's cap:

```rust
use moso::prelude::*;
use moso::extract::{ExtractBody, read_limited};
use moso::openapi::{ContentType, OperationBuilder, ResponseSpec};
use moso::schema::json_schema::{JsonType, SchemaNode, SchemaRef};
use moso::Request;

/// A body of newline-separated lines, read as text.
///
/// The name ends in `Body` on purpose.
pub struct LinesBody(pub Vec<String>);

impl ExtractBody for LinesBody {
    fn describe(op: &mut OperationBuilder) {
        op.request_body(
            ContentType::Text,
            SchemaRef::inline(SchemaNode::of_type(JsonType::String)),
            true,
        );
        op.response(413, ResponseSpec::problem("The body exceeded `http.body_max`"));
    }

    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        let bytes = read_limited(req, ctx.limits().body_max).await?;
        let text = core::str::from_utf8(bytes.as_slice())
            .map_err(|_| Error::bad_request("the body is not UTF-8"))?;
        Ok(LinesBody(text.lines().map(str::to_owned).collect()))
    }
}

/// Count the lines a client sent.
#[endpoint]
async fn count(LinesBody(lines): LinesBody) -> Result<Json<usize>> {
    Ok(Json(lines.len()))
}
```

## Using Axum extractors

Four wrappers bridge the two ecosystems. None of them contributes to the document, because an adapter
cannot know what the wrapped extractor means, and inventing a plausible schema would be worse than
saying nothing.

```rust
use moso::prelude::*;
use moso::deps::axum::extract::OriginalUri;
use moso::extract::Opaque;
use moso::response::NoContent;

/// Log the URI as the client wrote it, before any nesting rewrote it.
#[endpoint]
async fn show(Opaque(uri): Opaque<OriginalUri>) -> Result<NoContent> {
    let _ = uri;
    Ok(NoContent)
}
```

`Opaque<T>` lifts any Axum `FromRequestParts<()>`; `OpaqueBody<T>` lifts any `FromRequest<()>`. They
are two types rather than one because a single wrapper implementing both traits would make the
handler marker ambiguous for every handler ending in one, and the inference error that follows is
exactly the kind of message the framework exists to avoid.

In the other direction, `MosoExt<T>` and `MosoExtBody<T>` let a plain Axum handler use a Moso
extractor:

```rust
use moso::prelude::*;
use moso::extract::MosoExt;

/// The query string this listing accepts.
#[derive(Schema)]
pub struct ListPosts {
    /// How many rows to return.
    pub limit: Option<u32>,
}

// A plain Axum handler, not a Moso `#[endpoint]`.
async fn handler(MosoExt(Query(q)): MosoExt<Query<ListPosts>>) -> impl IntoResponse {
    format!("{:?}", q.limit)
}
```

> [!NOTE]
> `MosoExt<T>` only works inside a Moso route, because it needs the `RequestCtx` the route adapter
> installs. An `axum::Router` mounted with `Router::mount_axum` runs outside that adapter, and the
> extractor answers a 500 naming the reason.

Use `moso::extract::axum_rejection` to map an Axum rejection's status onto the Moso error taxonomy so
it renders as `application/problem+json` like everything else.

## Limits

Every extractor reads its bounds from the `Limits` snapshot in `RequestCtx`, taken once from
`HttpConfig` at boot. See [configuration](./configuration.md) for how to set them.

| Key | Default | Enforced by | Failure |
| --- | --- | --- | --- |
| `http.body_max` | 2 MiB | `Json`, `Form`, `Bytes`, `Text`, and `read_limited` in your own extractors; the `body_limit` slot up front | 413 with `max_bytes` |
| `http.multipart_max` | 32 MiB | `Field::bytes`, `Field::text` | 413 |
| `http.multipart_file_max` | 16 MiB | `Field::bytes`, `Field::text` | 413 |
| `http.query_depth_max` | 8 | `Query<T>` | 400 |
| `http.json_depth_max` | 64 | `Json<T>`, via `check_json_depth` before `serde_json` runs | 400 with `max_depth` |
| `http.uri_max` | 8 KiB | the `request_limits` slot | 414 with `max_bytes` |
| `http.header_max_count` | 100 | the `request_limits` slot | 431 with `max_count` |
| `http.header_max_bytes` | 16 KiB | the `request_limits` slot | 431 with `max_bytes` |
| `http.trusted_proxies` | empty | `ClientIp` | none |

The last three are checked by one middleware slot, `request_limits`, which sits immediately inside
`catch_error` and outside `timeout`, so a refusal is logged like any other error and does not first
buy a thirty-second budget. The check itself is `Limits::check_head`, and the slot's configuration
*is* the `Limits` snapshot the extractors read, so the layer and `ctx.limits()` cannot disagree about
a number the way `stack.body_limit(n)` and `http.body_max` can.

Measurement is worth stating exactly. `uri_max` counts the request target, including the scheme and
authority of an absolute-form target from a proxy. `header_max_count` counts header *fields*, so a
repeated header counts once per occurrence. `header_max_bytes` counts names plus values and excludes
framing, because HTTP/1.1 spends four bytes per field on `": "` and CRLF, HTTP/2 spends none, and a
limit that meant a different thing per protocol would be a limit nobody could set.

Adjust them together:

```rust
stack.request_limits(|limits| {
    limits.uri_max = 2048;
    limits.header_max_count = 64;
});
```

> [!NOTE]
> **Moso is not the only line of defence here, and should not be the outermost one.** By the time any
> Moso code runs, hyper has already read and parsed the head under its own header-count, buffer and
> `SETTINGS_MAX_HEADER_LIST_SIZE` limits. Nothing in a Rust framework can inspect a request target
> before the server that framed it allocated one. What the slot adds is *policy*: the operator's own
> numbers, and an RFC 9457 document naming the limit that fired, in place of hyper's bare
> connection-level refusal. Keep the same bounds on your reverse proxy; the tighter of the two wins.

> [!NOTE]
> **There is no 408.** It would need a read deadline on the request body distinct from the
> whole-request timeout, and Moso has neither that deadline nor a configuration key for one. The
> `timeout` slot already ends a request that does not finish, and it answers 504; a second spelling
> of the same condition would be a status a client could not act on differently.

## Failure modes worth knowing

- **Validation pointers are not rooted the way the design intended.** `Query`, `Path` and `Headers`
  pass a `/query`, `/path` or `/header` root, and a deserialisation failure honours it
  (`/query/limit`, `/header/x-api-version`). A **constraint** failure does not: the `Validate` impl
  the derive generates writes a literal field pointer, so `?limit=1000` against
  `#[schema(range = 1..=100)]` reports `/limit`, not `/query/limit`. Address a validation error by its
  field name and you will be right in both cases.
- **A missing `Content-Type` is accepted** by both `Json` and `Form`, so a client that forgets it gets
  a 400 or a 422 about its payload rather than a 415 about its headers.
- **Two extractors reading the same body is impossible by construction.** The macro allows one, and it
  must be last.
- **A guard that adds a cookie** reaches the same jar as the handler, through `ctx.cookies()`, and
  its write is sent even when the guard rejects the request.
- **Moso is JSON-first.** There is no `Xml<T>` body type and no content negotiation: JSON is the
  request and response representation, and `Error` chooses only between `application/problem+json`
  and `application/json`.
- **`path_shape` and `PathShape` are public but unused by the framework.** The boot-time check that a
  `Path<T>` agrees with its route template compares emitted parameter names against template
  placeholders instead. The helpers work; nothing calls them for you.

## See also

- [Responses](./responses.md) for the other half of the handler signature.
- [Schemas](./schemas.md) and [validation](./validation.md) for the `T: Schema` bound every typed
  extractor requires.
- [Errors](./errors.md) for the RFC 9457 document every rejection renders as.
- [Dependency injection](./dependency-injection.md) for `Inject` and `Depends` in full.
- [OpenAPI](./openapi.md) for what `describe` writes into and how the document is assembled.
