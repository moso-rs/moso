# 05 - Glossary

Terms are used with these exact meanings throughout the design documents. Where a term collides
with an Axum, Django, or FastAPI meaning, the difference is called out.

| Term | Meaning in Moso |
| --- | --- |
| **App** | The composed application value: config, providers, router, jobs, lifespan hooks. Constructed by `AppBuilder`, validated at `build()`. Not a singleton. |
| **Provider** | An app-lifetime dependency stored in the type-map (`Db`, `Kv`, `Mailer`). Retrieved with `Inject<T>`. Missing providers are a **boot** error. |
| **Dependency** | A request-scoped, memoised value implementing `Depends` (`CurrentUser`, `Tenant`). Resolution may fail into an HTTP error. FastAPI's `Depends()` covers both this and Provider; Moso splits them deliberately. |
| **Extractor** | A handler parameter type implementing `Extract` (from parts) or `ExtractBody` (consumes body). Extractors are **self-describing**: they contribute to the OpenAPI operation. |
| **Endpoint** | A handler function annotated `#[endpoint]`. The attribute generates the `OperationSpec`, the assertion codegen, and the adapter type. |
| **OperationSpec** | The compile-time-constructed description of one endpoint: parameters, request body, responses, security, tags, docs. Merged into the OpenAPI document at router build. |
| **Schema** | A type implementing `moso_schema::Schema`: serde + validation + JSON Schema, from one derive. The Moso analogue of a Pydantic model. Never an entity. |
| **DTO** | A `Schema` used as a request or response body. Input DTOs are `Create*`/`Update*`; output DTOs are `*Out`. |
| **Entity** | A type implementing `moso_orm::Entity`: maps to a table. Deliberately does *not* implement `Schema`. |
| **Projection** | A struct deriving `Projection`: a typed partial `SELECT` over one or more entities. |
| **Relation** | A declared association between entities (`has_many`, `belongs_to`, `has_one`, `many_to_many`). |
| **Loaded / `Related<T>`** | The state of a relation field. Accessing an unloaded relation returns `Err(NotLoaded)` rather than silently issuing a query - Moso never does implicit lazy loading. |
| **Preload** | Explicit eager loading (`.with(User::POSTS)`), executed as a batched second query. The N+1 prevention mechanism. |
| **Shape-stable builder** | A query builder whose Rust type does not change as clauses are added. `Select<User>` stays `Select<User>` after `.filter()`. The core fix for "40 lines of generic type vomit". |
| **Sealed facade** | A Moso-owned API whose implementation delegates to a third-party crate that never appears in a public signature (`moso-sql` over `sea-query`). Lets us swap the implementation without a breaking change. |
| **Battery** | An optional feature crate (`moso-auth`, `moso-jobs`, …) that depends on core and is enabled by a facade feature flag. |
| **Escape hatch** | A documented, tested API for dropping to the layer below (`Router::into_axum`, `Db::pool`, `Sql::raw`). Mandatory for every abstraction. |
| **Problem** | An RFC 9457 `application/problem+json` response body. The single error wire format. |
| **ErrorKind** | The taxonomy variant of `moso::Error` that determines status code, `type` URI, and whether detail is client-safe. |
| **Permission** | A typed, enumerable capability produced by `permissions!` (`Perm::PostsPublish`), never a bare string. |
| **Policy** | An implementation of `Policy<Action, Resource>` answering "may this actor do this to this object". Complements role/permission checks. |
| **Guard** | An extractor that fails the request if an authz condition is unmet (`Authorized<Publish, Post>`). |
| **Job** | A `#[job]`-annotated async fn with a typed payload, executed by a worker, with retry/backoff. |
| **Transactional enqueue** | Enqueuing a job inside the same DB transaction as the state change that justifies it, so the job cannot fire for a rolled-back write. |
| **Lifespan** | Startup/shutdown hooks run around serving, analogous to FastAPI's `lifespan`. |
| **Snapshot (schema)** | `migrations/.schema.json`: the serialised entity graph as of the last generated migration. The input to migration diffing. |
| **Drift** | Divergence between code and a committed artefact (`openapi.json`, `.schema.json`). Moso tests for drift in CI rather than regenerating silently. |
| **`moso check`** | Static analysis over a Moso project: DI graph, layering lints, blocking-in-async, drift, route conflicts. Not a compiler; complements it. |
| **UI test** | A `trybuild` test asserting the *exact* compiler output for a deliberately-wrong program. Our diagnostics regression suite. |
| **WP-nn** | A work package in `05-delivery/51-work-packages.md`: an independently assignable, acceptance-tested unit of implementation. |
| **Loop 1 / 2 / 3** | First-five-minutes / edit / scale. The three optimisation targets from `00-vision.md`. |
