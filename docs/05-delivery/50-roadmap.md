# 50 - Roadmap, Milestones & Kill Criteria

> **Status: this build is M1.** The model-driven core is complete; WP-21 (`moso-test`) was pulled
> forward and WP-23 (`moso-cli`) landed partially. M2 onward is untouched. Per-work-package detail:
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## Shape of the plan

Six milestones. Each has a **theme**, a **demo** (the thing you can show), an **exit gate**
(objective, testable), and where relevant a **kill criterion**. Nothing ships publicly before M1.

The sequencing principle: **prove the differentiator first.** M1 exists to answer "is the
model-driven core actually better than assembling Axum + utoipa + validator by hand?" If it is not,
nothing else matters and we should find out in three months rather than eighteen.

---

## M0 - Skeleton (weeks 1–4)

**Theme:** make the repository real and the measurement harness exist before anything is optimised.

- Workspace, CI, `cargo-deny`, release automation, `xtask`.
- `xtask bench-compile`, `xtask expand-size`, `xtask check-sealed`, `xtask check-diagnostics`.
- `moso-core` skeleton: `App`, `Router`, `Handler`, `Extract`, `Error` - no batteries.
- `tests/ui/` corpus with the first five cases.
- `examples/minimal` compiles and serves.

**Exit gate:** `cargo build` green on Linux/macOS/Windows; the four xtask tools run in CI and
produce baseline numbers; a 30-line hello-world app serves a request.

**Why the harness first:** every later decision about compile time and diagnostics is guesswork
without it. Build the instrument before the experiment.

---

## M1 - The model-driven core (months 2–5) - *the thesis test*

**Theme:** one type definition drives parsing, validation, serialisation, and OpenAPI.

- `#[derive(Schema)]` with the full attribute vocabulary and constrained types.
- Extractors and responses with `describe()`.
- `#[endpoint]`, `routes!`, `Router`, assertion codegen.
- OpenAPI 3.1 assembly, Scalar UI, `moso openapi check`.
- `Error` + RFC 9457, validation 422s with pointers.
- DI: `Inject`, `Depends`, boot-time graph validation.
- Config with `#[derive(Config)]`.
- `moso new`, `moso dev`, `moso routes`, `moso check` (first five lints).
- `moso-test`: `TestApp`, `TestClient`.
- The tutorial's first three chapters.

**Demo:** a validated, documented, tested CRUD API written in one file, with `/docs` live, and a
side-by-side comparison against the equivalent Axum + utoipa + validator + serde code (target:
**40% fewer lines and one source of truth instead of three**).

**Exit gate:**
- Zero `#[endpoint(...)]` arguments needed for the reference API.
- The generated OpenAPI validates against the 3.1 meta-schema.
- All M1 acceptance criteria in `01-http/*` pass.
- `moso new` → `/docs` in under 60 s warm.
- Ten external developers try the demo; ≥ 7 say the model loop is better than what they use today.

**Kill criterion:** if the external feedback is not clearly positive on the *core loop*, stop and
reconsider the whole thesis before building batteries. This is the cheapest possible off-ramp.

---

## M2 - Data layer (months 5–10) - *the risk milestone*

**Theme:** the ORM that has to be better than the alternatives.

- `moso-sql` sealed facade.
- `#[derive(Entity)]`, columns, shape-stable `Select<E>`, projections, writes.
- Relations, batched preloads, `Related<T>`, joins.
- Migrations: snapshot, differ, generator, runner, safety policy.
- Transactions, pooling, replicas, `RequestTx`.
- `moso db *` commands.
- DB testing: template strategy, factories, `assert_queries!`.
- Compile-time budgets met and published.

**Demo:** the tutorial's shop with relations, migrations generated from entity edits, and a
statement-count assertion proving no N+1 - next to the SeaORM and Diesel equivalents.

**Exit gate:** all non-negotiables N1–N8 from `02-data/20` demonstrated; benchmark suite published
including where we lose; migration operation-coverage table fully implemented and tested on Postgres
and SQLite.

**Kill criterion (from `02-data/20`):** if N1 or N3 is not achieved, or ORM-attributable compile
time exceeds 20 s on the reference app, or two consecutive milestones slip on data work, or
construction overhead is > 40% above hand-written sqlx - **abandon the bespoke ORM** and ship a thin
integration over SeaORM, keeping the migration generator and the preload API. Decide this at a
scheduled review, not by drift.

---

## M3 - Batteries (months 10–15)

**Theme:** everything FastAPI does not have.

- `moso-auth`: sessions, passwords, JWT, OAuth2, passkeys, API keys, the account lifecycle.
- `moso-authz`: permission registry, roles, policies, `Authorized`, query scoping, explain.
- `moso-kv`: trait, memory/Redis/Postgres backends, `#[cached]`, rate limiting, locks.
- `moso-jobs`: `#[job]`, transactional enqueue, workers, scheduler, DLQ.
- `moso-mail`, `moso-storage`, realtime (SSE/WS/Bus).
- `moso generate` for all resource kinds.
- Tutorial chapters 4–9.

**Exit gate:** the shop application uses every battery; all `03-batteries/*` acceptance criteria
pass; the security test suite from `04-devex/45` passes; `moso deploy checklist` implemented.

---

## M4 - Admin, polish & 0.1 public release (months 15–19)

**Theme:** ship it.

- `moso-admin` with pagination, filtering, search, inlines, audit, jobs dashboard.
- Diagnostics pass: every trait has `on_unimplemented`; the UI corpus is complete.
- Documentation complete: tutorial, three migration guides, cookbook, error reference,
  `llms.txt`, MCP server.
- Website, playground, prebuilt CLI binaries.
- Governance and funding decision executed; the vulnerability-disclosure process (GitHub Security Advisories) live.
- Compile-time and runtime benchmarks published.

**Exit gate for the public 0.1:**
1. Three maintainers with commit rights.
2. Three production deployments (ours or friendly early users) running for 30 days.
3. All quality gates in `53-quality-gates.md` green.
4. Usability test: five unfamiliar developers reach a working endpoint in under 15 minutes.
5. A vulnerability-disclosure process (GitHub Security Advisories) and a named security contact live.

**Deliberately not before this point:** no public announcement, no Hacker News post, no conference
talk. A framework that is discovered before it is good gets one impression and it is the wrong one.
(Rocket's trajectory is the cautionary tale: enormous early hype, then a five-year gap.)

---

## M5 - Hardening & 0.2 (months 19–24)

**Theme:** respond to real users, stabilise.

- Whatever the first 90 days of real usage demands - this is deliberately unplanned capacity.
- Performance work against published benchmarks.
- MySQL support to parity or an honest downgrade of its status.
- Typed client generators (TS, Python) to production quality.
- API stability review; `#[non_exhaustive]` audit; the first deprecations.
- Begin the 1.0 stability discussion, with an explicit LTS policy.

---

## Beyond (not committed)

Candidates, ranked by expected value, to be re-evaluated with real user data: GraphQL bridge,
multi-region/edge deployment story, a hosted managed platform (the commercial model - see
`52-governance.md`), `subsecond` hot-patching, an AsyncAPI story for WebSockets, a plugin ecosystem,
gRPC.

---

## Resourcing assumptions

The plan assumes **two full-time engineers plus one part-time** through M3, growing to three
full-time by M4. With one engineer, multiply every duration by ~2.2 (not 3 - coordination overhead
is real but so is context-switching cost) and cut M3 to auth + jobs only, deferring admin to
post-0.1.

If resourcing is one engineer, the recommended scope reduction is explicit:
**M1 + M2 + auth + jobs, released as 0.1 without an admin panel**, positioned as "FastAPI for Rust"
rather than "Django for Rust." Shipping a narrower thing well beats shipping a broad thing badly -
which is the lesson of every framework in the survey that stalled.

## Standing review cadence

- **Weekly:** benchmark dashboard (compile time, runtime, binary size). Regressions are treated as
  bugs, not as "we'll fix it later."
- **Per milestone:** re-read the kill criteria out loud and answer them honestly in writing.
- **Monthly from M1:** three external developers use the current build and are watched, not
  surveyed. Watching someone hit an error you thought was clear is worth fifty issue reports.
