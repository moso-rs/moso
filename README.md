<div align="center">

<img src="media/logo.png" alt="Moso logo" width="128" />

# Moso

**A batteries-included, model-driven web framework for Rust.**

One type definition drives parsing, validation, serialisation and documentation - so they cannot drift. FastAPI's developer experience and Django's completeness, on the Tokio / Tower / Hyper / Axum substrate.

[![CI](https://github.com/moso-rs/moso/actions/workflows/ci.yml/badge.svg)](https://github.com/moso-rs/moso/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/moso.svg)](https://crates.io/crates/moso)
![Rust 2024](https://img.shields.io/badge/Rust_2024-000000?logo=rust&logoColor=white)
![MSRV 1.94](https://img.shields.io/badge/MSRV-1.94-000000?logo=rust&logoColor=white)
![Axum 0.8](https://img.shields.io/badge/Axum_0.8-CC342D)
![Tokio 1.53](https://img.shields.io/badge/Tokio_1.53-172F45)
![OpenAPI 3.1](https://img.shields.io/badge/OpenAPI-3.1.1-6BA539?logo=openapiinitiative&logoColor=white)
![PostgreSQL](https://img.shields.io/badge/Postgres-4169E1?logo=postgresql&logoColor=white)
![SQLite](https://img.shields.io/badge/SQLite-003B57?logo=sqlite&logoColor=white)
![unsafe_code forbidden](https://img.shields.io/badge/unsafe__code-forbidden-2ea44f)
[![Licence: MIT](https://img.shields.io/badge/licence-MIT-blue)](LICENSE)

<a href="https://skillicons.dev">
  <img src="https://skillicons.dev/icons?i=rust,postgres,sqlite,redis,docker,githubactions" alt="Rust, Postgres, SQLite, Redis, Docker, GitHub Actions" />
</a>

[Get started](#quickstart) · [Architecture](docs/00-foundations/02-architecture.md) · [What is actually built](docs/06-reference/63-implementation-status.md) · [All docs](#documentation)

</div>

---

## What Moso is

Moso is two things at once.

**A model-driven framework.** One `#[derive(Schema)]` is the source of truth for a type: serde
deserialisation, field-level validation, the JSON Schema, and the entry in the OpenAPI document all
come from the same declaration. A request body cannot become a `T` without `T::validate` having run,
and a handler cannot return a shape the document does not describe. There is no annotation to keep
in sync, because there is nothing to sync - the document is derived, not written. Around that core
sit the batteries FastAPI never had: an ORM and migration runner, KV and cache, authentication,
authorization, background jobs, mail and storage.

```rust
/// Create a post.
///
/// The slug is derived from the title and suffixed if it collides, so two posts
/// called "Hello" become `hello` and `hello-2`. Requires the API key.
#[endpoint(errors = BlogError)]
async fn create(
    Inject(store): Inject<Store>,          // provider - proven to exist at boot
    Depends(actor): Depends<Actor>,        // per-request, memoised, may fail
    Json(body): Json<CreatePost>,          // parsed + validated + documented
) -> Result<Created<PostOut>> {            // status + schema + Location, documented
    let post = store.create(body, &actor.name).await?;
    let location = format!("/api/v1/posts/{}", post.id);
    Ok(Created::at(location, post.into()))
}
```

That is `examples/crud`, with only the trailing comments added. No OpenAPI annotation, no
`.validate()?`, no `unwrap` on a missing provider - the doc comment above becomes the operation
summary, and `/docs` is correct because it was derived from the signature. The example now runs on
the real batteries: `Post` is a `#[derive(Entity)]` over SQLite, the write guard is `moso-auth`'s
`ApiKeyAuthenticator`, and its tests pass with no server running.

**A specification you can hold it to.** Moso was designed on paper before it was written. The 41
documents in [`docs/`](docs/README.md) are normative - they state what to build, why the choice was
made, what the public API is, and what "done" means - and 18 [ADRs](docs/adr/README.md) record the
decisions expensive enough to need a reason on file.
[`63-implementation-status.md`](docs/06-reference/63-implementation-status.md) is the ledger that
keeps the two honest, document by document.

**Status: published on crates.io.** The workspace ships as 17 crates at `0.0.1`, installable with
`cargo add moso`. The HTTP
layer, the data layer, and every battery except the admin panel are built and tested - auth
(including the bearer-token flow), observability with OTLP export, the OpenAPI document, the CLI and
the test harness all ship. What is deliberately *not* here is named under the table below and never
implied. Two `xtask` budget gates stand red by design; see the end of that section.

## How you build with it

Moso ships a CLI whose command tree is complete. No subcommand prints "coming soon" - an unbuilt
idea is absent from the tree rather than stubbed.

- **`moso new shop`** - scaffolds the composition-root layout: `Cargo.toml`, `.gitignore`,
  `.env.example`, `.cargo/config.toml`, `Dockerfile`, `.dockerignore`, `README.md`,
  `src/{lib,main,routes,dump}.rs` and `tests/api.rs`; `--with-db` adds `src/db.rs` and a first
  migration, and `--auth` copies documented auth handlers and an end-to-end `tests/auth.rs` into the
  project. `main.rs` is a shim over `lib.rs`, so integration tests construct the *real* application
  rather than a parallel test-only copy.
- **`moso dev`** - the edit loop. Watches by mtime polling, debounces a burst, rebuilds, and
  **keeps the previous server serving when a build fails**.
- **`moso generate`** - writes an `endpoint`, `schema`, `error`, `middleware` or `workspace` into the
  project and registers it in `src/lib.rs`; `test` writes a `tests/<name>.rs`.
- **`moso db status|migrate|rollback|redo|make-migration|check|squash|seed`** - drives `moso-migrate`
  over your `migrations/` directory through the `--db-*` protocol `moso new --with-db` writes into
  `src/db.rs`. A ledger that disagrees with the files on disk exits 1, so it can gate CI.
- **`moso openapi export|check`** (with `--breaking` and `--prefix`), **`moso routes`**,
  **`moso middleware`**, **`moso jobs`**, **`moso authz`**, **`moso client`** (TypeScript + Rust),
  **`moso check`**, **`moso auth calibrate`**, **`moso config`**, **`moso doctor`** and
  **`moso deploy checklist`** are all built.

What keeps hand-written and generated code correct is not the generator, it is the gates: `cargo
fmt`, `cargo lint`, `cargo nextest run`, `cargo docs`, `cargo ui`, `cargo deny check`, and the
structural `xtask` checks that fail a build which breaks crate layering, unseals the SQL facade, or
ships a public trait without a hand-written diagnostic. A `bacon.toml` at the root drives the same
gates in a live loop.

## What's already built for you

| Area | What you get |
| --- | --- |
| Model layer | `#[derive(Schema)]` → serde + validation + JSON Schema 2020-12 from one declaration; the full `#[schema(..)]` vocabulary, enum representations, generics, recursion; nine constrained types (`Email`, `Password`, `Slug`, `Url`, `Id`, `Cursor`, `Bounded`, `Text`, `Net`) and `#[derive(Constrained)]` for your own |
| OpenAPI | 3.1.1, assembled at boot from the types themselves - **zero per-handler annotation**; deterministic byte-stable output, `/openapi.json` + `/openapi.yaml`, an embedded docs UI (also at `/redoc` and `/swagger` behind their features), a per-response CSP nonce, all doc routes forced off in production, `moso openapi export`, and drift plus breaking-change classification via `moso openapi check --breaking` |
| HTTP | `Router` that is **not generic over state** (no `FromRef`), `#[endpoint]`, `routes!`, `ep!`, const-validated paths, boot-time conflict detection naming both source locations |
| Extractors & responses | `Json`, `Path`, `Query`, `Form`, `Headers`, `Cookies`, `Body`, `Multipart` (gated); `Created`, `Page`, `Cursor`, `NoContent`, `Redirect`, `File`, `Sse`, `Text`, `Cached`, `Either`, `Raw` - each with a `describe()` that feeds the document |
| Errors | One concrete `Error`, 23 response kinds, RFC 9457 problem+json, per-field JSON Pointers on 422s, 5xx detail suppression, `#[derive(Error)]`, a developer error page |
| DI | `Inject` (app-lifetime providers, **validated at boot** - a missing provider is a boot error with a fix, never a 500) and `Depends` (per-request, memoised, fallible), over a `provide_with` DAG with cycle detection |
| Middleware | A `MiddlewareStack` of 15 named slots in a fixed order - panic catch, request id, trace, sensitive headers, catch-error, request limits, timeout, body limit, normalize-path, CORS, security headers, compression, rate limit, session, metrics - plus `#[middleware]`, `Guard`, and per-route layers |
| Config | `#[derive(Config)]`, layered sources with documented precedence, profiles, `SecretString`/`SecretBytes` (zeroized, redacting `Debug`), `Reloadable<T>`, `.env.example` generation |
| Observability | A tracing subscriber installed from config, OTLP **trace** export behind an off-by-default `otel` feature (grpc-tonic, no OpenSSL), W3C `traceparent` parenting, `db.query` / `kv.op` / job-execution spans, job trace propagation, a process-wide counter/gauge sink, request metrics with a cardinality cap |
| Data | `moso-orm`: `#[derive(Entity)]`, shape-stable `Select<E>`, projections, cursors, `Related<T>` with preloading, `Db`/`Tx`/`RequestTx`. `moso-sql` is a **sealed** construction facade - `xtask check-sealed` proves 0 foreign paths escape its 2,210 public items |
| Migrations | `moso-migrate`: introspect, diff, plan, emit, run, ledger, rename detection, destructive-change advice, `#[migration]` - mandatory-behaviour tests against real Postgres |
| KV & cache | `moso-kv`: `KvStore`, `namespace!`, `cached!`, plus locks, single-flight, circuit breaker, GCRA rate limiting, a bus and health checks - memory, Redis and Postgres backends all passing one shared conformance suite |
| Auth | `moso-auth`: sessions, passwords (argon2id), JWT + JWKS, OAuth2/OIDC, MFA/TOTP, API keys, passkeys (behind an off-by-default `passkeys` feature), a mounted **bearer-token flow** (`/auth/token` + `/auth/refresh` with reuse detection), table-backed stores, `AuthConfig::from_env`, and the account lifecycle |
| Authorization | `moso-authz`: `permissions!`, `roles!`, `#[requires]`, `#[public]`, resource policies, query-level scoping, explain traces, audit and redaction |
| Jobs | `moso-jobs`: `#[job]`, transactional enqueue through an outbox, retries, DLQ, cron and scheduling, health and metrics, actor and trace propagation into execution - Redis, Postgres and in-memory backends |
| Mail & storage | `moso-mail`: `Mailer`, in-house MIME, minijinja templates, preview, suppression, webhooks, and SMTP/SES/SendGrid/Mailgun/Postmark/Resend. `moso-storage`: objects, presigning (S3/GCS/Azure), multipart upload, serving, attachments - local/memory/S3/GCS/Azure |
| Testing | `moso-test`: `TestApp`, `TestClient`, `TestResponse`, `TestClock`, `#[derive(Factory)]`, battery accessors (`db()`/`kv()`/`mail()`/`jobs()`/`storage()`), `assert_sent`, `override_dependency`, an SSE client, log assertions, JSON diffing, OpenAPI contract assertions, in-process **and** real-socket transports |
| Diagnostics | `#[diagnostic::on_unimplemented]` on **100 % of public traits**, `do_not_recommend` on every blanket impl, hand-written macro errors with Levenshtein suggestions, and a 21-case `trybuild` corpus where a degraded message is a failing test |
| Security | Security headers, sensitive-header redaction, `expose_internal_errors` off in every profile with a boot warning when forced on, request limits enforced before allocation, signed and private cookies built with the `cookie` crate, `trusted_proxies` empty by default |
| Facade & CLI | One `moso` crate re-exports everything behind off-by-default features (`orm`, `auth`, `authz`, `jobs`, `kv`, `mail`, `storage`, `passkeys`, `otel`, …) plus a `full` feature; a 19-command CLI; four GitHub workflows, `deny.toml`, `cargo-audit`, and the structural `xtask` gates |

**Not built, and named rather than implied:** the admin panel (`moso-admin` is the one battery with
no crate); MySQL (Postgres and SQLite only - [ADR-0010](docs/adr/0010-postgres-first.md)); an image
codec for storage variants (a seam, so the encoder is yours); a bundled Fluent/i18n stack
(`MessageProvider` is the seam); CSRF; a native WebSocket contract (the `ws` feature re-exposes
Axum's); the tutorial; and `examples/bench` / `examples/realworld`. OpenTelemetry exports *traces*,
not metrics - metrics stay behind the `MetricsRecorder` seam. Two `xtask` budget gates are **red** on
this tree by design - `check-deps` rule 6 (155 third-party crates against a budget of 90) and
`expand-size` (the macro-expansion budgets) - and neither is closed by lowering the number.

## Quickstart

```sh
cargo add moso                             # the facade, published on crates.io at 0.0.1
cargo install moso-cli                     # the CLI, if you want the scaffolder
moso new shop                              # scaffold an application
```

Prefer to work from a clone? `cargo run -p moso-cli -- new shop` scaffolds the same application from
the workspace without installing the CLI.

Tests run on a laptop with no Docker: every database test gates on `DATABASE_URL` and skips with a
clear message when it is unset, and the macOS leg of CI runs the whole suite with it deliberately
unset to keep that true.

```sh
cargo nextest run --workspace --all-features          # passes with no servers (DB/Redis tests skip)

./scripts/test-db.sh up                               # Postgres 17 on 55433, Redis 7 on 56379
eval "$(./scripts/test-db.sh env)"                    # DATABASE_URL and REDIS_URL together
cargo nextest run --workspace --all-features          # the ORM, migration, KV and queue legs now run
```

A skipped test is still a passing test, so the second run is the one that exercises everything.
`compose.test.yaml` provisions both Postgres and Redis, and so does CI, so a documented command
never leaves a suite silently unexercised. `cargo test --workspace --all-features --doc` runs the
doctests, which `nextest` cannot.

## Stack

| Layer | Choice |
| --- | --- |
| Engine | `axum` 0.8 - Moso wraps `axum::Router`/`matchit` and **re-exports** its `IntoResponse` rather than defining a parallel trait ([ADR-0001](docs/adr/0001-build-on-axum.md)) |
| Runtime | `tokio` 1.53, the only supported runtime ([ADR-0011](docs/adr/0011-tokio-only.md)) - no runtime-agnostic layer |
| Server | `hyper` 1.11 + `hyper-util` - HTTP/1 + HTTP/2 on one listener, graceful shutdown |
| Middleware | `tower` 0.5 + `tower-http` 0.7; a handler is erased to `BoxCloneSyncService` at registration (rule A2) |
| Serialisation | `serde` 1.0 + `serde_json` with `preserve_order`, and `serde_path_to_error` so a bad body reports the RFC 6901 pointer `/items/2/qty` rather than a type name |
| Determinism | `indexmap` throughout the document model - a `HashMap` would reshuffle `openapi.json` every build and make the committed file undiffable |
| SQL | `sqlx` executes; `sea-query` constructs, behind a sealed facade so it appears in **no** public signature ([ADR-0005](docs/adr/0005-sealed-sql-facade.md)) |
| Databases | Postgres first-class, SQLite fully supported, MySQL deliberately absent ([ADR-0010](docs/adr/0010-postgres-first.md)) |
| Observability | `tracing` + `tracing-subscriber`; `opentelemetry` / `opentelemetry-otlp` / `tracing-opentelemetry` behind the `otel` feature |
| Macros | `syn` 2 + `quote` + `proc-macro2` + `darling` + `heck`; expansions name `::moso::__private::X` and nothing else |
| Secrets | `zeroize` behind `SecretString`/`SecretBytes`; `arc-swap` so `Reloadable<T>` costs no lock on the request path |
| Auth crypto | `ring` and RustCrypto (`argon2`, `sha2`, `hmac`); passkeys via `webauthn-rs` behind the `passkeys` feature |
| CLI | `clap` 4 + `clap_complete`; the CLI depends on no Moso crate and drives an application through a `--dump-*` protocol |
| Tests | `cargo-nextest`, `trybuild` for the diagnostics corpus, real Postgres and real Redis - there are no mocked data-layer tests, because a mocked one proves nothing about SQL |

Third-party versions belong in `[workspace.dependencies]`, declared once, each with its rationale as
a comment above it. `unsafe_code` is **forbidden** and `missing_docs` **denied** in every crate; the
two `examples/` members deliberately opt out so they read like code a user would write.

## Documentation

The design documents are the source of truth, and a change that diverges from one updates it in the
same pull request.

| Doc | What's inside |
| --- | --- |
| [`docs/README.md`](docs/README.md) | The index, the reading order, and the conventions these documents use |
| [63-implementation-status.md](docs/06-reference/63-implementation-status.md) | **What is actually built**, document by document |
| [00-vision.md](docs/00-foundations/00-vision.md) | Thesis, target user, competitive position, the three existential risks |
| [02-architecture.md](docs/00-foundations/02-architecture.md) | The layer cake, the Axum relationship, the life of a request, invariants I1–I4 |
| [03-crate-layout.md](docs/00-foundations/03-crate-layout.md) | Every crate, its dependencies, its public surface, the dependency budget |
| [11-routing.md](docs/01-http/11-routing.md) · [12-extractors-responses.md](docs/01-http/12-extractors-responses.md) · [13-schema-validation.md](docs/01-http/13-schema-validation.md) | The HTTP layer: routing, the self-describing extractor set, `#[derive(Schema)]` |
| [14-openapi.md](docs/01-http/14-openapi.md) · [15-dependency-injection.md](docs/01-http/15-dependency-injection.md) · [16-errors.md](docs/01-http/16-errors.md) · [17-middleware.md](docs/01-http/17-middleware.md) · [18-configuration.md](docs/01-http/18-configuration.md) | OpenAPI, DI, errors, the middleware stack, configuration |
| [20–25](docs/02-data/20-orm-overview.md) | The data layer: ORM, entities, relations, migrations, transactions, KV/cache |
| [30–34](docs/03-batteries/30-auth.md) | The batteries: auth, authorization, jobs, mail/storage |
| [40–46](docs/04-devex/40-cli.md) | Developer experience: the CLI, diagnostics, compile times, testing, observability, security |
| [60](docs/06-reference/60-example-app.md) · [61](docs/06-reference/61-api-reference.md) · [62](docs/06-reference/62-macro-reference.md) | The example app, every public signature, the exact expansion of every macro |
| [`docs/adr/`](docs/adr/README.md) | 18 decision records, each with its alternatives and its reversal criteria |

The rules a contributor or coding agent must not break live in [`AGENTS.md`](AGENTS.md).

**The one rule:** the type is the contract. A body cannot become a `T` without `T::validate` having
run, a handler cannot return a shape the OpenAPI document does not describe, and every provider a
handler injects is proven to exist before the server binds a port. Parsing, validation,
serialisation and documentation are four views of one declaration - so they cannot drift.

## Requirements

Rust ≥ 1.94 stable - `rust-toolchain.toml` selects it and installs `rustfmt` and `clippy`, so there
is no `rustup component add` ritual · Docker, only to run the test Postgres and Redis · no nightly,
ever.

## Licence

[MIT](LICENSE) © 2026 Alessandro Zucchiatti. Use it in anything, commercial or not; keep the copyright
notice. See [ADR-0018](docs/adr/0018-mit-relicence.md) for why Moso is permissively licensed (it was
briefly AGPL - the trail is on file).
