---
title: Responses
description: Every type a handler can return, what each one puts on the wire, and what each one contributes to the OpenAPI document.
order: 4
status: shipped
---

A handler's return type is the other half of its contract. It decides the status code, the headers
and the body a client receives, and it decides what the generated OpenAPI document says the
operation returns. Those two are the same decision in Moso: the type that sends a 201 is the type
that documents a 201, so they cannot drift.

This page covers every response type the framework ships, the exact bytes and headers each one
produces, what each one writes into the document, and how to add your own when none of them fits.

## The smallest thing that works

```rust
use moso::prelude::*;

/// A post, as the API returns one.
#[derive(Schema)]
pub struct PostOut {
    /// URL-safe identifier.
    pub slug: Slug,
    /// Bumped on every edit.
    pub version: u32,
}

/// Show a post.
#[endpoint]
async fn show(Path(slug): Path<Slug>) -> Result<PostOut> {
    Ok(PostOut { slug, version: 1 })
}
```

That sends `200 OK`, `content-type: application/json` and the serialised struct. In the document the
operation gets a 200 whose schema is `PostOut`, plus the 500 and 503 that `Error` contributes through
the `Err` arm. Nothing is annotated.

Returning a bare schema type works because `#[derive(Schema)]` generates `IntoResponse` and
`Describe` for that type. A blanket `impl<T: Schema> IntoResponse for T` is an orphan rule violation,
so the derive emits the impls per type instead. Two consequences worth knowing:

- A hand-written `impl Schema for MyType` does **not** make `MyType` returnable. You need the derive,
  or your own `IntoResponse` and `Describe`.
- The derive suppresses its own response impls when `#[derive(Responder)]` is also present or when
  `#[schema(no_response)]` is set, because two sources of `IntoResponse` for one type is a coherence
  error.

## What a return type has to satisfy

`#[endpoint]` requires one bound, `HandlerReturn`, and there is exactly one impl of it:

```rust
#[diagnostic::do_not_recommend]
impl<T: IntoResponse + Describe> HandlerReturn for T { /* the only impl */ }
```

`IntoResponse` is Axum's, re-exported unchanged, so anything that is an Axum response is a Moso
response. `Describe` is the addition: it says what the response means for the API contract.

The two are bounds on a blanket impl rather than supertraits so that a return type which satisfies
neither produces one hand-written diagnostic instead of two, one of which would be rustc's own
`IntoResponse` message listing `&'static [u8; N]` among the implementers:

```text
error[E0277]: `MyType` cannot be returned from a handler
   |
   = help: the trait `HandlerReturn` is not implemented for `MyType`
   = note: help: add `#[derive(moso::Schema)]` to return it as a 200 JSON body
   = note: help: or add `#[derive(moso::Responder)]` to control the status and the headers
   = note: help: or wrap it: `Json<MyType>`, `Created<MyType>`, `Page<MyType>`, `Raw<MyType>`
   = note: a handler usually returns `Result<T>`, which documents `T` and the error taxonomy together
```

`Describe::describe` runs once per route at `App::build()`, against a fresh `OperationBuilder`.
Nothing runs it per request, so documentation costs nothing at runtime.

## Every response type

| Type | Import | On the wire | In the document |
| --- | --- | --- | --- |
| `T: Schema` | your own type | 200, `application/json`, serialised | 200 with `T`'s schema |
| `Json<T>` | prelude | the same, spelled out | the same |
| `Created<T>` | prelude | 201 plus `Location` | 201 with `T`'s schema and a `Location` header |
| `Accepted<T>` | `moso::response::Accepted` | 202, `Location` optional | 202 with `T`'s schema |
| `NoContent` (alias `Empty`) | prelude | 204, no body, no headers at all | 204 empty |
| `Page<T>` | prelude | 200 JSON envelope | 200 with the component `Page_T` |
| `Redirect` | `moso::response::Redirect` | 302, 303, 307 or 308 with `Location` | all four statuses, each with `Location` |
| `File` | `moso::response::File` | 200 streamed, or 206, or 304 | 200, 206, 304, 404, 416 |
| `Attachment` | `moso::response::Attachment` | 200 with `Content-Disposition` | 200 binary |
| `Cached<T>` | `moso::response::Cached` | `T` plus `ETag` and `Cache-Control`, or a bodyless 304 | `T`'s 200 plus those headers, plus 304 |
| `Sse<S>` | `moso::response::Sse` | 200 `text/event-stream`, streamed | 200 event stream plus a `Last-Event-ID` parameter |
| `Either<A, B>` | `moso::response::Either` | whichever arm you returned | the union of both, `oneOf` on a shared status |
| `Text` | `moso::response::Text` | 200 `text/plain; charset=utf-8` | 200 string |
| `Html` | `moso::response::Html` | 200 `text/html; charset=utf-8` | 200 string |
| `Bytes` | `moso::response::Bytes` | 200 `application/octet-stream` | 200 binary |
| `Raw<T>` | `moso::response::Raw` | whatever `T` produces | 200 under `*/*` with an empty schema |
| `Result<T, E>` | core | the `Ok` arm, or the error as `problem+json` | the union of both arms |
| `()` | core | 200 with no body | 200 empty |
| `(StatusCode, T)` | core | that status with `T`'s body | `T`'s responses only |
| `Error` | prelude | RFC 9457 `application/problem+json` | 500 and 503 |

Everything in that table composes. `Result<Created<Cached<Json<PostOut>>>>` is a legal return type
and every layer contributes.

## JSON bodies

`Json<T>` is both an extractor and a response. As a response it serialises `T` at 200 with
`content-type: application/json`. It is exactly what a bare `T: Schema` does, so use whichever reads
better: `Json<T>` when the handler already spells out other wrappers, the bare type otherwise.

```rust
use moso::prelude::*;

/// Show a post.
#[endpoint]
async fn show(Path(slug): Path<Slug>) -> Result<Json<PostOut>> {
    Ok(Json(PostOut { slug, version: 1 }))
}
```

Serialisation is not allowed to panic a request: a failure inside `json_response` becomes a 500 with
the usual problem body.

## 201 Created and 202 Accepted

```rust
use moso::prelude::*;

/// Create a post.
#[endpoint]
async fn create(Json(body): Json<CreatePost>) -> Result<Created<PostOut>> {
    let slug = Slug::from_title(&body.title).ok_or_else(|| Error::bad_request("empty title"))?;
    let location = format!("/api/v1/posts/{slug}");
    Ok(Created::at(location, PostOut { slug, version: 1 }))
}
```

`Created::at` takes the location first because a 201 without one is a documented interoperability
problem, and making you pass an empty string to opt out is a worse trade than making it easy to
forget. `Created::without_location(body)` is the deliberate opt-out.

`Accepted<T>` is the same shape for a 202: `Accepted::new(body)`, or `Accepted::at(status_url, body)`
when the client should poll something.

Three behaviours to know:

- **An inner error status wins.** `Created::at("/x", some_error)` renders the error's status, not
  201. A 500 wrapped in a 201 would be a lie.
- **The 200 is restaged only when this call caused it.** `Created<T>` documents `T` at 201 by moving
  the 200 that `T` documented. If an extractor had already put a 200 on the operation, that 200 stays
  where it is and the 201 is documented as empty.
- **A `Location` containing a newline is dropped**, not sent. The 201 still carries the
  representation, so it degrades to something a client can use.

## 204 No Content

```rust
use moso::prelude::*;
use moso::response::NoContent;

/// Delete a post.
#[endpoint]
async fn destroy(Path(_slug): Path<Slug>) -> Result<NoContent> {
    Ok(NoContent)
}
```

A 204 with no headers at all, which is what a `DELETE` should answer. `Empty` is an alias for the
same type, and `Result<Empty>` reads better than `Result<NoContent>` in some signatures. Prefer it to
returning `()`: `()` is a 200 with an empty body, which is a weaker statement.

## Pagination

`Page<T>` is the one pagination envelope. Every listing in every Moso API has the same shape, which
is what lets a client write one pagination helper instead of one per endpoint.

```json
{
  "items": [],
  "next_cursor": "eyJpZCI6...",
  "prev_cursor": null,
  "total": 1042
}
```

The envelope is snake_case. Every member past `items` is omitted entirely when it is `None`, so a
full result set serialises as `{"items": [...]}`, and which members are present is the signal for
which pagination style is in use:

| Style | Members |
| --- | --- |
| Cursor | `items`, `next_cursor?`, `prev_cursor?` |
| Offset | `items`, `total`, `page`, `per_page` |
| The whole set | `items` |

So a client that sees no `next_cursor` on a full page is not looking at the last page. It is
looking at an offset page, and `page`, `per_page` and `total` tell it where it is and how far it has
to go.

The intended construction is one query for `limit + 1` rows:

```rust
// `page` returns `limit + 1` rows; the extra one is what tells
// `Page::from_items` there is a next page, so no second query is issued.
let rows = store.page(&filter, after, limit);
let total = store.count(&filter);

Ok(Page::from_items(rows, limit, |post| post.key().to_cursor())
    .map(PostOut::from)
    .with_total(total))
```

`from_items` keeps `limit` rows, mints a cursor from the last one it kept, and drops the extra. A
short read is the last page and costs no cursor, so the closure is not called.

| Constructor or combinator | What it does |
| --- | --- |
| `Page::new(items)` | The whole result set, no cursors, no total |
| `Page::empty()` | No rows |
| `Page::from_items(items, limit, cursor_for)` | The `limit + 1` idiom above |
| `Page::from_offset(items, page, per_page, total)` | Offset pagination, carrying the page it is |
| `page.with_next(c)`, `with_prev(c)`, `with_total(n)`, `with_offset(p, n)` | Add one field |
| `page.map(f)`, `page.try_map(f)` | Turn `Page<Row>` into `Page<RowOut>` |

Cursors are the default because `OFFSET 100000` makes the database walk a hundred thousand rows, and
a row inserted between two requests shifts every later page, so a scanning client silently skips
records. `total` is optional because counting is a second query and, on a large filtered table, the
expensive one, but an offset page always carries it, because a page number without a page count
is not usable.

`page` and `per_page` are echoes of what the request asked for, so a page-number UI can render
"page 3 of 42" from one response instead of remembering what it sent. They are **response** members.
The **request** parameters of the same name are not `Page`'s to declare: they arrive on your
handler's own `Query<T>`, and that signature is the one home for what the operation accepts. `Page`
is
`#[non_exhaustive]`, so build one with the constructors above rather than a struct literal.

### Link headers

`PageLinks` renders RFC 8288 `Link` values for a page. It preserves every other query parameter and
rewrites only `cursor`, so a filtered listing stays filtered across pages.

```rust
use moso::prelude::*;
use moso::response::PageLinks;

let links = PageLinks::for_page(&page, &uri);
let header = links.to_header();
```

`first` is always present, because the same request with no cursor is always a valid first page.

### Signed cursors

A cursor is a query parameter the client can edit, and it encodes a sort key that goes straight into
a `WHERE` clause. `CursorCodec` signs one with a truncated HMAC-SHA256 so editing it fails.

```rust
use moso::prelude::*;
use moso::response::cursor::CursorCodec;

/// List posts, page by page.
#[endpoint]
async fn list(
    Inject(cursors): Inject<CursorCodec>,
    Query(q): Query<ListQuery>,
) -> Result<Page<PostOut>> {
    let after: Option<u64> = q
        .cursor
        .map(|c| cursors.verify_value("posts", &c))
        .transpose()?;
    let _ = after;
    Ok(Page::new(Vec::new()).with_next(cursors.sign_value("posts", &42_u64)?))
}
```

Register the codec once with `App::provide` (see [dependency injection](./dependency-injection.md)).
The scope string is mixed into the tag and never transmitted, so a cursor minted for `"posts"` fails
against `"comments"` without either endpoint checking anything. Every failure mode (truncated,
edited, wrong scope, wrong secret) produces one indistinguishable 400, because telling an attacker
which part of a token failed is how a forgery oracle starts.

`CursorCodec::new` accepts a secret of any length because HMAC normalises it, but a secret shorter
than 32 bytes gives the tag less strength than its length suggests. `moso doctor` reports that.

## Redirects

The constructors are named after semantics rather than numbers, because the numbers are famously
confusing and picking the wrong one changes whether a `POST` is replayed.

| Constructor | Status | Method preserved | Cacheable |
| --- | --- | --- | --- |
| `Redirect::to` | 303 See Other | no, becomes `GET` | no |
| `Redirect::temporary` | 307 | yes | no |
| `Redirect::permanent` | 308 | yes | yes |
| `Redirect::found` | 302 | in practice, no | no |

```rust
use moso::prelude::*;
use moso::response::Redirect;

/// Send a browser somewhere else after a form post.
#[endpoint]
async fn login() -> Result<Redirect> {
    Ok(Redirect::to("/dashboard"))
}
```

`Describe` sees the type, not the value, so it documents all four statuses. A handler that only ever
returns one can narrow the document with `#[endpoint(response(303, "Signed in"))]`.

A location with a control character or newline in it becomes a **500**, not a `Location`-less 3xx. A
3xx with no `Location` carries nothing a client can act on, so failing loudly is the better answer.

## Files and downloads

```rust
use moso::prelude::*;
use moso::deps::http::HeaderMap;
use moso::response::File;

/// Download a stored report.
#[endpoint]
async fn download(Path(id): Path<u64>, headers: HeaderMap) -> Result<File> {
    Ok(File::open(storage_path(id)).await?
        .attachment("report.pdf")
        .evaluate(&headers))
}
```

`File::open` sets `Content-Type` from the extension (34 known extensions, falling back to
`application/octet-stream`), `Content-Length` from the metadata, `Last-Modified`, `Accept-Ranges`,
`Content-Disposition` and a strong `ETag`. Builders: `content_type`, `attachment(filename)`,
`inline`, and `evaluate(&headers)`.

`evaluate` is what adds `If-None-Match`, `If-Range` and `Range` handling, so 304 and 206 and 416
become possible. Forgetting it is not a bug, just a permanent 200 that cannot resume.

The body streams in 64 KiB chunks through `tokio::task::spawn_blocking` with a queue bounded at two,
so a large file costs about 128 KiB of memory regardless of size.

Range support handles `bytes=N-M`, `bytes=N-` and `bytes=-N`. Multiple ranges and non-`bytes` units
are ignored and the whole representation is sent, which RFC 9110 permits and every client falls back
from cleanly. `multipart/byteranges` is deliberately not implemented.

> [!CAUTION]
> `File::open` does not sanitise its argument. It is for a server-side path the handler chose.
> Serving a client-supplied path is what `Router::static_files` is for, and that refuses traversal.
> Building a path by concatenating a request parameter is the bug this note exists to name.

For bytes you generated rather than read, use `Attachment`:

```rust
use moso::prelude::*;
use moso::response::Attachment;

/// Export the ledger.
#[endpoint]
async fn export() -> Result<Attachment> {
    Ok(Attachment::csv("ledger.csv", "date,amount\n2026-01-01,10\n"))
}
```

`Attachment::new(filename, content_type, body)` is the general form. Both emit a sanitised ASCII
`filename=` and an RFC 5987 `filename*=`, so a non-ASCII name survives.

## Caching and conditional responses

`Cached<T>` wraps any response and adds validators.

```rust
use moso::prelude::*;
use moso::deps::http::HeaderMap;
use moso::response::{Cached, ETag};
use std::time::Duration;

/// Show a post, letting a repeat visitor skip the body.
#[endpoint]
async fn show(Path(slug): Path<Slug>, headers: HeaderMap) -> Result<Cached<Json<PostOut>>> {
    let post = PostOut { slug, version: 3 };
    let etag = ETag::strong(post.version);
    Ok(Cached::new(Json(post))
        .etag(etag)
        .max_age(Duration::from_secs(60))
        .evaluate(&headers))
}
```

`IntoResponse` is handed a value and nothing else, so a response type cannot read a request header on
its own. That is why `evaluate` takes the `HeaderMap` explicitly and why the handler takes one.

| Builder | Effect |
| --- | --- |
| `.etag(ETag)` | Sets `ETag` and enables `If-None-Match` |
| `.last_modified(SystemTime)` | Sets `Last-Modified` and enables `If-Modified-Since` |
| `.max_age(Duration)` | Adds `max-age=` to `Cache-Control` |
| `.visibility(Visibility)` | `Public`, `Private` (the default) or `NoStore` |
| `.evaluate(&headers)` | Arms the 304 |
| `.is_not_modified()` | Whether the 304 will fire |

`Cache-Control` renders as `private`, `public, max-age=3600`, `no-store` and so on. `no-store` is
never softened by a `max-age`.

Build tags with `ETag::strong(value)`, `ETag::weak(value)` or `ETag::from_bytes(bytes)`. The last one
is FNV-1a over the bytes, not a cryptographic digest, and is documented as such: an entity tag is a
cache key, not a signature.

`If-None-Match` takes absolute precedence when present, including when it fails to match, as RFC 9110
section 13.1.3 requires. A timestamp has one-second resolution and an entity tag does not, so letting
the coarser check override the finer one is how a client caches a body it was explicitly told had
changed.

A 304 never renders `T` at all, so a `Cached<Json<Expensive>>` that fires costs no serialisation.

## Server-sent events

```rust
use moso::prelude::*;
use moso::response::sse::{Event, Sse};
use futures_util::{Stream, stream};
use std::pin::Pin;
use std::time::Duration;

/// A stream of events, named concretely so it can appear in a signature.
pub type Events = Pin<Box<dyn Stream<Item = Result<Event>> + Send>>;

/// Stream progress to the browser.
#[endpoint]
async fn progress() -> Result<Sse<Events>> {
    let events = stream::iter([Ok(Event::data("started")), Ok(Event::data("done"))]);
    Ok(Sse::new(Box::pin(events) as Events).keep_alive(Duration::from_secs(15)))
}
```

The response sets `content-type: text/event-stream`, `cache-control: no-cache` and
`x-accel-buffering: no`, the last so an nginx in front of you does not buffer the stream into
uselessness. Compression is skipped for `text/event-stream` regardless of the `compression` feature.

`Event` builders: `Event::data(s)`, `Event::json(&value)`, `Event::comment(text)`, then `.named(..)`,
`.with_id(..)`, `.with_retry(Duration)`. CR and LF are stripped from the event name, the id and
comments so a value cannot inject a frame, and `data` is split into one `data:` line per line.

An `Err` in the stream becomes a terminal `error` event carrying `type`, `title` and `status`, with
`detail` included only when the error kind says the detail is client-safe.

For resumption, read `moso::response::sse::last_event_id(&headers)`: browsers send `Last-Event-ID`
automatically on reconnect, and `Describe` documents it as a header parameter on the operation.

> [!IMPORTANT]
> An SSE handler outlives a normal request, so it must cooperate with shutdown: take an
> `Inject<Signal>` and stop when it fires. The framework logs a warning naming any route still
> streaming when the grace period ends. Also note the stream type has to be nameable, because
> `#[endpoint]` writes `<ReturnType as Describe>::describe(..)` and `impl Trait` cannot appear in a
> path. A boxed alias is the shape that compiles. Your application adds `futures-util` to its own
> manifest for the stream combinators.

## One operation, two shapes

```rust
use moso::prelude::*;
use moso::response::{Either, Redirect};

/// Show a post, following a move if there was one.
#[endpoint]
async fn show(Path(id): Path<u64>) -> Result<Either<Json<PostOut>, Redirect>> {
    match find(id) {
        Post::Current(post) => Ok(Either::A(Json(post))),
        Post::Moved(slug)   => Ok(Either::B(Redirect::permanent(format!("/posts/{slug}")))),
    }
}
```

Each arm is described against the same baseline and the two results are folded together afterwards,
so two arms that both document a 200 become a `oneOf` rather than one silently winning. Identical
schemas do not produce a pointless union, and nested `Either`s flatten into a single `oneOf`.

Do not use `Either` as a poor person's error type. An error belongs in the `Err` arm, where the
taxonomy documents it and the boundary logs it. See [errors](./errors.md).

## Text, HTML, bytes and raw

| Type | Content type |
| --- | --- |
| `Text(String)` | `text/plain; charset=utf-8` |
| `Html(String)` | `text/html; charset=utf-8` |
| `Bytes(bytes::Bytes)` | `application/octet-stream` |

`Text` and `Bytes` are the same types you use as body extractors, re-exported under
`moso::response` so the import reads correctly in return position.

> [!WARNING]
> `Html` performs no escaping. It is an assertion that the bytes are already HTML, so interpolating a
> request value into one is exactly the injection you would expect. `moso::response::text::escape_html`
> covers `&`, `<`, `>`, `"` and `'`, and it is not a templating solution.

`Raw<T>` returns anything at all and documents itself honestly:

```rust
use moso::prelude::*;
use moso::response::Raw;
use moso::Response;

/// Forward an upstream response verbatim.
#[endpoint]
async fn proxy() -> Result<Raw<Response>> {
    Ok(Raw(upstream()))
}
```

It documents a 200 under `*/*` with an empty schema and the `x-moso-raw-response` extension, so a
lint can count the operations that opted out rather than guess. Naming a concrete media type would be
a guess dressed up as a fact. If you reach for `Raw` often, the response is under-modelled, and
`#[derive(Responder)]` covers most of what people use it for.

## Controlling status and headers on your own type

```rust
use moso::prelude::*;

/// A user that has just been created.
#[derive(Schema, Responder)]
#[responder(status = 201, header(location = "self.url"))]
pub struct UserCreated {
    /// Sent as `Location`, not in the body.
    #[serde(skip)]
    pub url: String,
    /// Stable identifier.
    pub id: u64,
    /// Contact address.
    pub email: Email,
}
```

The derive emits `IntoResponse` and `Describe` from the same attribute, so the status a handler sends
and the status the document claims cannot disagree.

| Key | Form | Meaning |
| --- | --- | --- |
| `status` | `status = 201` | The status sent and documented. 100 to 999. Defaults to 200. |
| `description` | `description = "..."` | The response description. Falls back to the doc comment, then to a sentence-cased type name. |
| `header` | `header(location = "self.url")` | Repeatable. The value is a Rust expression with `self` in scope. `_` in the key becomes `-` on the wire. Setting the same header twice is a compile error. |

`Responder` is for structs. An enum is a compile error that points you at `Either`.

## Composing: Result, unit, tuple

`Result<T, E>` documents the union of both arms. `Result<T>` is `Result<T, Error>`, and
`Describe for Error` is deliberately conservative: it contributes only 500 and 503, the two statuses
every operation can genuinely return. Operation-specific errors come from the extractors that raise
them and from `#[endpoint(errors = MyError)]`, so a document does not claim every endpoint can return
a 409.

`()` documents a 200 with no body. Prefer `NoContent`, which is a 204 and says so.

`(StatusCode, T)` matches Axum's tuple form and sends that status with `T`'s body. Its `Describe`
delegates to `T` alone, so **the status in the tuple is not documented**. Use `Created`, `Accepted`
or `#[derive(Responder)]` when the status matters to the contract, and keep the tuple for the cases
where it does not.

> [!WARNING]
> `Option<T>` is **not** returnable. `Describe for Option<T>` exists and documents `T` plus a 404, but
> Axum does not implement `IntoResponse for Option<T>`, so `Result<Option<Json<T>>>` fails the
> `HandlerReturn` bound and does not compile. Write `Result<Json<T>>` and raise
> `Error::not_found("post")` instead, which produces the same 404 with a problem body that names what
> was missing.

## Status and header rules

These apply to every response type above.

- **A 204, a 304 and any 1xx never carry a body.** `json_response` drops the body and omits the
  `Content-Type` for those statuses even if you hand it a value, so a proxy cannot disagree about
  where the next response starts.
- **A header value that is not header-safe is dropped, not sent.** `set_header` silently skips it.
  The two exceptions are described above: `Redirect` turns a bad location into a 500, and `Created`
  drops the `Location` and keeps the 201.
- **The status a wrapper claims never overwrites an inner error status.** `Created` and `Accepted`
  both preserve a 4xx or 5xx coming from inside.
- **Errors render as RFC 9457** `application/problem+json` carrying the request id, whatever the
  handler's declared return type was.

## Writing your own response type

Implement `IntoResponse` for the bytes and `Describe` for the contract. `Describe` has no default
body on purpose: a default of "contributes nothing" would silently produce wrong documentation for
exactly the type that most needed to speak up.

```rust
use moso::prelude::*;
use moso::openapi::{ContentType, OperationBuilder, ResponseSpec};
use moso::response::Describe;
use moso::schema::json_schema::{JsonType, SchemaNode};
use moso::Response;

/// A `text/csv` export.
pub struct Csv(pub String);

impl IntoResponse for Csv {
    fn into_response(self) -> Response {
        ([("content-type", "text/csv")], self.0).into_response()
    }
}

impl Describe for Csv {
    fn describe(op: &mut OperationBuilder) {
        op.response(
            200,
            ResponseSpec::with_content(
                ContentType::custom("text/csv"),
                SchemaNode::of_type(JsonType::String),
            )
                .description("A CSV export"),
        );
    }
}
```

The helpers the derives use are public, so a JSON-shaped custom type can reuse them:
`json_response(status, &value)`, `describe_json::<T>(op, status)`, `empty_response(status)` and
`set_header(&mut response, name, value)`, all in `moso::response`.

## Failure modes worth knowing

- **A response type cannot see the request.** `Cached::evaluate` and `File::evaluate` exist because of
  that. Take a `HeaderMap` in the handler and hand it over. Forgetting the call is not an error, just
  a permanent 200.
- **`Describe` sees the type, not the value.** `Redirect` documents four statuses even when your
  handler only sends one. Narrow it with `#[endpoint(response(...))]` if the document matters more
  than the two extra lines.
- **`Created` does not always restage the 200.** If an extractor documented a 200 first, the body
  stays there and the 201 is documented as empty. Check the generated document if a 201 looks bare.
- **Moso is JSON-first, by choice.** There is no `Router::negotiate`, `Format` enum or content
  negotiation: JSON is the representation, and `Error` chooses only between `application/problem+json`
  and `application/json`. Adding negotiation later would be purely additive, so nothing here
  forecloses it.
- **There is no `Xml<T>` response**, in keeping with the JSON-first scope, even though
  `ContentType::Xml` and the `Xml` name are reserved in the macro's body-extractor list.
- **A `Page`'s doc comment shows a camelCase envelope.** The serde attributes and the tests produce
  snake_case. `next_cursor` is the wire name.
- **`ETag::from_bytes` hashes the bytes you give it**, not the response the framework eventually
  sends. If you build a tag from a body and then wrap it in something that changes the body, the tag
  is wrong.

## See also

- [Extractors](./extractors.md) for the other half of the handler signature.
- [Schemas](./schemas.md) for the derive that makes a type returnable.
- [Errors](./errors.md) for the problem document every failure renders as.
- [OpenAPI](./openapi.md) for what `describe` writes into and how the document is assembled at boot.
- [Server sent events and realtime](./realtime.md) for the streaming side in full.
- [File storage](./file-storage.md) for serving objects you did not put on local disk yourself.
- [Extractors](./extractors.md) for the request side of the same handler signature.
