# 01 — Goals, Non-Goals, and Success Metrics

## Product goals (ranked; ties are broken by rank)

1. **G1 — Model-driven core.** A single type definition drives request parsing, validation,
   response serialisation, and the OpenAPI document. No duplicate declarations, no drift.
2. **G2 — Batteries included, batteries removable.** ORM, migrations, auth, authorization,
   sessions, KV/cache, jobs, mail, admin, storage — all present, all behind feature flags, all
   replaceable by a documented trait.
3. **G3 — Diagnostics as a feature.** The framework never emits an unexplained trait-bound error.
   Every failure mode Moso can cause has a hand-written message with a fix.
4. **G4 — Conventions that scale.** A generated project layout that is correct at 3 endpoints and
   still correct at 300, with a documented migration path to a Cargo workspace.
5. **G5 — Iteration speed.** Architected so an app-code edit recompiles a small, stable unit.
   Numeric budgets in `04-devex/42-compile-times.md`.
6. **G6 — Escape hatches everywhere.** Any Moso abstraction can be bypassed for the layer below
   (Axum, Tower, sqlx, raw SQL, raw `hyper`) without leaving the framework.
7. **G7 — Production defaults.** Observability, graceful shutdown, health checks, security headers,
   rate limiting, and structured errors are on by default and correct.

## Technical goals

- **T1** — Async-native throughout, Tokio only. No runtime-agnostic abstraction layer.
- **T2** — `#![forbid(unsafe_code)]` in every Moso crate except where a documented, reviewed,
  benchmark-justified exception exists (target: zero exceptions at 1.0).
- **T3** — OpenAPI **3.1** output (JSON Schema 2020-12 aligned), not 3.0.
- **T4** — Postgres is the first-class database. SQLite is fully supported for dev/test/embedded.
  MySQL is best-effort and community-maintained.
- **T5** — Stable-Rust only. No nightly features, ever. Edition 2024. MSRV = stable minus 2
  releases, bumped in minor versions only.
- **T6** — Every public trait is either sealed or documented as an extension point. No accidental
  API surface.
- **T7** — Compile-time cost of the framework is measured in CI and is a release gate.

## Non-goals

These are *deliberately out of scope*. Reopening one requires an ADR.

| Non-goal | Why | What to do instead |
| --- | --- | --- |
| Being the fastest framework in benchmarks | Optimising for TechEmpower distorts API design; our users' bottleneck is the database | Publish honest overhead-vs-bare-Axum numbers |
| Runtime-agnostic (smol/async-std support) | Doubles the surface, serves ~0% of users, the ecosystem consolidated on Tokio | Use Tokio |
| A frontend framework or SSR-first story | Different product; Leptos/Dioxus own it | First-class SPA/API support; server-rendered HTML only in the admin |
| Replacing Axum/Tower/Hyper | The substrate war is over | Build on it, expose it |
| Supporting every database | Dialect breadth is where ORMs go to die | Postgres + SQLite excellently; a `Driver` trait for others |
| GraphQL as a core concern | Distinct model; `async-graphql` is excellent | An integration guide, maybe a `moso-graphql` bridge post-1.0 |
| Multi-language codegen beyond typed clients | Scope creep | OpenAPI is the contract; generate TS/Python clients only |
| A plugin marketplace / package registry | Premature | crates.io + a curated "awesome" list |
| Windows-first development experience | Cost/benefit; CI will test it, but `moso dev` tuning targets Linux/macOS | Support it, don't optimise it |

## Anti-goals (things we will actively refuse)

- **No magic that cannot be printed.** Every derive expansion must be inspectable via
  `cargo expand` and documented in `06-reference/62-macro-reference.md`. If a behaviour cannot be
  explained by pointing at generated code, it does not ship.
- **No global mutable state.** No `lazy_static` registries that make tests order-dependent, no
  hidden singletons. The `App` value owns everything. (Exception: `inventory`-style link-time
  collection is explicitly banned — see ADR-0004.)
- **No stringly-typed APIs where a type will do.** Permissions, job names, config keys, column
  names, and route names are all typed.
- **No "you must restructure your app to use this feature."** Every battery is additive.
- **No breaking changes without a mechanical migration.** If we break it, `moso migrate` fixes it
  or `cargo fix --moso` does. See `05-delivery/53-quality-gates.md`.

## Success metrics (instrumented, not vibes)

### Loop 1 — first five minutes
- `moso new demo && cd demo && moso dev` succeeds on a machine with only `rustup` and Docker.
- Time from `moso new` to a browsable `/docs`: **< 5 min cold, < 60 s warm** (M1 gate).
- Number of files a user must edit to add a working, documented, validated endpoint: **1**.

### Loop 2 — the edit loop
- Incremental rebuild after changing a handler body, 50-endpoint reference app,
  `mold`/`rust-lld` + Cranelift dev profile: **p50 < 3 s, p95 < 6 s** (M2 gate).
- Cold `cargo build` of the reference app, all features: **< 90 s on 8 cores** (M2 gate).
- Percentage of framework-caused compile errors that carry a hand-written diagnostic: **100%**,
  enforced by the `trybuild` corpus in `crates/moso-ui-tests`.

### Loop 3 — scale
- Reference app at 200 endpoints: no single file over 300 lines, no compile-time cliff
  (super-linear growth ratio < 1.2× between 50→100→200 endpoints).
- Zero-drift guarantee: a CI test proves the committed `openapi.json` matches the code.

### Runtime
- Overhead vs. hand-written Axum on a JSON echo endpoint: **< 5% p99 latency**, **< 10% throughput**
  (M2 gate; published, reproducible, in-repo harness).
- Idle RSS of the reference app: **< 30 MB**.

### Ecosystem
- ≥ 3 maintainers with commit rights at 0.1 announcement.
- ≥ 10 non-founder contributors within 3 months of 0.1.
- ≥ 3 named production users at 0.2.

## Explicit trade-offs we accept

| We accept | In exchange for |
| --- | --- |
| Heavier proc-macro use than idiomatic Rust libraries | Zero-annotation OpenAPI and one-derive models |
| A sealed facade around the query builder (users can't reach `sea-query` directly) | Freedom to replace the query engine without a breaking change |
| Opinionated project layout | Generators, the admin panel, and `moso check` can assume structure |
| Postgres-first feature set | A data layer that is actually good rather than uniformly mediocre |
| No runtime reflection, so a bare attribute macro is required per handler | Compile-time everything, no startup cost, no drift |

## Definition of done for M1 (the "it's real" bar)

Moso 0.1 ships when a developer can, with no third-party crates beyond what `moso new` generates:

1. Define a `User` entity and a `CreateUser` schema.
2. Generate and run a migration.
3. Write a `POST /users` handler with validated input and a typed response.
4. See it documented, with correct schemas and a 422 example, at `/docs`.
5. Register a user, log them in with a session cookie, and gate an endpoint on a permission.
6. Enqueue a background job from inside the request transaction.
7. Write an integration test that runs against a real, isolated database.
8. Deploy the result as a single container image built from the generated `Dockerfile`.

Anything not required by those eight steps is M2 or later.
