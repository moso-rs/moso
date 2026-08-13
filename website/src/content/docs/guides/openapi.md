---
title: OpenAPI
description: Moso assembles an OpenAPI 3.1.1 document at boot from the handlers you mounted, serves it and a network-free docs UI, and gives you the checks that fail CI when the committed copy drifts.
order: 11
status: shipped
---

Every Moso application assembles a complete OpenAPI 3.1.1 document while `App::build()` runs, out of the routes it actually mounted. You annotate nothing. The doc comment on a handler becomes the summary, the extractors in its signature become parameters and a request body, the return type becomes the responses, and each guard on the route contributes the security requirement it enforces. If the document disagrees with the code, that is a bug in Moso rather than a chore for you.

This page covers what each part of a handler contributes, how to say the things the types cannot say, how the document and the docs UI are served, how to export the document, and how to fail CI when it drifts.

> [!NOTE]
> `/openapi.yaml` is served alongside `/openapi.json`, `moso openapi check --breaking` classifies changes and exits nonzero only on a breaking one, and the `scalar`, `redoc` and `swagger-ui` cargo features each mount a docs route. All of them render Moso's own network-free UI rather than a vendored bundle, so the feature selects only the path, not the tool.

## The smallest thing that works

```rust title="src/routes/posts.rs"
use moso::prelude::*;

/// Create a post.
///
/// The slug is derived from the title and suffixed if it collides, so two posts
/// called "Hello" become `hello` and `hello-2`.
#[endpoint]
async fn create(
    Inject(store): Inject<Store>,
    Json(body): Json<CreatePost>,
) -> Result<Created<PostOut>> {
    let post = store.create(body)?;
    Ok(Created::at(format!("/posts/{}", post.id), post.into()))
}

pub fn router() -> Router {
    moso::routes! { POST "/posts" => create }.tag("posts")
}
```

With nothing else, that operation comes out with `summary: "Create a post."`, the rest of the doc comment as `description`, `operationId: "posts_create"`, `tags: ["posts"]`, a required `application/json` request body referencing `#/components/schemas/CreatePost`, a 201 whose body is `PostOut` and which documents its `location` header, a 400, 413 and 415 from `Json`, a 422 if `CreatePost` declares any constraint, and a 500 and 503 from the `Err` arm of `Result`.

Read the assembled document back in process with `app.openapi()`, which hands you `&moso::openapi::Document`.

## What each part of the signature contributes

`#[endpoint]` compiles the handler into an `Endpoint` impl whose `spec` function drives an `OperationBuilder` in a fixed order: summary, description, operation id, source location, tags, hidden, deprecated, the `response(...)` attributes, then each parameter's describer, then the return type's, then the `errors = Type` describers, then the examples.

| Part of the handler | What it writes |
| --- | --- |
| First line of the doc comment | `summary` |
| The rest of the doc comment | `description` |
| Module path and function name | `operationId`, as `{last module segment}_{fn name}`, so `blog::routes::posts::list` becomes `posts_list` |
| The handler's file and line | recorded on the spec, used by `moso routes` and by conflict diagnostics |
| Each parameter | whatever that extractor's `describe` writes, see below |
| The return type | the responses, see below |

A handler registered without `#[endpoint]` still routes. It contributes nothing, gets a derived `operationId` of the form `get_users_by_id_posts`, and is exempted from the path-parameter consistency check, because such a handler declares no parameters by construction.

### What the extractors write

| Extractor | Contribution |
| --- | --- |
| `Json<T>` | required `application/json` body; 400, 413, 415, plus a 422 when `T` declares a constraint |
| `Form<T>` | required `application/x-www-form-urlencoded` body; 400, 413, 415, plus a 422 when `T` declares a constraint |
| `Text` | required `text/plain` body; 400, 413 |
| `Bytes` | required `application/octet-stream` body with `format: binary`; 413 |
| `Multipart` | required `multipart/form-data` body, modelled as an open object; 400, 413 |
| `Query<T>` | one `in: query` parameter per property of `T`, with its type, constraints, description, default and deprecation |
| `Path<T>` | one `in: path` parameter per field, always required |
| `Headers<T>` | one `in: header` parameter per field, named in header case |
| `Inject<T>` | nothing: a provider is not part of the wire contract |
| `Depends<T>` | whatever `T::describe` writes, which is nothing unless you override it |
| `Cookies`, `HeaderMap`, `RequestId`, `ClientIp`, `Extension<T>` | nothing, deliberately |

Three details are easy to trip over. A `Query<T>` parameter is marked required only when the property is required *and* has no default; an array property is emitted with `style: form` and `explode: true`, an object property with `style: deepObject`. `Headers<T>` skips the redacted names (`authorization`, `proxy-authorization`, `cookie`, `set-cookie`, `x-api-key`, `x-auth-token`, `x-csrf-token`), because a credential is documented by whatever authenticates with it, not as a plain header parameter. `Cookies` documents nothing for the same reason: listing `Cookie` as a header parameter produces a document that generates a client sending its own session by hand.

A positional `Path<(u64, String)>` contributes nameless placeholders, and the router fills the names in from the path template at `App::build()`. If the tuple has more elements than the template has `{placeholders}`, the extra one is left nameless on purpose, so the arity mismatch is reported rather than papered over.

### What the return type writes

| Return type | Documented as |
| --- | --- |
| `Json<T>` | 200, `application/json`, `T`'s schema |
| `Created<T>` | `T`'s body restaged to 201, plus a `location` header |
| `Accepted<T>` | `T`'s body restaged to 202, plus a `location` header |
| `NoContent`, also spelled `Empty` | 204, no body |
| `()` | 200 with no body. Prefer `NoContent`, which is a 204 and says so |
| `Page<T>` | 200 with the paginated envelope's schema |
| `Sse<S>` | 200 `text/event-stream` with an `x-sse-events` extension, plus an optional `last-event-id` request header parameter |
| `File` | 200 (with `accept-ranges`, `etag`, `content-disposition`), 206, 304, 404 and 416 |
| `Cached<T>` | `T`'s responses plus a 200 and a 304, both carrying `etag` |
| `Redirect` | all four of 302, 303, 307 and 308, each with a required `location` header |
| `Text`, `Html` | 200 `text/plain` or `text/html` |
| `Raw<T>` | 200 with an unspecified `*/*` body and an `x-moso-raw-response` marker |
| `Either<A, B>` | both arms, folded into a `oneOf` where they disagree at the same status |
| `Result<T, E>` | both arms |
| `moso::Error`, the usual `E` | 500 and 503, and nothing else |
| `Option<T>` | the inner type plus a 404 |

`moso::Error` is deliberately conservative: only the two statuses every operation can genuinely return. An operation-specific 409 comes from the extractor that raises it or from `#[endpoint(errors = MyError)]`, so the document does not claim every endpoint can return every status in your taxonomy.

`Redirect` documents all four of its statuses because `describe` sees the type and not the value. There is no attribute that removes one again, so a handler that must document exactly one redirect needs a response type of its own.

The first time any error response is described, the RFC 9457 `Problem` schema is published into `components/schemas`, and `ValidationProblem` alongside it when a 422 is described. `ValidationProblem` repeats `Problem`'s members instead of composing with `allOf`, because an operation that documents a 422 without a 400 is perfectly normal and the reference would dangle. See [the error model](./errors.md) for the shape of both.

> [!IMPORTANT]
> A 422 is documented only when the request type actually has constraints. `Json<T>::describe` checks `T::HAS_CONSTRAINTS` first, so a constraint-free DTO does not tell clients to handle a response the server can never send. See [validation](./validation.md).

## Document metadata

The types cannot know your title, your version or where you deploy. Say those once, at the composition root:

```rust title="src/lib.rs"
App::new(config)
    .provide(Store::new())
    .mount(routes::router())
    .openapi(move |document| {
        document
            .title("Moso blog API")
            .version(env!("CARGO_PKG_VERSION"))
            .description(
                "The Moso tutorial application. Posts live in memory, so every restart \
                 starts from an empty store.",
            )
            .server(public_url, "this instance")
            .security_scheme(
                crate::auth::API_KEY_SCHEME,
                SecurityScheme::api_key_header(crate::auth::API_KEY_HEADER),
            )
            .tag_description("posts", "Everything you can do with a post.")
            .tag_description("status", "What this instance is doing.");
    })
    .build()
```

The closure receives `&mut DocumentBuilder`, and every method on it returns `&mut Self`, so they chain.

| Method | Sets |
| --- | --- |
| `title`, `version`, `summary`, `description` | `info` |
| `terms_of_service(url)` | `info.termsOfService` |
| `contact(name, email)`, `contact_url(url)` | `info.contact` |
| `license_spdx(id)`, `license(name, url)` | `info.license` |
| `server(url, description)`, `server_spec(Server)` | `servers`, the second form with variables |
| `tag_description(tag, text)`, `tag(Tag)` | `tags`, the second form also carrying external docs |
| `security_scheme(name, scheme)` | `components.securitySchemes` |
| `security(requirement)` | document-level security |
| `shared_response(name, response)` | `components.responses`, referenced with `ResponseSpec::shared(name)` |
| `external_docs(url, description)` | `externalDocs` |
| `json_schema_dialect(url)` | `jsonSchemaDialect`, already defaulted to JSON Schema 2020-12 |
| `extension(key, value)` | any `x-*` member |
| `webhook(name, method, spec)` | `webhooks`, which nothing else in Moso populates |

A missing title defaults to `"API"` and a missing version to `"0.0.0"`. The readiness probe compares against that placeholder to tell a real version from an unset one, so `version` is worth the one line.

`SecurityScheme`, `ResponseSpec` and `OperationBuilder` are in the prelude. `SecurityRequirement`, `Document`, `Param` and `ContentType` are one path away, under `moso::openapi`.

## Security

Scheme *names* are declared once on the document, because a name is an application-level naming decision. Security *requirements* are contributed by whatever actually enforces them. That is what stops the document drifting away from the code: there is no second place to forget.

A `Guard` writes both halves in its `describe`:

```rust title="src/auth.rs"
use moso::openapi::SecurityRequirement;
use moso::prelude::*;

impl Guard for ApiKeyGuard {
    fn describe(&self, op: &mut OperationBuilder) {
        op.security(SecurityRequirement::scheme(API_KEY_SCHEME));
        op.response(
            401,
            ResponseSpec::problem("The `x-api-key` header is absent or wrong."),
        );
    }

    fn check<'a>(&'a self, parts: &'a Parts, ctx: &'a RequestCtx) -> BoxFuture<'a, Result<()>> {
        // the enforcement
    }
}
```

Apply it to half a router and every operation underneath carries the requirement and the 401:

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

The four guarded operations get `"security": [{ "api_key": [] }]` and a documented 401. The two public ones carry no `security` member at all, so they inherit whatever the document declares.

A `Dependency` does the same by overriding `Dependency::describe`, which contributes nothing by default:

```rust title="src/auth.rs"
impl Dependency for Session {
    fn describe(op: &mut OperationBuilder) {
        op.security(SecurityRequirement::scheme("session"));
        op.response(401, ResponseSpec::problem("No session cookie, or it expired."));
    }

    async fn resolve(ctx: &RequestCtx) -> Result<Self> {
        // the lookup
    }
}
```

An ordinary `tower::Layer` deliberately cannot describe anything. That is the reason to reach for a guard when the thing you are adding changes the API contract, and the reason a layer is fine when it does not. See [middleware](./middleware.md).

### Declaring schemes

| Constructor | Emits |
| --- | --- |
| `SecurityScheme::api_key_header(name)` | `apiKey` in `header` |
| `SecurityScheme::api_key_query(name)` | `apiKey` in `query`, discouraged: it lands in access logs and `Referer` |
| `SecurityScheme::cookie(name)` | `apiKey` in `cookie` |
| `SecurityScheme::http_basic()` | `http`, scheme `basic` |
| `SecurityScheme::http_bearer(format)` | `http`, scheme `bearer`, plus `bearerFormat` |
| `SecurityScheme::http(scheme)` | any registered RFC 7235 scheme name |
| `SecurityScheme::mutual_tls()` | `mutualTLS` |
| `SecurityScheme::oauth2(flows)` | `oauth2`, with `OAuthFlows` carrying up to four flows and their scope maps |
| `SecurityScheme::open_id_connect(url)` | `openIdConnect` |

`.with_description(text)` on any of them adds prose.

One `SecurityRequirement` is a conjunction: `SecurityRequirement::scopes("oauth", ["read"]).and("session", [])` means both must be satisfied. A `Vec<SecurityRequirement>` on an operation is a disjunction, so any one of them suffices. `SecurityRequirement::none()` permits unauthenticated access, and `op.public()` is the shorthand that writes `security: []` explicitly rather than leaving the member absent.

Naming a scheme that was never declared is a boot error, as is giving scopes to a scheme that takes none, naming a scope no OAuth flow declares, or writing an OAuth flow without the URL its flow type requires.

## Subtree metadata on the router

Five router methods state a fact that a whole group of routes shares. They apply to the routes registered so far, so their position in the chain is what scopes them, and they run *after* the handler has described itself, so they can only fill gaps and never overwrite.

| Method | Effect |
| --- | --- |
| `.tag("posts")` | appends a tag to every operation beneath, deduplicated |
| `.security(requirement)` | adds a security requirement |
| `.responds(429, spec)` | documents a response on every operation beneath |
| `.deprecated()` | marks the subtree deprecated |
| `.hidden()` | removes the subtree from the document, while still serving it |

```rust title="src/routes/mod.rs"
pub fn router() -> Router {
    Router::new()
        .merge(health::router())
        .nest("/api/v1", api_v1())
}

fn api_v1() -> Router {
    posts::router().responds(429, ResponseSpec::problem("Too many requests."))
}
```

`nest` rewrites the documented paths as well as the routed ones, so a route registered at `/posts` appears as `/api/v1/posts`, and the 429 lands on every operation underneath without any handler mentioning it. When routers nest, the outer router's tags and responses are appended *after* the inner router's, so the more specific declaration is listed first.

> [!WARNING]
> A hidden operation is not a secured one. `.hidden()` and `#[endpoint(hidden)]` remove the operation from the document. The route is still mounted and still serves traffic to anyone who guesses the path.

## Per-endpoint overrides

| Argument | Form | Effect |
| --- | --- | --- |
| `operation_id` | `operation_id = "list_users"` | replaces the derived id |
| `tag` | `tag = "users"`, repeatable | one tag per occurrence |
| `hidden` | bare word | drops the operation from the document |
| `deprecated` | bare word | sets `deprecated`. A plain Rust `#[deprecated]` on the handler does the same |
| `response` | `response(409, "Already exists")` | a status of 400 or above becomes a `Problem` response, below that an empty one. Emitted before the describers, so it wins the first-writer race |
| `example` | `example(request = "...", response = "...")` | fills `example` on the request body and on the first `2xx` response, only where none is set. A string that parses as JSON becomes that JSON, anything else becomes a JSON string |
| `errors` | `errors = BlogError`, repeatable | runs that type's `Describe`, so its whole error taxonomy lands on this operation |

Anything else is a compile error that prints the list of valid arguments.

## The merge rules

Several describers write into one operation: the handler, each extractor, the return type, each error type, each guard, each dependency, and the router. The rules that keep them from fighting are short enough to recite.

| Member | Rule |
| --- | --- |
| `summary`, `description`, `operationId`, `externalDocs` | first writer wins |
| `tags` | appended, deduplicated, insertion order kept |
| `parameters` | keyed by `(in, name)`; first wins, later calls fill only absent members, booleans union |
| `requestBody` | first wins including its `required`; later calls add only content types it did not describe |
| `responses` | keyed by status; first wins, later calls fill only absent members |
| `security` | appended unless an identical requirement is already present |
| `deprecated`, `hidden` | sticky: once true, always true |
| `x-*` extensions | first writer wins per key |

`#[endpoint]` emits the summary first, so no extractor can overwrite the words you wrote in the doc comment. Router metadata is applied last and can only add, so `Router::tag("users")` cannot clobber an endpoint that named its own tag. `requestBody.required` is deliberately left alone by later writers, because the first writer installed the whole object and its `false` means "this body is optional" rather than "unset".

Response keys are ordered numerically ascending, then `NXX` ranges, then anything unrecognised, then `default`. A response with no description of its own is given the status code's IANA reason phrase.

## Describing something the framework does not know

Implement `Describe` for a response type of your own:

```rust title="src/response/csv.rs"
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

`ResponseSpec` has constructors for the common shapes (`json_of::<T>()`, `problem(desc)`, `empty(desc)`, `binary`, `sse`, `text`, `redirect`, `shared(name)`) and builder methods for headers, links, extra content types, examples and `x-*` extensions. `Param` is the equivalent for parameters, with `Param::query`, `Param::path`, `Param::header` and `Param::cookie`.

Inside `op.parameter(...)`, use `schema_of::<T>()` rather than `schema::<T>(op.generator())`: the call already holds `&mut op`, so the argument expression cannot borrow the generator as well. A path parameter is required by definition, and `.required(false)` on one is a no-op.

When none of that fits, `op.spec_mut()` hands you the raw `OperationSpec` and you can write any member of the wire operation directly.

## Serving the document

The document and UI routes are mounted on an **outer** router whose fallback is your application. That is why they never appear in the document itself, never appear in `moso routes`, and are not logged, compressed, traced or subject to your request timeout.

`GET /openapi.json` serves bytes that were serialised exactly once at boot, with a weak ETag and `Cache-Control: no-cache`. A matching `If-None-Match` gets a 304. No other request touches the document; it is a boot artefact. `GET /openapi.yaml` serves the same document as YAML, from `Document::to_yaml`, gated exactly like the JSON route.

`GET /docs` serves the pre-rendered documentation UI as `text/html; charset=utf-8`.

| Field on `HttpConfig` | Default | Effect |
| --- | --- | --- |
| `expose_docs` | derived from the profile: on everywhere except production | whether the routes are mounted at all |
| `docs_path` | `/docs` | where the UI is mounted |
| `openapi_path` | `/openapi.json` | where the document is served, and the URL the UI fetches |

```rust title="src/lib.rs"
App::new(config)
    .http_config(HttpConfig {
        expose_docs: true,
        openapi_path: "/internal/openapi.json".to_owned(),
        ..HttpConfig::default()
    })
```

Registering an application route on `docs_path` or `openapi_path` is a boot error, because your route would sit behind the framework's and never be reached. The error names the route, says where it was registered, and tells you to move the framework's one in configuration.

> [!CAUTION]
> `HttpConfig::default()` has `expose_docs: true` in **every** profile, and the profile-derived default applies only when you supply no `HttpConfig` at all, so if you construct one set `expose_docs` yourself. The production profile is the exception that overrides both: it forces every doc route (`/openapi.json`, `/openapi.yaml`, `/docs`, and `/redoc` and `/swagger` when built) off regardless of `expose_docs`, so even a hand-built config cannot expose the document in production.

The `openapi` cargo feature (on by default) decides whether the document and UI routes are compiled in. It changes nothing else: the document is still assembled in every build and `app.openapi()` still works with the feature off, which is what lets `moso openapi export` work against a binary that serves neither route.

## The documentation UI

The page at `/docs` is Moso's own renderer, not a CDN loader. It makes **no network request other than a same-origin fetch of the spec URL**, which is the only version of "works air-gapped" that survives a TLS-intercepting corporate proxy or a CI runner with no egress. Two unit tests hold that line: one asserts the template contains no absolute URL, no `<script src>`, no external stylesheet and no `@import`; the other parses the rendered output and checks every element is closed.

It renders a sidebar grouped by tag with a filter box, per-operation method, path, summary and description, a parameters table, the request body and every response with an expandable schema tree that resolves `$ref` and stops at cycles, a synthesised example, and a "Try it" panel that issues a request and reports the status, elapsed time, response headers and a pretty-printed body. It follows `prefers-color-scheme` with a manual three-state override, deep links to `#operationId`, is operable from the keyboard alone, and lays out down to a 375 px viewport.

> [!NOTE]
> The served `/docs` page carries a **per-response CSP nonce**, drawn from the OS CSPRNG, on every inline script and style, and sets no `unsafe-inline`. A `script-src 'nonce-...'` policy works against it as shipped. Render the UI yourself with `DocsUi` only when you want to supply your own nonce or pin a theme.

```rust
use moso::openapi::Theme;
use moso::openapi::ui::DocsUi;

let html = DocsUi::new()
    .title("Shop API")
    .spec_url("/openapi.json")
    .theme(Theme::Dark)
    .nonce(request_nonce)
    .render();
```

Every value you pass is escaped for the position it lands in. HTML positions are entity-escaped; script positions are JSON-encoded and every literal `<` is additionally rewritten to its JSON unicode escape. Escaping only `</` would stop a value closing a script element but not opening one, and a title containing `<!--<script>` drives the HTML tokenizer into a state where the real `</script>` no longer ends the element. Three injection tests cover it.

The `scalar`, `redoc` and `swagger-ui` cargo features on `moso-openapi` each mount an additional docs route: `scalar` mounts `/docs`, `redoc` mounts `/redoc`, and `swagger-ui` mounts `/swagger`. The nuance worth knowing is that all three render **Moso's own** self-contained, network-free renderer, not a vendored ReDoc, Swagger UI or Scalar bundle. That is a deliberate no-CDN, no-bundle choice: the feature you enable selects only the route path, and the page it serves is the same air-gapped UI described above, with the same per-response CSP nonce.

## Exporting the document

```bash
moso openapi export --out openapi.json
moso openapi export --compact          # one line instead of indented
moso openapi export                    # to standard output
```

`export` runs your application binary with `--dump-openapi`, reads one JSON document off standard output, creates any missing parent directories, writes the file with a trailing newline, and reports the operation count. Indented is the default, because the usual destination is a committed file and byte stability only pays off if the diff is readable. `--pretty` exists so a script can say so explicitly. `--manifest-path`, `--bin`, `--release` and `--features` control how the application is reached and built, and the global `--json` turns the result into structured output.

If your application writes anything else to standard output while booting, the export fails and tells you to look for a `println!` in a startup hook: everything except the document has to go to standard error.

From Rust, `Document` serialises itself with `to_json`, `to_json_pretty` (exactly one trailing newline), `to_json_bytes` and `to_yaml`, and parses a committed file back with `Document::from_json`. `Document::filter_prefix("/api/v1")` keeps only the paths under a prefix and strips it, matching on segment boundaries, which is how you split a multi-version API into one document per version. `to_yaml` is served live at `/openapi.yaml`, and `filter_prefix` also backs `moso openapi export --prefix`, so that same split is one flag away from the CLI. The document is OpenAPI 3.1 throughout.

## Generating a client

```bash
moso client --out ../web/src/api                 # TypeScript, from your application
moso client --lang rust --out ../sdk/src/api     # Rust
moso client --input openapi.json --out src/api   # from a committed document
moso client --out ../web/src/api --check         # in CI
```

Without `--input` it runs your binary with `--dump-openapi`, exactly as `export` does. With `--input` it reads the file and never touches cargo, so it also works in a front-end repository that has the committed document and no `Cargo.toml` anywhere above it. `--lang` takes `ts` (the default) or `rust`.

Output is **deterministic**: the same document always produces the same bytes, because every map is sorted before it is read and nothing writes a clock, a path or a version number into a file. That is what makes the output worth committing, and it is what `--check` rests on: it regenerates into memory, compares byte for byte, prints which files differ, and exits 1. Files in `--out` that the generator does not produce are left alone, so hand-written code can live beside it.

The **TypeScript** target writes `types.ts`, `client.ts` and `index.ts`, and depends on nothing: it is `fetch`, `Headers` and `URLSearchParams`. Every operation becomes one method on the object `createClient(options)` returns, and every method *resolves*; none of them reject:

```ts
import { createClient, fieldErrorAt, hasFieldCode } from "./api";

const api = createClient({ baseUrl: "https://api.example.com" });
const result = await api.postsCreate({ body: { title: "Hello", body: "…" } });

if (result.ok) {
  console.log(result.data.slug);            // PostOut
} else if (result.failure.kind === "problem") {
  if (result.failure.problem.type.endsWith("/validation")) {
    fieldErrorAt(result.failure, "/title")?.message;
    hasFieldCode(result.failure, "len");
  }
}
```

`result.failure.problem` is typed as the union of the schemas *that operation* documents for its error statuses (`Problem | ValidationProblem` where both are declared), so branching on `type` and on `FieldError::code` needs no cast. The other two failure arms are `"network"` (the request never got an answer) and `"malformed"` (the answer was not the documented JSON, with the raw text kept). The output uses only erasable syntax (no `enum`, no `namespace`), so it passes through esbuild, swc and `node --experimental-strip-types` untouched.

The **Rust** target writes `mod.rs`, `models.rs` and `client.rs`, and is transport-agnostic: it needs `serde` and `serde_json` and nothing else. It describes requests rather than performing them, and you implement `Transport` once over the HTTP client your program already has. Naming `reqwest` in generated code would also name a TLS stack, and that is not a code generator's decision to make; the manifest snippet and a fifteen-line `reqwest` implementation are in the generated `mod.rs`. Every method returns `Result<T, ApiError<E>>`, where `ApiError::Problem` carries the parsed problem document with `has_code` and `field_error` on it.

Both targets handle `$ref`, `allOf`, `oneOf`, `anyOf`, `type: [T, "null"]`, enums, arrays, `additionalProperties`, path/query/header parameters, request bodies and multiple response codes; anonymous objects and unions are hoisted into named types so the two targets agree on names. What cannot be expressed is named rather than dropped: a tuple schema, an external `$ref` and a mixed-type enum become a clearly-marked opaque type carrying the sentence explaining why, and anything only partly carried across is listed in the generated file's header and printed when the command runs.

Four things are worth knowing before you point it at a document. `text/event-stream` hands you the raw `Response` rather than pretending a stream is a value. A cookie parameter is not an argument, because `fetch` refuses to set the header. A `multipart/form-data` body is passed through as you build it rather than typed field by field. And in the Rust target an integer is `i64`, a number is `f64`, and an absent member and a `null` one both decode to `None`.

## Failing CI on drift

Two approaches, and they are not equivalent.

### The CLI check

```bash
moso openapi check              # compares against ./openapi.json
moso openapi check spec/api.json
```

It runs the application, parses both documents, and compares the parsed JSON, so reformatting the committed file is not a failure. On a difference it prints up to 20 RFC 6901 JSON pointers and exits with a user fault:

```text
  ✗ openapi.json is out of date       (3 differences)
      changed   /paths/~1users/get/parameters/0/schema/maximum
      added     /paths/~1users~1{id}~1deactivate
      removed   /paths/~1legacy~1users
error: `openapi.json` does not match the code
help: moso openapi export --out openapi.json
```

Object key order is not a difference, because parsed maps compare order-insensitively. Array order **is** a difference, because a `parameters` list in a different order is a different document to a code generator.

By default that is a list of pointers, not reviewer-friendly prose, and it treats every difference alike. `moso openapi check --breaking` adds the missing notion of breaking: it classifies each change as breaking or compatible and exits nonzero **only** on a breaking one, so a purely additive change passes. See [breaking-change classification](#breaking-change-classification).

### The in-repo test

A plain `#[test]` gives the same guarantee with a regeneration escape hatch and no subprocess:

```rust title="tests/openapi.rs"
use std::path::PathBuf;

/// The committed document.
fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("openapi.json")
}

/// What this application generates, exactly as it would be committed.
fn generated() -> String {
    let app = myapp::build(myapp::AppConfig::defaults()).expect("the application builds");
    let mut rendered =
        serde_json::to_string_pretty(app.openapi()).expect("the document renders");
    rendered.push('\n');
    rendered
}

#[test]
fn the_committed_document_matches_the_application() {
    let generated = generated();

    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(committed_path(), &generated).expect("openapi.json is writable");
        return;
    }

    let committed = std::fs::read_to_string(committed_path()).expect("openapi.json is missing");
    assert_eq!(committed, generated);
}
```

Regenerate after an intentional change with `UPDATE_OPENAPI=1 cargo test --test openapi` and commit the diff. This works because the document is byte-stable: every map is ordered, components are sorted by name, paths lexicographically, and responses by status with `default` last. Inside a `moso_test::TestApp`, the same document is `app.openapi()`.

### Breaking-change classification

`moso::openapi::diff` is a complete semantic differ that classifies every change:

| Breaking | Not breaking |
| --- | --- |
| an operation is removed | an operation is added |
| a required request field or parameter is added | an optional one is added |
| a request field becomes required | a required field becomes optional |
| a response field is removed | a response field is added |
| a type is narrowed (`number` to `integer`, a variant leaves a `oneOf`) | a type is widened |
| a constraint is tightened (`maxLength` down, `minimum` up, `enum` shrinks) | a constraint is relaxed |
| a security requirement is added or gains scopes | a requirement is removed |
| a success status is removed | an error status is added |

Requests are contravariant and responses covariant with respect to field presence, so "a field was added" reads in opposite directions depending on where the schema sits. Also breaking: closing `additionalProperties` on a request object, withdrawing a content type, removing a response header or making it optional, gaining an `allOf` part, changing a `discriminator`, turning on `readOnly` in a request position or `writeOnly` in a response position, and changing a security scheme's `type`.

`moso openapi check --breaking` runs this classifier for you against the committed document. To compare against a specific released version instead, or to keep the check in-process with no subprocess, call the differ directly from a test that parses the last released document and diffs it against the live one:

```rust title="tests/breaking.rs"
use moso::openapi::Document;
use moso::openapi::diff::{ChangeReport, diff, has_breaking};

/// The document as of the last release, committed next to this test.
const RELEASED: &str = include_str!("fixtures/openapi-1.0.json");

#[test]
fn nothing_breaking_since_the_last_release() {
    let released = Document::from_json(RELEASED).expect("a valid document");
    let app = myapp::build(myapp::AppConfig::defaults()).expect("the application builds");

    let changes = diff(&released, app.openapi());
    let report = ChangeReport::new(&changes).file("openapi.json");
    assert!(!has_breaking(&changes), "{report}");
}
```

`diff_with` and `DiffOptions` drop descriptions, examples or extensions from the comparison, and `DiffOptions::structural()` drops all three. The `x-source` extension is read but never diffed, because it moves whenever line numbers shift. Moso never writes it either, so the report's `(added in src/routes/users.rs:102)` annotation always degrades to `(added)`.

## Failure modes

`App::build()` refuses to start when the description is wrong, and it reports every problem at once rather than one per run.

| Boot error | Cause | Fix |
| --- | --- | --- |
| Duplicate `operationId` | two handlers derived the same id | `#[endpoint(operation_id = "...")]` on one |
| Schema collision | two Rust types produce the same `schema_name()` | `#[schema(rename = "...")]` on one |
| Route conflict | the same method and path registered twice | remove one registration |
| Path parameter mismatch | the `{placeholders}` in the template disagree with the `in: path` parameters, in either direction | fix the template or the extractor |
| Invalid status key | a response registered under something that is not a status code, an `NXX` range or `default` | use a valid key |
| Unknown security scheme | a requirement names a scheme that was never declared | add `.security_scheme("name", ...)` |
| Dangling `$ref` | a reference that resolves to nothing | usually a schema that was never registered |

Two limits are worth knowing. External `$ref`s are never refuted, because the crate performs no I/O, so a typo in an external reference is not caught at boot. And the `Problem` and `ValidationProblem` component schemas mirror `moso_core::error::Problem` member by member by hand, with no compile-time link between them, so they can drift.

Once the application is running, `TestResponse::assert_matches_openapi()` checks a live response against the schema its own document publishes, and `TestApp::builder().assert_openapi(options)` does it for every response in a suite. The validator treats an undocumented property as a violation by default, which is stricter than JSON Schema and deliberately so. It ignores `format`, because JSON Schema calls `format` an annotation; formats are enforced on the way in, by validation. See [testing](./testing.md).

## See also

- [Schemas](./schemas.md) for what fills `components/schemas`.
- [Errors](./errors.md) for the `Problem` shape every error response references.
- [Extractors](./extractors.md) and [responses](./responses.md) for the full list of describers.
- [Routing](./routing.md) for `merge`, `nest` and where subtree metadata applies.
- [Testing](./testing.md) for contract assertions against the document.
