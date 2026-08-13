# 51 - Work Packages

> **Status: WP-02 – WP-10 and WP-21 are done; WP-23 and WP-24 partially; the rest are not started.**
> The per-WP ledger, with what is missing from each partial one, is
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).
>
> Rule 5 below - *"if a WP's design document is wrong or under-specified, update the document in the
> same PR"* - was applied to everything M1 touched. Where the divergence was a **decision** rather
> than an oversight, an ADR records it: [ADR-0013](../adr/0013-handler-registration.md) is the one
> this build produced, and [ADR-0002](../adr/0002-own-the-handler-traits.md) grew an implementation
> note.

> This document is the build order. Each work package (WP) is independently assignable, has explicit
> inputs, outputs, file paths, dependencies, and acceptance criteria. An engineer or agent assigned
> a WP should need no other context than the WP entry and the documents it references.

## How to use this document

1. Pick a WP whose dependencies are all **Done**.
2. Read the referenced design documents in full before writing code.
3. Implement to the acceptance criteria. The criteria are the definition of done, not a suggestion.
4. Every WP delivers: code + tests + rustdoc + an entry in the changelog + any UI tests for new
   diagnostics.
5. If a WP's design document is wrong or under-specified, **update the document in the same PR**.
   The documents are the source of truth and must not rot.

## Conventions

- `→` marks a hard dependency.
- **Size** is a rough estimate for one experienced Rust engineer: S ≤ 3 days, M ≤ 2 weeks,
  L ≤ 5 weeks, XL > 5 weeks (XL packages should be split before starting).
- **Parallel group** indicates which WPs can proceed simultaneously.

---

## Phase 0 - Foundation

### WP-00 · Repository & CI  · S · no deps
**Do:** workspace `Cargo.toml`, `rust-toolchain.toml`, `deny.toml`, `.github/workflows/`,
licence (`MIT`, ADR-0018), issue templates.
**Files:** repo root, `.github/`.
**Accept:** CI runs fmt, clippy (`-D warnings`), test, deny, audit on Linux/macOS/Windows; a
scheduled advisory job exists; branch protection documented.

### WP-01 · `xtask` measurement harness · M · → WP-00
**Do:** `xtask` crate with `bench-compile`, `expand-size`, `check-sealed`, `check-deps`,
`check-diagnostics`, `release`.
**Design:** `04-devex/42`, `00-foundations/03`, `04-devex/41`.
**Accept:** `bench-compile` reproducible within ±5% over 5 runs; emits JSON; posts a PR comment
diffing against a committed baseline; `check-sealed` parses rustdoc JSON and fails on a foreign
type in a public signature; `check-diagnostics` fails when a public trait lacks
`on_unimplemented`.
**Note:** this WP is deliberately first. Do not start optimisation work without it.

### WP-02 · `moso-core` skeleton: `App` & lifecycle · M · → WP-00
**Design:** `01-http/10`.
**Files:** `crates/moso-core/src/{app.rs,lifecycle.rs,health.rs,shutdown.rs,resolver.rs}`.
**Deliver:** `App`, `AppBuilder`, `Resolver`, provider type-map, lifespan hooks, health checks,
graceful shutdown, the boot-error report renderer.
**Accept:** all criteria in `01-http/10 § Acceptance`. In particular: multi-problem boot report with
file:line and fixes; SIGTERM drains within grace; two apps in one process.

---

## Phase 1 - The model-driven core (parallel group A)

### WP-03 · Extractors, responses & the request pipeline · L · → WP-02
**Design:** `01-http/12`.
**Files:** `crates/moso-core/src/{extract/*,response/*,request_ctx.rs}`.
**Deliver:** `Extract`, `ExtractBody`, `Describe`, `Opaque`, the full built-in extractor and
response set, body limits, `serde_path_to_error` integration, the Axum interop blanket impls.
**Accept:** `01-http/12 § Acceptance`. Query-string behaviour table covered case by case.

### WP-04 · `Router`, `Handler`, `#[endpoint]`, `routes!` · L · → WP-03, WP-05
**Design:** `01-http/11`, `04-devex/41`, `06-reference/62`.
**Files:** `crates/moso-core/src/{router.rs,handler.rs,endpoint.rs}`,
`crates/moso-macros/src/{endpoint.rs,routes.rs}`.
**Deliver:** non-generic `Router`, `Handler<M>` blanket impls 0..=16, `#[endpoint]` with spec
generation + assertion codegen + source capture, `routes!`, path-syntax validation, route-conflict
detection, `moso routes` data.
**Accept:** `01-http/11 § Acceptance`. Body-not-last and legacy-path-syntax UI tests must pass with
the exact documented messages.

### WP-05 · `moso-schema` + `#[derive(Schema)]` · L · → WP-00
**Design:** `01-http/13`.
**Files:** `crates/moso-schema/src/*`, `crates/moso-macros/src/schema.rs`.
**Deliver:** `Schema`, `Validate`, `ValidationErrors`/`FieldError`, the full attribute vocabulary,
constrained types, `#[derive(Constrained)]`, enum representations, generic/recursive handling,
`MessageProvider`.
**Accept:** `01-http/13 § Acceptance`. Every attribute has a runtime test **and** a JSON Schema
snapshot test in the same test.
**Parallel with:** WP-03. This is the largest M1 package; consider splitting into
WP-05a (traits + types) and WP-05b (derive).

### WP-06 · `Error`, problem+json, the dev error page · M · → WP-03
**Design:** `01-http/16`.
**Files:** `crates/moso-core/src/error/*`, `crates/moso-macros/src/error.rs`.
**Accept:** `01-http/16 § Acceptance`. Canary-secret grep over error logs must pass.

### WP-07 · `moso-openapi` + document assembly · L · → WP-04, WP-05
**Design:** `01-http/14`.
**Files:** `crates/moso-openapi/src/*`, doc UI assets.
**Deliver:** the 3.1 document model, `SchemaGenerator` with `$defs` dedup and deterministic output,
assembly at `App::build()`, embedded Scalar/ReDoc/Swagger, `/openapi.{json,yaml}`, drift checking,
breaking-change classification.
**Accept:** `01-http/14 § Acceptance`. Meta-schema validation in CI; no outbound requests from
`/docs`.

### WP-08 · Dependency injection · M · → WP-02, WP-03
**Design:** `01-http/15`.
**Deliver:** `Inject`, `Dependency`, `Depends`, request-scoped cache, `ProviderReq`, boot-time graph
validation, `#[derive(Dependency)]`, test overrides.
**Accept:** `01-http/15 § Acceptance`. DI overhead < 200 ns benchmarked.

### WP-09 · Middleware stack · M · → WP-04
**Design:** `01-http/17`.
**Deliver:** `MiddlewareStack` with named slots, the default stack in the specified order,
`#[middleware]`, `Guard`, `moso middleware` output.
**Accept:** `01-http/17 § Acceptance`. Overhead < 3 µs.

### WP-10 · Configuration · M · → WP-02
**Design:** `01-http/18`.
**Deliver:** `#[derive(Config)]`, layered sources with the documented precedence, `SecretString`,
profiles, boot-error reporting, `moso config`, `.env.example` generation, `Reloadable<T>`, flags.
**Accept:** `01-http/18 § Acceptance`.

---

## Phase 2 - Data layer (parallel group B, gated on M1 exit)

### WP-11 · `moso-sql` sealed facade · M · → WP-00
**Design:** `02-data/20`.
**Deliver:** `Select`/`Insert`/`Update`/`Delete`/`Expr`/`Sql`/`Dialect`, Postgres and SQLite
dialects, delegating to `sea-query` internally.
**Accept:** `xtask check-sealed` passes; no foreign type in any public signature; snapshot tests of
generated SQL for every construct on both dialects.

### WP-12 · `#[derive(Entity)]` + query builder · XL → split · → WP-11, WP-05
**Design:** `02-data/21`.
**Split into:**
- **WP-12a** `Entity` trait, `Column<E,T>`, derive, `from_row`, `EntityDescriptor`, `NewE` generation.
- **WP-12b** `Select<E>`: filters, ordering, limits, `filter_opt`/`when`, fetch methods, scopes.
- **WP-12c** writes: insert/upsert/update/delete, atomic set, guards against unfiltered mutation.
- **WP-12d** projections: tuple + `#[derive(Projection)]`, aggregates, group by.
- **WP-12e** pagination: keyset cursors (signed), offset, `Page`.
**Accept:** `02-data/21 § Acceptance`. Shape stability asserted by a type-equality test; the
`ColumnValue` diagnostic UI test passes.

### WP-13 · Relations & preloading · L · → WP-12
**Design:** `02-data/22`.
**Deliver:** all four relation kinds, `Related<T>`, batched preloads incl. nested/filtered/
`limit_per_parent`, `with_count`, `load`/`load_many`, joins with the unjoined-column compile error,
`attach`/`detach`/`sync`, the dev N+1 detector.
**Accept:** `02-data/22 § Acceptance`. Statement-count assertions are the primary test form here.
**Note:** the joined-set type parameter is a **TODO(agent)** decision - see `02-data/22 § Joins`.
Prototype both and choose with measured ergonomics before locking the API.

### WP-14 · Migrations · L · → WP-12
**Design:** `02-data/23`.
**Deliver:** `EntityDescriptor` → schema snapshot, differ, migration generation (SQL + Rust),
runner with advisory locks, checksums, dirty state, destructive-change policy, lock/statement
timeouts, `moso db *`, `db check`, squash, seeds.
**Accept:** `02-data/23 § Acceptance`. Every row of the operation-coverage table tested on both
dialects; generator idempotence is non-negotiable.

### WP-15 · `Db`, transactions, pooling, replicas, tenancy · L · → WP-12
**Design:** `02-data/24`.
**Accept:** `02-data/24 § Acceptance`. The tenant-scope compile error is a UI test.

### WP-16 · `moso-kv` · M · → WP-02
**Design:** `02-data/25`.
**Deliver:** `KvStore`, memory/Redis/Postgres backends, `namespace!`, `#[cached]` with
single-flight, SWR, locks, rate limiter, degrade/fail modes, circuit breaker.
**Accept:** `02-data/25 § Acceptance`. The same suite passes on all three backends.
**Parallel with:** the ORM packages - `moso-kv` has no ORM dependency.

---

## Phase 3 - Batteries (parallel group C)

### WP-17 · `moso-auth` core · L · → WP-08, WP-16, WP-12
**Design:** `03-batteries/30`.
**Split:** WP-17a sessions + password + the account lifecycle; WP-17b JWT + API keys;
WP-17c OAuth2/OIDC; WP-17d passkeys + TOTP.
**Accept:** `03-batteries/30 § Acceptance`. Timing-equality and blocking-pool tests are mandatory,
not optional.

### WP-18 · `moso-authz` · L · → WP-17, WP-12
**Design:** `03-batteries/31`.
**Deliver:** `permissions!`, `roles!`, `PermSet`, `Policy`, `ScopedPolicy`, `Authorized<A,R>`,
`#[requires]`, `#[public]`, boot-time permission validation, `moso authz explain`, audit log.
**Accept:** `03-batteries/31 § Acceptance`.
**Note:** this is the differentiator with no incumbent. Give it the strongest engineer available.

### WP-19 · `moso-jobs` · L · → WP-12, WP-16
**Design:** `03-batteries/32`.
**Deliver:** `#[job]`, `Job`, registry with boot validation, Postgres/Redis/memory backends,
transactional enqueue + outbox, workers, leases/heartbeats, retries, DLQ, cron with leader
election, trace propagation, `drain()`.
**Accept:** `03-batteries/32 § Acceptance`. Benchmark 1000 jobs/s on Postgres before declaring done.
**Decision:** wrap `apalis` vs. build - decide by benchmark; record in an ADR.

### WP-20 · `moso-mail` + `moso-storage` + realtime · L · → WP-19
**Design:** `03-batteries/34`.
**Accept:** `03-batteries/34 § Acceptance`.

### WP-21 · `moso-test` · M · → WP-04, WP-15
**Design:** `04-devex/43`.
**Deliver:** `TestApp`, `TestClient` with rich failure output, DB strategies (template/transaction/
migrate), `#[derive(Factory)]`, `assert_queries!`, mail/jobs/log assertions, time control,
`assert_matches_openapi`, proptest strategy generation.
**Accept:** `04-devex/43 § Acceptance`. Spawn < 200 ms; 100 parallel tests isolated.
**Note:** build this early - every other WP's tests depend on it. Consider promoting to Phase 1
with a reduced scope (client + template DB only) and extending later.

### WP-22 · `moso-admin` · XL → split · → WP-13, WP-18, WP-19
**Design:** `03-batteries/33`.
**Split:** WP-22a shell + list/detail/edit + permissions; WP-22b filters/search/bulk/inlines;
WP-22c audit/history/import/export; WP-22d dashboard + jobs + flags + impersonation.
**Accept:** `03-batteries/33 § Acceptance`. The adversarial field-permission test and the WCAG check
are gates, not nice-to-haves.

---

## Phase 4 - Tooling, docs, release

### WP-23 · `moso-cli` · L · → WP-10, WP-14
**Design:** `04-devex/40`.
**Split:** WP-23a `new` + templates; WP-23b `dev` with watch/restart/request-replay;
WP-23c `generate`; WP-23d `check` lints; WP-23e `doctor`/`config`/`routes`/`middleware`;
WP-23f `deploy` artefacts; WP-23g distribution (binaries, binstall, Homebrew, completions).
**Accept:** `04-devex/40 § Acceptance`.

### WP-24 · Diagnostics pass · M · → all trait-defining WPs
**Design:** `04-devex/41`.
**Do:** `on_unimplemented` on every public trait, `do_not_recommend` on every blanket impl,
complete the `tests/ui/` corpus, wire `moso check` references into the messages, enforce the style
guide.
**Accept:** `04-devex/41 § Acceptance`. `check-diagnostics` green with zero allowances.

### WP-25 · Compile-time optimisation to budget · M · → WP-01, most of Phase 1–2
**Design:** `04-devex/42`.
**Accept:** every M2 budget met and published; sub-linear growth 50→200 endpoints;
`moso generate workspace` delivers the tabled improvement.

### WP-26 · Observability · M · → WP-09
**Design:** `04-devex/44`.
**Accept:** `04-devex/44 § Acceptance`. Trace propagation across request→job→outbound is the key
test.

### WP-27 · Security hardening & review · M · → Phase 3 complete
**Design:** `04-devex/45`.
**Do:** implement and test every default; fuzz targets; `moso deploy checklist`; SBOM and signing;
commission an **external security review** before 0.1.
**Accept:** `04-devex/45 § Acceptance` + external review findings resolved or documented.

### WP-28 · Documentation & website · L · continuous, gated at M4
**Design:** `04-devex/46`.
**Accept:** `04-devex/46 § Acceptance`.

### WP-29 · Governance & sustainability · S · → before public 0.1
**Design:** `05-delivery/52`.
**Accept:** three maintainers with commit rights; RFC process live; funding model decided and
documented.

### WP-30 · Reference applications · M · continuous
**Do:** `examples/minimal`, `examples/crud` (the tutorial app), `examples/realworld` (the RealWorld
spec, for cross-framework comparability), `examples/bench` (50/200-endpoint harness).
**Accept:** all four build in CI on every commit and are used by the benchmark and compile-time
harnesses. `realworld` passes the official RealWorld API test suite.

---

## Dependency graph (critical path)

```
WP-00 ─┬─ WP-01 ──────────────────────────────────────────────► WP-25
       ├─ WP-02 ─┬─ WP-03 ─┬─ WP-04 ─┬─ WP-07 ──► M1 exit
       │         │         │         ├─ WP-09 ──► WP-26
       │         │         │         └─ WP-21 ─────────────────► all tests
       │         │         ├─ WP-06
       │         │         └─ WP-08
       │         └─ WP-10
       ├─ WP-05 ─┘ (also feeds WP-07, WP-12)
       └─ WP-11 ─── WP-12 ─┬─ WP-13 ─┬───────────────► WP-22
                           ├─ WP-14 ─┴─ WP-23
                           └─ WP-15 ─┬─ WP-17 ─── WP-18 ──► WP-22
                                     └─ WP-19 ─── WP-20
       WP-16 (independent, feeds WP-17/WP-19)
```

**Critical path:** WP-00 → WP-02 → WP-03 → WP-04 → WP-07 (M1) → WP-12 → WP-13 (M2) → WP-17 →
WP-18 → WP-22 (M4).

## Parallelisation guidance for a multi-agent build

| Wave | Concurrent WPs | Notes |
| --- | --- | --- |
| 1 | WP-00 | must complete first |
| 2 | WP-01, WP-02, WP-05, WP-11 | four independent tracks |
| 3 | WP-03, WP-10, WP-16 | WP-03 gates most of Phase 1 |
| 4 | WP-04, WP-06, WP-08 | |
| 5 | WP-07, WP-09, WP-21, WP-12a-e | M1 closes when WP-07 lands |
| 6 | WP-13, WP-14, WP-15 | data layer completion |
| 7 | WP-17a-d, WP-19 | batteries |
| 8 | WP-18, WP-20, WP-23a-g, WP-26 | |
| 9 | WP-22a-d, WP-24, WP-25, WP-27, WP-28 | polish and release |

**Cross-cutting rule for agents:** WP-24 (diagnostics) and WP-28 (docs) are *not* end-phase
cleanup. Every WP delivers its own diagnostics and its own docs; WP-24 and WP-28 are the audits
that verify nothing was missed.

## Definition of done (every WP)

- [ ] Implemented to the referenced design document.
- [ ] All acceptance criteria in that document's `§ Acceptance` section pass.
- [ ] Unit + integration tests; UI tests for every new user-facing compile error.
- [ ] Rustdoc on every public item, with a runnable example.
- [ ] `#![forbid(unsafe_code)]` intact; `#![deny(missing_docs)]` intact.
- [ ] `moso check`-equivalent lints added where the feature introduces a misuse risk.
- [ ] Benchmarks added if the WP touches a hot path; budgets not regressed.
- [ ] Changelog entry.
- [ ] Design document updated if reality diverged from the spec.
