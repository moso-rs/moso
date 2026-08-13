# 00 - Vision & Positioning

## The one-sentence thesis

> **Rust's web substrate is settled (Tokio/Tower/Hyper/Axum). The opinionated layer above it is
> not. Moso claims that layer by shipping the one thing FastAPI got right - a single type
> definition that drives parsing, validation, serialisation and documentation - plus the batteries
> FastAPI never had.**

## Why now

Three facts, taken together, define the opening:

1. **The substrate war is over.** Axum has roughly a 5:1 download lead over actix-web and is the
   default recommendation across the ecosystem. `async-std`'s discontinuation removed the last
   competing runtime story. Building a new HTTP core would be burning capital on a solved problem.
2. **The batteries layer is fragmented and immature.** Loco (Rails-shaped, tightly coupled to
   SeaORM, tiny adoption), Cot (Django-shaped, self-described as not production-ready, hand-rolling
   its own incomplete ORM), Pavex (technically impressive, commercial, closed-source, extra build
   step). None has won. None is close to winning.
3. **The reasons people don't reach for Rust for web apps are documented and specific** - they are
   not "Rust is hard." They are: no consensus data layer, annotation burden for OpenAPI,
   incomprehensible trait-bound errors, and the edit→run loop. Every one of those is a solvable
   product problem, not a language limitation.

## Who this is for

**Primary persona - "the FastAPI refugee."** A developer who ships product APIs, likes Python's
iteration speed, and is bleeding money or reliability on Python's runtime. They are *competent* in
Rust but not an expert in it, and their patience for a 60-line trait error is zero. They want to
type `moso new shop && moso dev` and have a migrating, authenticating, self-documenting API in
under five minutes.

**Secondary persona - "the Rails/Django refugee."** Wants generators, an admin panel, jobs, and
conventions. Evaluates frameworks by "how much do I have to assemble myself."

**Explicit non-persona.** The team writing a 2M-req/s edge proxy. They should use Axum directly, or
hyper. Moso will not out-benchmark hand-written Axum and will not pretend to. Our performance claim
is "within noise of a competent hand-rolled Axum app," not "fastest in TechEmpower."

## What "better than FastAPI" means, concretely

FastAPI's DX advantage is **mechanical, not magical**. It is reproducible. Here is the scorecard we
are held to:

| Capability | FastAPI | Axum today (assembled) | Moso target |
| --- | --- | --- | --- |
| One model → validation + serde + OpenAPI | ✅ Pydantic | ❌ three derives, kept in sync by hand | ✅ one `#[derive(Schema)]` |
| Zero per-handler OpenAPI annotation | ✅ | ❌ `#[utoipa::path(...)]` per handler | ✅ one bare `#[endpoint]` |
| Field-pathed 422 validation errors | ✅ | ❌ roll your own | ✅ built in, RFC 9457 |
| Dependency injection | ✅ `Depends()` | ⚠️ `State` + `FromRef` | ✅ `Inject` + `Depends`, validated at boot |
| Interactive docs at `/docs` | ✅ | ⚠️ manual mount | ✅ automatic |
| **ORM + migrations** | ❌ | ❌ | ✅ `moso-orm` |
| **Auth (sessions, OAuth, passkeys)** | ⚠️ OAuth2 utilities only | ❌ | ✅ `moso-auth` |
| **Authorization / RBAC** | ❌ | ❌ | ✅ `moso-authz` (the biggest unclaimed gap) |
| **Background jobs** | ❌ Celery bolted on | ⚠️ apalis, unwired | ✅ `#[job]`, transactional enqueue |
| **Admin panel** | ❌ | ❌ | ✅ `moso-admin` |
| **Project structure / generators** | ❌ user's problem | ❌ | ✅ `moso new`, `moso generate` |
| Runtime performance | ~1x | ~30–50x | ~30–50x |
| Memory per instance | high | low | low |
| Compile/iteration loop | instant | poor | **the hard problem - see `04-devex/42`** |

The two rows we can lose on are **iteration loop** and **ecosystem breadth**. Everything else we
should win outright. Doc `42-compile-times.md` is therefore not a nice-to-have chapter; it is a
core feature spec with numeric budgets.

## The three loops we optimise

Every design decision in these documents is justified by one of three loops. If a proposed feature
does not shorten one of them, it does not ship in M1.

1. **The first-five-minutes loop.** `install → new → dev → see docs at /docs → first endpoint`.
   Target: under 5 minutes on a cold machine, under 60 seconds warm.
2. **The edit loop.** `save file → compiler says something useful OR server is live again`.
   Target: p50 under 3 seconds for a handler-body change; every compile error the framework can
   cause has a hand-written message.
3. **The scale loop.** `first endpoint → 200 endpoints, 8 engineers, no rewrite`. Conventions,
   module boundaries, workspace splitting, and stability guarantees serve this loop.

## Positioning statement

> Moso is to Axum what FastAPI is to Starlette - except that Moso also ships the ORM, the auth, the
> jobs, and the admin that FastAPI made you assemble yourself.

We do **not** position against Axum. Axum is the engine. Moso apps can mount Axum routers, use
`tower-http` middleware, and drop to `axum::Router` at any point. Every "escape hatch" is a
documented, tested, first-class API - see `02-architecture.md § Escape hatches`.

## The three existential risks (and our stance)

### Risk 1 - the ORM
Every prior Rust ORM attempt has either stalled (Prisma Client Rust, archived), stayed niche
(SeaORM's verbosity, Diesel's dynamic-query pain), or is unfinished (Toasty, Cot's ORM). Prisma
itself *retreated from Rust* for its query engine.

**Stance:** we do not write a query engine. We build on `sqlx` for execution and a **sealed
facade** over query construction (initially `sea-query`) so the internals can be replaced without
breaking users. We spend the entire differentiation budget on ergonomics: shape-stable builders,
N+1-safe relations, generated-but-reviewable migrations, and error messages.
Kill criterion in `05-delivery/50-roadmap.md`.

### Risk 2 - error messages
Rust frameworks that encode invariants in the type system (Axum, Diesel, Bevy) produce
trait-resolution errors that are the single most cited reason people bounce.

**Stance:** treat diagnostics as a product surface with an owner and a test suite. Every framework
trait gets `#[diagnostic::on_unimplemented]`. Every handler gets assertion codegen by default (not
opt-in like `#[axum::debug_handler]`). `moso check` decodes the top failure shapes. We maintain a
`trybuild` corpus of *wrong* programs with snapshotted expected error output (the `moso-ui-tests`
crate) - regressions in
error quality break CI. See `04-devex/41-diagnostics.md`.

### Risk 3 - sustainability
The ecosystem's two cautionary tales - Actix's 2020 maintainer collapse and Rocket's five-year
stagnation - were both single-maintainer failures. The frameworks we want to emulate (Laravel,
FastAPI, NestJS) all pair OSS with a funding model.

**Stance:** minimum three maintainers with commit rights before the 0.1 announcement; a written
governance doc and RFC process from day one; a decided (even if not yet executed) commercial model
before launch. See `05-delivery/52-governance.md`.

## What success looks like

| Horizon | Signal |
| --- | --- |
| M1 + 3 months | 50 GitHub stars/week sustained; 10 non-founder contributors; 3 public production deployments |
| M1 + 12 months | Top-3 result for "rust web framework" discussions; ≥1 conference talk not given by us; a book or course by a third party |
| M2 + 12 months | Named in the Rust survey's ecosystem section; a company hires "Moso experience" in a job post |

The anti-signal we watch for: high star count with low `moso new` telemetry (opt-in) and no
production deployments - that means we built a demo, not a framework.
