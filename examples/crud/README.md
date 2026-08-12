# `examples/crud` — the tutorial application

A small blog API: create, read, edit, publish and delete posts, with validated
input, cursor pagination, an API key on the writes, a per-request identity, a
custom error type, and an OpenAPI 3.1 document that nobody wrote.

There is no database. Posts live in a `RwLock<HashMap<Id<Post>, Post>>` provided
once at boot, so the example runs with `cargo run` and nothing else — and so
every line in it is about the framework rather than about SQL.

```text
cargo run -p example-crud
open http://localhost:3000/docs
```

```text
curl localhost:3000/api/v1/posts

curl -X POST localhost:3000/api/v1/posts \
  -H 'content-type: application/json' \
  -H 'x-api-key: let-me-in' \
  -H 'x-author: ada' \
  -d '{"title":"Hello","body":"My first post","tags":["rust"],"publish":true}'
```

---

## The tour

| File | What it is for |
| --- | --- |
| `src/main.rs` | five lines, forever |
| `src/lib.rs` | the composition root: everything the application *is* |
| `src/config.rs` | `#[derive(Config)]` — typed, layered, with a boot-time report |
| `src/models/post.rs` | the domain type, three DTOs, and the pagination key |
| `src/store.rs` | the in-memory store, and the only file a real database would touch |
| `src/routes/posts.rs` | six handlers and the route table |
| `src/routes/health.rs` | a plain endpoint reading three providers |
| `src/auth.rs` | a guard, a dependency, and a derived dependency |
| `src/middleware.rs` | `#[middleware]` — one function, one Tower layer |
| `src/error.rs` | `#[derive(moso::Error)]` — the domain's failures as problem documents |
| `openapi.json` | generated, committed, and checked by a test |

---

## 1. The composition root

`src/lib.rs` is one expression. Read it and you know the whole application: its
configuration, its providers, its routes, its middleware, its readiness probe
and its API metadata.

```rust
App::new(config)
    .provide(Store::new())
    .provide(Metrics::default())
    .mount(routes::router().layer(ObserveLayer::new()))
    .server_config(ServerConfig { bind, ..ServerConfig::default() })
    .health_check("store", StoreIsReachable)
    .openapi(|document| { /* title, version, server, security scheme */ })
    .build()
```

`build()` is where the checking happens. It walks the route table and proves
that every `Inject<T>` any handler — or any middleware — asks for has a
provider, that every path pattern is well formed, that no two routes collide and
that no two operations share an id. It returns **all** the problems at once.
Delete the `.provide(Store::new())` line and the program does not start:

```text
error: application failed to build (1 problem)

  x missing provider: `example_crud::store::Store`
      required by  GET /status                      examples/crud/src/routes/health.rs:36
                   GET /api/v1/posts                examples/crud/src/routes/posts.rs:57
                   GET /api/v1/posts/{id}           examples/crud/src/routes/posts.rs:102
                   POST /api/v1/posts               examples/crud/src/routes/posts.rs:87
                   …
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Store = /* construct it */;
                   App::new(config).provide(value)
```

That is a whole class of production incident moved to the second before the
listener binds.

## 2. Configuration

```rust
#[derive(Config, Debug)]
pub struct AppConfig {
    #[config(default = "moso blog")]           pub name: String,
    #[config(default = "0.0.0.0:3000")]        pub bind: SocketAddr,
    #[config(default = "http://localhost:3000")] pub public_url: Url,
    #[config(default = "let-me-in", secret)]   pub api_key: SecretString,
    #[config(nested)]                          pub posts: PostsConfig,
}
```

Every field has a default, so the example runs with an empty environment.
Override any of them:

```text
NAME="my blog" BIND=127.0.0.1:8080 POSTS__PAGE_SIZE=5 cargo run -p example-crud
```

`#[config(secret)]` requires the field to be a `SecretString` and redacts it in
`Debug`, in `moso config` and in log lines. `#[config(range = 1..=100)]` on
`posts.page_size` is checked *after* coercion, so `POSTS__PAGE_SIZE=0` is a boot
error naming the key rather than a division by zero at request time.

Tests use `AppConfig::defaults()`, which loads from a `ConfigLoader` with no
sources at all — so a variable exported in somebody's shell cannot change what
the test suite asserts.

## 3. The model layer: one attribute, two outputs

```rust
#[derive(Schema, Debug, Clone, PartialEq, Eq)]
pub struct CreatePost {
    /// Headline, shown in listings.
    #[schema(len = 3..=200, trim)]
    pub title: String,

    /// Free-form tags, at most five, each a short lower-case word.
    #[schema(default, len = ..=5, each(len = 2..=20, pattern = r"^[a-z0-9-]+$"))]
    pub tags: Vec<String>,
    …
}
```

`len = 3..=200` produces the runtime check **and** the `minLength`/`maxLength`
in `openapi.json`, from the same parsed attribute. They cannot drift apart,
because there is nowhere for them to drift to.

A bad body never reaches a handler. `Json<CreatePost>` reads the body under a
byte cap, deserialises it, validates it, and only then does the handler run — so
there is no way to obtain a `CreatePost` that has not been validated:

```json
{
  "type": "https://moso.rs/errors/validation",
  "title": "Validation Failed",
  "status": 422,
  "detail": "2 fields failed validation",
  "errors": [
    { "pointer": "/title",  "code": "len", "params": { "min": 3, "max": 200 } },
    { "pointer": "/tags/1", "code": "pattern" }
  ],
  "request_id": "01KYQ…"
}
```

RFC 9457 `application/problem+json`, with RFC 6901 JSON Pointers — including the
index of the offending element. A client can highlight the right form field
without parsing prose.

**400 and 422 mean different things here.** A body that is not JSON, or that is
missing a required member, never becomes a `CreatePost`: that is a
deserialisation failure and a `400`. A body that parsed and then broke a rule is
a `422`. The distinction is what lets a client tell "my serialiser is wrong"
from "my data is wrong".

### `Post` is not a `Schema`, on purpose

The domain type does not implement `Schema`, so it cannot be returned from a
handler. `PostOut` is the projection, and `#[schema(from = Post)]` generates the
conversion field by field — rename a field on `Post` and the projection stops
compiling. That is how the API surface stays deliberate instead of being
"whatever the struct happens to hold this month".

## 4. Handlers

Six of them, in `src/routes/posts.rs`. What is worth noticing is what is
**absent**: no OpenAPI annotations, no `.validate()?` call, no manual 404
mapping, no serialisation code. Every one of those comes from a type in the
signature.

```rust
/// List posts.
///
/// Published posts, newest first. Name yourself with `x-author` to see your own
/// drafts as well; an editor may ask for every draft with `?drafts=true`.
#[endpoint(errors = BlogError)]
async fn list(
    Inject(store): Inject<Store>,
    Inject(config): Inject<AppConfig>,
    Depends(actor): Depends<Actor>,
    Query(query): Query<ListPosts>,
) -> Result<Page<PostOut>> {
```

- the doc comment becomes the operation's `summary` and `description`;
- `Inject<Store>` is infallible at the use site, because boot proved it exists;
- `Depends<Actor>` resolves once per request and is memoised for the rest of it;
- `Query<ListPosts>` contributes five documented parameters with their
  constraints;
- `Page<PostOut>` contributes the 200 and registers `Page_PostOut` in
  `components/schemas`;
- `errors = BlogError` contributes the 404, the 409 and the 422 that the
  application's own error enum can produce.

### Pagination

Cursor-based, because offset pagination skips rows when the table is being
written to. The store returns `limit + 1` rows and the extra row is what says
there is a next page — no second query, no count that the client did not ask
for:

```rust
let rows = store.page(&filter, after, limit);
Ok(Page::from_items(rows, limit, |post| post.key().to_cursor())
    .map(PostOut::from)
    .with_total(total))
```

The cursor is the sort key — `created_at` plus the id, so it is total — encoded
base64url. A client passes back `next_cursor` verbatim; a cursor this API did
not issue is a 422 against `/cursor` rather than a 500.

## 5. Authorisation, in two shapes

**A guard** protects the four write routes. It runs *after routing and before
extraction*, so a request without the key is a 401 that never allocates a body,
and — unlike a bare Tower layer — it contributes its 401 and its security
requirement to every operation it covers:

```rust
moso::routes! {
    POST   "/posts"              => create,
    PATCH  "/posts/{id}"         => update,
    DELETE "/posts/{id}"         => destroy,
    POST   "/posts/{id}/publish" => publish,
}
.guard(ApiKeyGuard)
```

**A dependency** carries the caller's identity. `Actor` is a hand-written
`Dependency` that reads two headers; `Editor` is the derived "wrap and check"
shape, and asking for it in a signature *is* the authorisation rule:

```rust
#[derive(Dependency, Debug, Clone)]
#[depends(from = Actor, check = "editor", error = "only an editor may publish a post")]
pub struct Editor(pub Actor);

#[endpoint(errors = BlogError)]
async fn publish(
    Inject(store): Inject<Store>,
    Depends(_editor): Depends<Editor>,
    Path(id): Path<Id<Post>>,
) -> Result<Json<PostOut>> { … }
```

A caller without `x-role: editor` gets a 403 carrying exactly that message, and
the handler body never runs.

## 6. Middleware

```rust
#[moso::middleware]
pub async fn observe(
    Inject(metrics): Inject<Metrics>,
    req: Request,
    next: Next,
) -> Result<Response> { … }
```

One `async fn` becomes `ObserveLayer` and `ObserveService`. Parameters before
`req` are extracted first, so `Inject<Metrics>` works — and its requirement is
folded into `ObserveLayer::PROVIDER_REQ`, which means forgetting
`.provide(Metrics::default())` is a boot error naming `Metrics` rather than a
500 on the first request.

## 7. Errors

```rust
#[derive(Debug, moso::Error)]
#[error(type_base = "https://moso.example/errors/")]
pub enum BlogError {
    #[error(status = 404, detail = "No post with id {id}")]
    PostNotFound { id: String },

    #[error(status = 409, detail = "The slug `{slug}` is already taken")]
    SlugTaken { slug: String },
    …
}
```

The derive writes `Display`, `std::error::Error`, `From<BlogError> for
moso::Error` — which is what makes `?` work in every handler — and a `Describe`
impl, which is what puts these statuses in the document when a handler declares
`#[endpoint(errors = BlogError)]`. One source of truth for "what can go wrong
here", used by the runtime and by the contract.

## 8. The document

`openapi.json` in this directory was written by nobody. It is what
`App::build()` produced from the handlers, and it is committed on purpose: every
change to the API surface becomes a reviewable diff instead of a surprise in a
client's error log.

A test compares the committed file to what the application generates. After an
intentional change:

```text
UPDATE_OPENAPI=1 cargo test -p example-crud --test openapi
```

The same document is served at `/openapi.json` and rendered at `/docs`, and
`App::build()` mounts `/healthz` and `/readyz` on its own.

## 9. The tests

```text
cargo test -p example-crud
```

- **43 unit tests**, beside the code they test — slug uniqueness, draft
  visibility, cursor round-trips, the redaction of a secret.
- **29 integration tests** in `tests/posts.rs`, each booting the whole
  application and speaking HTTP to it: 201 with a `Location`, the 422 with the
  exact pointer and the exact code, the 404, two pages walked with a cursor with
  nothing repeated, the guard's 401, the derived dependency's 403, and the
  middleware's header and counter.
- **11 document tests** in `tests/openapi.rs`, including the drift check and a
  test that the contract validator below really does reject drift — a contract
  check that cannot fail is worse than none, because it reads like coverage.

`tests/support/mod.rs` is the whole harness, and its most interesting assertion
is `assert_matches_openapi()`: it finds the operation in the document *this*
application generated, finds the response declared for the status that came
back, resolves the `$ref`, and validates the body against it — with an
undocumented field counted as a failure. A handler that starts returning a field
the contract does not mention breaks a test, which is the one class of drift a
status-code assertion cannot see.

---

## What a real deployment changes

Replace `src/store.rs` with a database. `Post`, `PostOut`, the handlers, the
guard, the document and every test above stay exactly as they are — they are
written against `Store`'s methods, not against a `HashMap`.
