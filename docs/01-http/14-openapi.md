# 14 — OpenAPI: Zero-Annotation Documentation

> **Status: mostly implemented.** Generation, assembly, determinism, drift detection, the
> embedded UI, `/openapi.yaml` and the `/redoc` / `/swagger` UI routes are built. Client
> generation, the 3.0 downgrade and `moso asyncapi` are not — each is flagged ⛔ in place below.

## Goal

The OpenAPI document is a **derived artefact**, never a hand-maintained one. Nothing in it is
written by the user except doc comments and type definitions. If the document disagrees with the
code, that is a bug in Moso, not a chore for the user.

## How it is assembled

```
#[endpoint]  →  Endpoint::spec(&mut OperationBuilder)
                     │  extractors call op.parameter(..) / op.request_body(..)
                     │  response types call op.response(..)
                     │  auth/authz extractors call op.security(..) / op.response(401,..)
                     ▼
Router::get(path, handler)  →  merges OperationSpec at (method, path)
                     │  router-level .tag() / .security() / .responds() applied
                     ▼
App::build()  →  Document assembly
                     │  SchemaGenerator dedups every Schema into #/components/schemas
                     │  duplicate operationId / schema-name collision ⇒ boot error
                     ▼
GET /openapi.json          (pre-serialised bytes + ETag)
GET /openapi.yaml          (the same document, YAML-encoded, + ETag — one suffix off openapi_path)
GET /docs                  (the embedded UI — see below)
GET /redoc                 (the embedded UI, when the `redoc` feature is on)
GET /swagger               (the embedded UI, when the `swagger-ui` feature is on)
```

Both configurable paths (`http.openapi_path`, `http.docs_path`) and every route above are gated
twice: on the `openapi` cargo feature (compile time) and on `http.expose_docs` (runtime — **on in
`dev` and `test`, off in `production`**). `expose_docs` is forced off in the production profile at
boot even when an application constructs `HttpConfig` by hand and leaves the field at its `true`
struct-literal default; forcing it off is announced in the boot log. The document itself is
assembled in every build regardless, so `moso openapi export` works even where the routes are not
mounted. `/openapi.yaml` has no config key of its own: it is derived from `http.openapi_path` by
swapping the `.json` suffix for `.yaml`, so moving one moves both.

`SchemaGenerator` lives in **`moso-schema`**, not `moso-openapi` (decision D2); `moso-openapi`
depends on it and embeds what it produces into `components/schemas`.

## The document model

```rust
// spec — moso-openapi
pub struct Document {
    pub openapi: &'static str,          // "3.1.1"
    pub info: Info,
    pub servers: Vec<Server>,
    pub paths: IndexMap<String, PathItem>,
    pub components: Components,
    pub security: Vec<SecurityRequirement>,
    pub tags: Vec<Tag>,
    pub webhooks: IndexMap<String, PathItem>,
    pub extensions: IndexMap<String, Value>,
}
```

`IndexMap` throughout: **output must be deterministic** so the committed `openapi.json` diffs
cleanly and drift tests are meaningful. Schemas are emitted in insertion order after a stable sort
by name.

The shipped struct also carries `json_schema_dialect` and `external_docs`, and `openapi` is a
`String` rather than a `&'static str` so a document can round-trip through `Deserialize`
(`OPENAPI_VERSION` is `"3.1.1"`).

Why 3.1 and not 3.0: 3.1 aligns with JSON Schema 2020-12, so `#[derive(Schema)]` emits one schema
dialect instead of a lossy 3.0 translation (`nullable`, `exclusiveMinimum`-as-bool, no `$defs`,
no `const`). Tooling support is now good enough.
⛔ `moso openapi export --version 3.0` (the documented lossy downgrade) is **not implemented**.

## Configuration

```rust
// example — src/lib.rs
App::new(cfg)
    .openapi(|d| {
        d.title("Shop API")
         .version(env!("CARGO_PKG_VERSION"))
         .description(include_str!("../docs/api-intro.md"))
         .contact("API team", "api@shop.example")
         .license_spdx("Apache-2.0")
         .server("https://api.shop.example", "production")
         .server("http://localhost:3000", "local")
         .tag_description("users", "Account management")
         .security_scheme("session", SecurityScheme::cookie("sid"))
         .security_scheme("bearer", SecurityScheme::http_bearer("JWT"))
    })
```

Everything here is optional; sensible values come from `Cargo.toml` metadata.

## Doc UI

**The UI is Moso's own renderer, not a vendored Scalar/ReDoc/Swagger bundle.** This is a deliberate
change from the original design. `moso_openapi::ui::TEMPLATE` is a single self-contained HTML
document with inlined CSS and vanilla JavaScript whose only network request is a same-origin `fetch`
of the spec URL. Vendoring three real bundles would add megabytes of third-party JavaScript to every
Moso binary, and shipping one renderer we control is the only version of the promise "works
air-gapped" that we can actually keep. The `scalar` (default), `redoc` and `swagger-ui` cargo
features select which *route* is mounted — `scalar` is the `/docs` route, `redoc` additionally
mounts `/redoc`, `swagger-ui` additionally mounts `/swagger` — and **all three render this UI.**

Each HTML page carries a per-response `Content-Security-Policy` with a 128-bit nonce from the OS
CSPRNG, and the inline `<style>`/`<script>` carry that nonce, so the page runs with no
`unsafe-inline`. The policy is set on the response itself because the doc routes live on the outer
router, outside the security-headers middleware; the page is off in production, so the policy is only
ever served in `dev` and `test`.

What it renders: a sidebar grouped by tag with a filter box; per-operation method, path, summary and
a small CommonMark-ish description renderer; a parameters table; the request body and every
response, each with an expandable schema tree that resolves `$ref` (stopping at cycles) and a
synthesised example; and a "Try it" panel that issues a `fetch` and reports status, elapsed time,
response headers and a pretty-printed body. It follows `prefers-color-scheme` with a manual
three-state override, deep-links to `#operationId`, is keyboard-operable, and lays out to 375 px.

Two unit tests hold the no-CDN line: one asserts the template contains no absolute URL, no
`<script src>`, no external stylesheet and no `@import`; the other parses the rendered output and
checks every element is closed.

- `/openapi.json` is served as **pre-serialised bytes with a strong ETag**, answering 304 to a
  matching `If-None-Match`. `/openapi.yaml` is the same document YAML-encoded (`Document::to_yaml`),
  served the same way with its own ETag and a `application/yaml` content type.
- Doc routes are mounted on an **outer** router, so they are excluded from the document itself and
  from the application's route table, and they are **off in `production`** (`http.expose_docs`),
  because publishing your full API surface is a decision, not a default.

## Security schemes are contributed, not declared

The mechanism is built; the contributor in the original example is not. `Dependency::describe` and
`Guard::describe` both take an `&mut OperationBuilder` and can call `op.security(..)` and
`op.response(401, ..)`, and `Router::security(..)` overlays a requirement onto everything registered
so far. What does not exist is `moso-authz`, so there is no `Authorized<A, R>` to contribute one.

```rust
// as built — examples/crud does exactly this
#[endpoint]
async fn destroy(Path(key): Path<PostKey>) -> Result<NoContent> { /* … */ }

pub fn router() -> Router {
    moso::routes! { DELETE "/posts/{id}" => destroy }
        .guard(ApiKeyGuard)          // ← contributes the security scheme and the 401
}
```

⛔ The designed form, for when `moso-authz` lands:

```rust
#[endpoint]
async fn destroy(
    Authorized(_): Authorized<Delete, User>,   // ← contributes security + 401 + 403
    Path(id): Path<Id<User>>,
) -> Result<Empty> { /* … */ }
```

with `Authorized<A, R>::describe` emitting a 403 whose description names the permission, taken from
the typed permission registry so it cannot go stale.

## Drift detection (the feature that keeps it honest)

```
$ moso openapi check
✗ openapi.json is out of date

  + POST /users/{id}/deactivate      (added in src/routes/users.rs:102)
  ~ GET /users                        parameter `limit` maximum 100 → 200
  - GET /legacy/users                 (removed)

  run `moso openapi export` to update, and review the diff before committing
```

- The generated document is committed to the repo. CI runs `moso openapi check` and fails on drift.
- This gives code review a readable view of API changes, which is worth more than the file itself.
- `moso openapi check --breaking` additionally classifies changes (backwards-compatible vs
  breaking) using an in-repo rule set: removing an endpoint/field, narrowing a type, adding a
  required field, tightening a constraint are breaking; the inverse are not. CI can gate on it.

**As built.** `moso openapi export` and `moso openapi check` both work by running the application
binary with `--dump-openapi` and reading what it answers (ADR-0004: the route table is ordinary
Rust). `check` compares **parsed JSON, not bytes**, so reformatting the committed file or changing
its indent is not a failure — only a change in meaning is, and it prints at most 20 differences.
The classifier lives in `moso_openapi::diff`, which yields `Change { kind, path, detail, breaking }`
with a location language aimed at a reviewer (`GET /users`, `components.schemas.UserOut`) rather
than JSON pointers.

## Generated clients

⛔ **Not implemented.** There is no `moso client` subcommand. The strategy below stands as intent;
nothing about the shipped document forecloses it, and third-party generators work on
`/openapi.json` today.

```
$ moso client typescript --out ../web/src/api      # ⛔
$ moso client python     --out ../sdk/shop         # ⛔
$ moso client rust       --out ../crates/shop-client   # ⛔
```

Intended strategy:
- **TypeScript** and **Python** generators are Moso-owned and narrow: they target *our* document
  shape (which is regular by construction), not arbitrary OpenAPI. This is why they can produce
  clean output where general-purpose generators produce sludge. TS output uses discriminated unions
  from our `discriminator` usage, and a `Result`-style error type from our problem+json shape.
- **Rust** client generation reuses the very same `Schema` structs when the server crate is
  available (a `--from-crate` mode), avoiding a round-trip through JSON Schema entirely.
- Any third-party generator still works on `/openapi.json`; testing against `openapi-generator` and
  `orval` in CI is intended, and there is no CI yet.

## Handling the hard cases

| Case | Approach |
| --- | --- |
| Same struct used as input and output | One `$def`; `read_only`/`write_only` fields marked. Generators handle it. |
| Endpoint returning one of two shapes | `Either<A, B>` → `oneOf`. Discourage; document. |
| File download | `File` response → `content: application/octet-stream, schema: {type: string, format: binary}` |
| Streaming/SSE | `Sse<S>` → `text/event-stream` with an `x-sse-events` extension listing event schemas |
| WebSocket | Not expressible in OpenAPI. ⛔ Neither the `x-websocket` extension nor `moso asyncapi export` is implemented; the `ws` cargo feature only exposes Axum's upgrade surface. |
| Untyped passthrough | `Raw` → `{}` with a description saying it is intentionally unmodelled |
| Deprecation | `#[deprecated(note = "...")]` → `deprecated: true` + `x-sunset` if a date is given |
| Multiple API versions | Separate routers → `moso openapi export --prefix /api/v1 --out openapi.v1.json` |

## Performance

The document is built **once, at boot**, and cached as pre-serialised bytes. `/openapi.json` is a
static byte slice with an ETag. Cost at boot for 200 endpoints: target < 15 ms. This matters
because it is a per-process cost people will benchmark.

## Why not a build-script or a separate binary?

Considered and rejected (ADR-0006). A build-script approach could avoid the runtime assembly cost,
but it cannot see the router composition (which is ordinary Rust code), and it would make the
document a build artefact users cannot inspect with `cargo expand`. Boot-time assembly from
compile-time-constructed specs is the right trade: the *content* is compile-time, only the
*assembly* is runtime, and it is 15 ms once.

## Acceptance criteria (WP-07)

1. 🟡 The tutorial app (`examples/crud`) produces a document asserted by
   `examples/crud/tests/openapi.rs`. It uses `#[endpoint(errors = BlogError)]` on the handlers that
   have a custom taxonomy — everything else is bare. **It is not validated against the official 3.1
   meta-schema**; that needs a JSON Schema validator in CI, and there is no CI.
2. ✅ Every extractor and response type has a test asserting its exact contribution.
3. ✅ `moso openapi check` detects added/removed operations, changed parameter constraints, changed
   response schemas and changed security requirements, and classifies each as breaking or not.
4. ⛔ Generated TypeScript — no client generator exists.
5. ⛔ Not measured: assembly < 15 ms for 200 endpoints, `/openapi.json` from cache < 100 µs. The
   caching *is* implemented (pre-serialised bytes + ETag + 304); there is no `examples/bench` and no
   benchmark harness to put a number on it.
6. ✅ `/docs` renders with no outbound requests — asserted directly over the template (no absolute
   URL, no `<script src>`, no external stylesheet, no `@import`) rather than by blocking egress.
7. ✅ Two schemas with the same `schema_name()` are collected by `SchemaGenerator::collisions()` and
   become a boot error naming both types.
