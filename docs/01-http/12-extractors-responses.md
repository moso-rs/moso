# 12 - Extractors & Responses

> **Status: implemented.** This document describes the shipped API. Two blanket impls it originally
> specified turned out to be impossible; see § *The traits* and the implementation note on
> [ADR-0002](../adr/0002-own-the-handler-traits.md).

## The core idea: self-describing extractors

An Axum extractor answers "how do I build myself from a request." A Moso extractor answers that
**and** "what does my presence mean for the API contract." That second method is what lets the
OpenAPI document be generated with zero per-handler annotation, and what lets boot-time validation
know which providers a route needs.

## The traits

```rust
// spec - moso-core/src/extract/mod.rs

/// Built from request parts (headers, URI, extensions). Any number per handler.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used as a handler parameter",
    label = "not an extractor",
    note = "built-in extractors: `Path<T>`, `Query<T>`, `Headers<T>`, `Cookies`, `Inject<T>`, \
            `Depends<T>`, `Extension<T>`, `RequestId`, `ClientIp`, `Method`, `Uri`",
    note = "for a request body use `Json<T>`, `Form<T>`, `Bytes`, `Text` or `BodyStream` - and \
            it must be the LAST parameter",
    note = "help: for an application-lifetime value: `Inject<{Self}>`, registered with \
            `App::provide`",
    note = "help: for a per-request value: `#[derive(moso::Dependency)]` on `{Self}`, then take \
            `Depends<{Self}>`",
    note = "help: to use an Axum extractor unchanged, wrap it: `Opaque<{Self}>`"
)]
pub trait Extract: Sized + Send {
    /// Contribute parameters / security / responses to the operation being described.
    fn describe(op: &mut OperationBuilder);

    /// Providers this extractor needs at app scope. Checked at boot.
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn extract<'a>(parts: &'a mut Parts, ctx: &'a RequestCtx)
        -> impl Future<Output = Result<Self>> + Send + 'a;
}

/// Consumes the request body. At most one per handler, and it must be last.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot be used as a request body",
    label = "not a body extractor",
    note = "built-in body extractors: `Json<T>`, `Form<T>`, `Bytes`, `Text`, `BodyStream`, `RawBody`",
    note = "for `Json<T>` and `Form<T>`, `T` must derive `moso::Schema`",
    note = "help: to use an Axum body extractor unchanged, wrap it: `OpaqueBody<{Self}>`",
    note = "a handler has at most one body extractor and it must be the last parameter"
)]
pub trait ExtractBody: Sized + Send {
    fn describe(op: &mut OperationBuilder);
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn extract_body<'a>(req: Request, ctx: &'a RequestCtx)
        -> impl Future<Output = Result<Self>> + Send + 'a;
}
```

Both are RPITIT (return-position `impl Trait` in trait), not `#[async_trait]`, so an extraction
costs no boxed future. The explicit `'a` lifetime is required: the returned future borrows `parts`
and `ctx`. Implementers write the equivalent `async fn extract(parts: &mut Parts, ctx: &RequestCtx)
-> Result<Self>`, which the compiler accepts and which avoids `clippy::manual_async_fn` on every
implementation.

`describe` has **no default body**. A default of "contributes nothing" would silently produce wrong
documentation for exactly the extractor that most needed to speak up.

### There is no blanket `impl<T: Extract> ExtractBody for T`

The original design specified one, so that a handler ending in `Inject<Db>` would still satisfy the
"last parameter is a body extractor" shape. It cannot exist:

- it conflicts under coherence with `impl<T: Schema> ExtractBody for Json<T>` and every other real
  body extractor;
- it would make the `PartsOnly` / `WithBody` marker types that distinguish the two `Handler`
  families ambiguous for **every** handler, since any handler's last parameter would satisfy both.

The ordering rule is enforced by `#[endpoint]` instead, where the message is hand-written
(`11-routing.md § Body extractor must be last`). A handler registered without `#[endpoint]` picks
the family by whether its last parameter implements `ExtractBody`, which is unambiguous precisely
because no blanket impl exists.

### Interop, both directions - by wrapper, not by blanket impl

```rust
// spec
/// Any Axum `FromRequestParts` extractor, in a Moso handler.
pub struct Opaque<T>(pub T);
impl<T: axum::extract::FromRequestParts<()> + Send + 'static> Extract for Opaque<T> { /* … */ }

/// Any Axum `FromRequest` extractor, in a Moso handler.
pub struct OpaqueBody<T>(pub T);
impl<T: axum::extract::FromRequest<()> + Send + 'static> ExtractBody for OpaqueBody<T> { /* … */ }

/// A Moso parts extractor, in a plain Axum handler.
pub struct MosoExt<T>(pub T);
/// A Moso body extractor, in a plain Axum handler.
pub struct MosoExtBody<T>(pub T);
```

`Opaque` and `OpaqueBody` are separate types on purpose: a single wrapper implementing both traits
would make the handler marker ambiguous for every handler ending in one.

The reverse direction was originally specified as
`impl<T: Extract> axum::extract::FromRequestParts<()> for T`. That is an **orphan-rule violation**
(E0210): `T` is an uncovered type parameter in an impl of a foreign trait. `MosoExt<T>` /
`MosoExtBody<T>` are the wrappers that replace it; both read the `RequestCtx` that Moso's handler
adapter inserts into the request extensions, which is also why `extract::ctx_from_parts(&Parts)
-> Result<RequestCtx>` is public.

Neither adapter contributes anything to the OpenAPI document. That is the honest default - an
adapter cannot know what the wrapped extractor means - and the documentation says so rather than
inventing a plausible-looking schema.

## The built-in extractor set

| Extractor | Kind | Describes | Notes |
| --- | --- | --- | --- |
| `Path<T>` | parts | path parameters from `T`'s fields | `T: Schema`, or a tuple/scalar |
| `Query<T>` | parts | query parameters, incl. defaults & constraints | validated; nested & arrays |
| `Headers<T>` | parts | header parameters | `#[derive(Schema)]` with `#[schema(rename)]` |
| `HeaderMap` | parts | - | the raw map |
| `Cookies` | parts | - | plus `SignedCookies`, `PrivateCookies` |
| `Inject<T>` | parts | nothing; contributes a `ProviderReq` | infallible at the use site |
| `Depends<T>` | parts | whatever `T::describe` says (401/403, security) | memoised per request |
| `Method`, `Uri`, `Version` | parts | - | plain impls on the `http` types |
| `RequestCtx` | parts | - | the context itself, for advanced use |
| `ConnectInfo<T>` | parts | - | peer address |
| `ClientIp` | parts | - | honours `http.trusted_proxies` |
| `Extension<T>` | parts | - | middleware-inserted values |
| `MatchedPath` | parts | - | the route pattern Axum matched |
| `RequestId` | parts | - | correlation id (`Ulid`) |
| `Option<T>` | parts | whatever `T` says | `None` instead of an error |
| `()` | parts | - | for a zero-parameter handler |
| `Json<T>` | body | `requestBody` schema + 400 + 422 | `T: Schema`; validates |
| `Form<T>` | body | `requestBody` (urlencoded) + 400 + 422 | `T: Schema`; validates |
| `Multipart` | body | `requestBody` (multipart/form-data) | **feature `multipart`**; streaming, size-limited |
| `Bytes` | body | `requestBody` as binary | |
| `Text` | body | `requestBody` as text | this is the `String` row of the old table |
| `BodyStream` | body | `requestBody` as a stream | for large uploads |
| `RawBody` | body | `requestBody: {}` | escape hatch; documents itself as unknown |

Changes from the original table, all deliberate:

- `String` → **`Text`**, `Stream` → **`BodyStream`**, `Raw` (body) → **`RawBody`**. `Raw` is a
  *response* type; reusing the name for a body extractor made two different things share one name,
  and `String`/`Stream` shadowed `std` and `futures` in a prelude-heavy file.
- **`Upload<T>` does not exist.** It was specified as "typed multipart with file fields,
  integrates `moso-storage`", and there is no `moso-storage` in this build. It is not stubbed.
- **`Multipart` is behind the `multipart` cargo feature**, off by default, because it pulls extra
  `axum` surface into every cold compile.
- `Xml<T>` (feature `xml`) is named in the `on_unimplemented` note of the original design and does
  not exist. The shipped note lists only real extractors.

### Which extractors describe nothing, and why

`Inject<T>`, `Cookies`, `RequestId`, `ClientIp`, `ConnectInfo<T>`, `Extension<T>`, `MatchedPath`,
`Method`, `Uri`, `Version`, `RequestCtx` and `HeaderMap` contribute nothing. None corresponds to a
fact a client can act on: "this handler read the request method" is not part of an API contract, and
an injected connection pool certainly is not. The apparent exception is `Cookies` - a cookie *can*
be a security scheme, but then it is documented by the `Dependency` that authenticates with it, not
twice.

### `Json<T>` - parse, validate, and document in one step

```rust
// spec
pub struct Json<T>(pub T);

impl<T: Schema> ExtractBody for Json<T> {
    fn describe(op: &mut OperationBuilder) {
        op.request_body_of::<T>(ContentType::Json, /*required=*/true);
        op.response(400, ResponseSpec::problem("Malformed JSON"));
        op.response(413, ResponseSpec::problem("The body exceeded `http.body_max`"));
        op.response(415, ResponseSpec::problem("The `Content-Type` is not JSON"));
        // A 422 is only reachable when `T` declares at least one constraint.
        if T::HAS_CONSTRAINTS {
            op.response(422, ResponseSpec::validation_problem_of::<T>());
            op.mark_validated();
        }
    }
    async fn extract_body(req: Request, ctx: &RequestCtx) -> Result<Self> {
        if !is_json_content_type(req.headers()) {
            return Err(Error::unsupported_media(content_type_of(req.headers())));  // 415
        }
        let bytes = read_limited(req, ctx.limits().body_max).await?;      // 413 on overflow
        let value = from_slice::<T>(&bytes)?;      // serde_path_to_error → 400 w/ JSON pointer
        let mut validation = ValidationCtx::new();
        value.validate(&mut validation).map_err(Error::validation)?;     // 422 w/ field pointers
        Ok(Json(value))
    }
}
```

Four documented statuses, not two: the original spec listed 400 and 422 only, and the shipped
extractor can also answer 413 (over the limit) and 415 (wrong `Content-Type`). Documenting a
response the server can send is the whole point.

Three properties FastAPI users expect and Axum does not give:
1. **Deserialisation errors carry a field path.** `serde_path_to_error` is mandatory, not optional.
   `{"detail":"invalid type: string, expected u32","pointer":"/items/2/quantity"}`.
2. **Validation is part of extraction.** There is no way to get a `CreateUser` out of a request
   without it having been validated. No `.validate()?` line to forget.
3. **The 422 shape is documented in OpenAPI**, generated from the same constraints - and only when
   `T::HAS_CONSTRAINTS`, so an unconstrained DTO does not document a 422 it can never return.

`Json<T>` is also a **response** type (`response::Json`), re-exported from the same module.

### `Query<T>` - the details that matter

Query strings are where frameworks quietly disagree. Moso's normative behaviour, tested row by row
in `extract/query.rs`:

| Case | Behaviour |
| --- | --- |
| `?tags=a&tags=b` | `Vec<String>` = `["a","b"]` |
| `?tags=a,b` | also `["a","b"]` if the field is `#[schema(delimiter = ",")]` |
| `?filter[status]=open` | nested struct via `#[schema(flatten_bracket)]` |
| missing field, `Option<T>` | `None` |
| missing field with `#[schema(default = …)]` | the default; documented in OpenAPI |
| missing field, required | 422 with `{"pointer":"/query/limit","code":"required"}` |
| `?limit=abc` for `u32` | 422 with `code: "type"`, not 400 |
| unknown parameter | ignored by default; `#[schema(deny_unknown)]` makes it 422 |
| `?flag` (no value) for `bool` | `true` |

**Pointers are rooted at `/query`.** A query parameter has no position in the request body that RFC
6901 could address, so `moso_core::extract::query::QUERY_POINTER_ROOT` (`"/query"`) is prefixed to
every pointer, for deserialisation and validation failures alike. `Headers<T>` and `Path<T>` use the
same scheme with their own roots.

A field declared with a delimiter still accepts the repeated-key form: a delimiter widens what a
field accepts rather than narrowing it.

### `Depends<T>` - request-scoped dependencies

See `15-dependency-injection.md` for the full model. From the extractor's point of view:

```rust
// spec
pub struct Depends<T: Dependency>(pub T);

#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a request dependency",
    note = "add `#[derive(moso::Dependency)]` to `{Self}`, or implement `Dependency` manually",
    note = "a dependency is resolved once per request and cached; \
            use `Inject<T>` instead for app-lifetime values like a database pool",
)]
pub trait Dependency: Clone + Send + Sync + 'static {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];
    fn describe(op: &mut OperationBuilder) { let _ = op; }
    fn resolve<'a>(ctx: &'a RequestCtx) -> impl Future<Output = Result<Self>> + Send + 'a;
}
```

`RequestCtx::depends` itself returns a `BoxFuture` rather than `impl Future`: dependency resolution
is recursive (a `resolve` body calls `ctx.depends::<Other>()`), and a recursive RPITIT would be an
infinitely-sized type.

## Responses

Moso re-exports Axum's `IntoResponse`, so the entire ecosystem's response types work. On top of it:

```rust
// spec - moso-core/src/response/mod.rs
pub trait Describe {
    fn describe(op: &mut OperationBuilder);
}
```

`Describe` is implemented for `Result<T, E>`, `Option<T>`, `()`, `(StatusCode, T)` and `Error`, so
the ordinary handler return type documents itself.

| Type | Status | Documents as | Notes |
| --- | --- | --- | --- |
| `T: Schema` | 200 | `T` | **requires `#[derive(Schema)]`** - see below |
| `Json<T>` | 200 | `T` | explicit form |
| `Created<T>` | 201 | `T` + `Location` header | `Created::at("/users/1", body)` |
| `Accepted<T>` | 202 | `T` | |
| `NoContent` | 204 | - | |
| `Empty` | 204 | - | alias, reads better in `Result<Empty>` |
| `Page<T>` | 200 | `{items: [T], next_cursor?, prev_cursor?, total?}` | the pagination envelope |
| `Redirect` | 302/303/307/308 | `Location` | |
| `File` / `Attachment` | 200 | binary | streams, content-type/disposition, Range |
| `Sse<S>` | 200 | `text/event-stream` | documented as an SSE operation |
| `Raw<T>` | as given | `{}` | escape hatch |
| `Text` / `Html` | 200 | `text/plain` / `text/html` | |
| `Result<T, Error>` | from `T` + error taxonomy | union of both | the normal handler return |
| `Either<A, B>` | union | `oneOf` | when one endpoint genuinely returns two shapes |
| `Cached<T>` | 200/304 | `T` + ETag | conditional-request handling |
| `(StatusCode, T)` | as given | `T` | Axum-compatible tuple form |

### Returning a bare `T: Schema` requires the derive to emit `IntoResponse`

`impl<T: Schema> IntoResponse for T` is an orphan-rule violation - `IntoResponse` is Axum's and `T`
is uncovered. So "returning a bare schema is fine" is true only because **`#[derive(Schema)]`
generates `IntoResponse` and `Describe` for the user's type**, using the helpers
`response::json_response::<T>(StatusCode, &T)` and `response::describe_json::<T>(op, status)`.

Consequences worth knowing:

- A hand-written `impl Schema for MyType` does **not** make `MyType` returnable. Add
  `#[derive(Responder)]`, or write `IntoResponse` yourself.
- `#[derive(Schema)]` suppresses its own `IntoResponse`/`Describe` when `#[derive(Responder)]` is
  also present, or when the container carries `#[schema(no_response)]` - otherwise the two derives
  would be a coherence error.

### The pagination envelope

Because every API needs it and every team invents a different one:

```rust
// spec
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<Cursor>,
    pub prev_cursor: Option<Cursor>,
    pub total: Option<u64>,          // only when the query asked for a count
}

pub struct Cursor(/* opaque, base64url */);   // moso_schema::Cursor
```

Cursor pagination is the default because offset pagination is wrong at scale;
`Page::from_offset(..)` exists and documents itself with `page`/`per_page` parameters. `PageLinks`
carries `next`/`prev`/`first` URLs for clients that prefer links to cursors.

The original text said "`moso-orm`'s `.paginate()` produces a `Page` directly". There is no ORM in
this build; `Page` is constructed by hand or from a slice.

### Custom response types

```rust
// example
#[derive(Schema, Responder)]
#[responder(status = 201, header(location = "self.url"))]
struct UserCreated {
    #[serde(skip)] url: String,
    id: Uuid,
    email: Email,
}
```

`#[derive(Responder)]` generates `IntoResponse` + `Describe`. Without it - and without
`#[derive(Schema)]` - a non-response type in return position gives:

```
error[E0277]: `UserCreated` cannot be returned from a handler
  --> src/routes/users.rs:33:6
   |
33 | ) -> Result<UserCreated> {
   |      ^^^^^^^^^^^^^^^^^^^ this type is not a response
   |
   = help: add `#[derive(moso::Schema)]` to return it as a 200 JSON body
   = help: or add `#[derive(moso::Responder)]` to control the status and headers
   = help: or implement `moso::IntoResponse` manually for full control
```

## Content negotiation

**Not implemented.** The original design specified
`Router::new().negotiate([Format::Json, Format::MsgPack, Format::Cbor])` and per-`Accept` encoding
of `T: Schema`. Neither `Router::negotiate` nor a `Format` enum exists in this build: JSON is the
only representation, and `Error` negotiates only between `application/problem+json` and
`application/json`. Adding it later is additive - a new `Router` method and a new
`Describe`/`IntoResponse` path - so nothing here forecloses it.

## Limits and safety defaults

| Limit | Default | Config key | Field on `HttpConfig` |
| --- | --- | --- | --- |
| Body size | 2 MiB | `http.body_max` | `body_max` |
| Multipart total | 32 MiB | `http.multipart_max` | `multipart_max` |
| Multipart per-file | 16 MiB | `http.multipart_file_max` | `multipart_file_max` |
| Header count | 100 | `http.header_max_count` | `header_max_count` |
| Header bytes | 16 KiB | `http.header_max_bytes` | `header_max_bytes` |
| URI length | 8 KiB | `http.uri_max` | `uri_max` |
| Query depth (nested) | 8 | `http.query_depth_max` | `query_depth_max` |
| JSON nesting depth | 64 | `http.json_depth_max` | `json_depth_max` |
| Request timeout | 30 s | `http.timeout` | `timeout` |

The original table showed one `http.header_max` key; the implementation splits count from bytes,
because they fail differently (431 for either, but the fix is different).

Exceeding a limit produces a documented problem response - 413 from `body_limit` and the body
extractors, 414 and 431 from `Slot::RequestLimits`, 400 with a `max_depth` member from
`query_depth_max` and `json_depth_max` - never a panic and never a silent truncation. `Json<T>`
reads with a hard cap *before* deserialising - no "allocate 4 GB then fail" - via
`extract::read_limited`, then checks nesting with `extract::check_json_depth` before `serde_json`
sees a byte.

There is no 408, deliberately: it would need a read deadline on the request *body* distinct from
`http.timeout`, there is no key for one, and the `timeout` slot already answers an unfinished
request with a 504.

Limits reach a handler through `ctx.limits()`, a `Limits` struct built from `HttpConfig` at boot, so
an extractor never reads global state.

## Writing a custom extractor

The extension path must be pleasant, since "the framework doesn't have my thing" is the top reason
people abandon batteries-included frameworks.

```rust
// example - an extractor for a tenant resolved from a subdomain
pub struct Tenant(pub TenantId);

impl Extract for Tenant {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<Db>()];

    fn describe(op: &mut OperationBuilder) {
        op.parameter(Param::header("x-tenant").required(false).schema_of::<String>());
        op.response(404, ResponseSpec::problem("Unknown tenant"));
    }

    async fn extract(parts: &mut Parts, ctx: &RequestCtx) -> Result<Self> {
        let host = parts.headers.get(HOST).and_then(|h| h.to_str().ok())
            .ok_or_else(|| Error::bad_request("missing Host header"))?;
        let sub = host.split('.').next().unwrap_or_default();
        let db: Arc<Db> = ctx.provider::<Db>()?;
        Tenant::lookup(&db, sub).await?.ok_or_else(|| Error::not_found("tenant"))
    }
}
```

Two spellings changed from the original sketch and both matter:

- `Param::…​.schema_of::<T>()` (deferred), not `.schema::<T>(op.generator())`. The eager form does
  not compile inside `op.parameter(..)`, because that call already holds `&mut op`. Use
  `schema_of` in argument position, or hoist:
  `let s = op.generator().subschema_for::<T>(); op.parameter(Param::query("x").schema_node(s));`
- `ctx.provider::<T>()` returns `Result<Arc<T>>`, not `&T`. It is fallible in the *signature* only;
  boot proved the provider exists, which is why `Inject<T>` can present it as infallible.

## Acceptance criteria (WP-03, WP-06)

1. ✅ `Json<T>` deserialisation failure yields 400 with a JSON Pointer; validation failure yields 422
   with per-field codes. Both shapes appear in the generated OpenAPI.
2. ✅ Every extractor in the table has a `describe()` test asserting its OpenAPI contribution.
3. ✅ `Opaque<axum::extract::…>` compiles in a Moso handler; a Moso extractor compiles in an Axum
   handler **via `MosoExt` / `MosoExtBody`**, not via a blanket impl.
4. ⚠️ Body-size limit is enforced before allocation (`read_limited` checks `Content-Length` first
   and caps the streaming read). The *peak-RSS* form of the criterion is not measured - there is no
   benchmark harness in this build.
5. ✅ `on_unimplemented` messages are written for every public trait; the `trybuild` corpus lives in
   `crates/moso-ui-tests`.
6. ✅ The query-string behaviour table is covered case by case, one test per row.
