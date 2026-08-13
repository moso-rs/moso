# 46 - Documentation Strategy

> 🟡 **Status: the rustdoc half is real, the rest is not.** `#![deny(missing_docs)]` is enforced in
> every crate and the doc comments are written to the standard this document sets - every public
> item explains *why*, not just *what*. ⛔ Not built: the website, the tutorial, the doctest harness
> over the tutorial, the LLM-facing corpus, and the docs CI.

> Tutorial-quality documentation is repeatedly cited as a primary reason FastAPI won adoption, and
> "docs are thin" is a standing complaint about Axum. Documentation is a **shipped artefact with a
> release gate**, not a follow-up task.

## The rule

**No feature ships without its documentation.** A PR adding a public API that lacks docs, an
example, and a place in the guide does not merge. This is enforced by `#![deny(missing_docs)]` plus
a review checklist.

## The four documentation modes (Diátaxis)

| Mode | Question | Where | Owner |
| --- | --- | --- | --- |
| **Tutorial** | "Teach me" | `moso.rs/learn` | one named maintainer |
| **How-to** | "Help me do X" | `moso.rs/guides` | whoever built the feature |
| **Reference** | "What are the details" | docs.rs | the code |
| **Explanation** | "Why is it like this" | `moso.rs/design` + `docs/adr` | the architect |

The most common failure is writing only reference. The tutorial is the acquisition channel.

## The tutorial (the highest-leverage artefact)

One continuous, tested narrative that builds a real application - a small shop API - in twelve
chapters, each ending with a working, committed state.

1. Install, `moso new`, first request, `/docs`
2. Your first endpoint: types in, types out
3. Validation and error responses
4. The database: entity, migration, first query
5. Relations and avoiding N+1
6. Authentication: register, login, sessions
7. Authorization: permissions and policies
8. Background jobs and email
9. Testing what you have built
10. The admin panel
11. Configuration, secrets, and profiles
12. Deploying: Docker, migrations, health checks, observability

Requirements:
- **Every snippet is compiled and run in CI** (via `mdbook-test`/`trybuild`-style extraction from
  the tutorial repo). Documentation that does not compile is worse than no documentation, and the
  Rust ecosystem is full of it.
- Each chapter states what you will build, what it will cost you in time, and links to the diff.
- A companion repository with one commit (and one tag) per chapter, so a reader can join anywhere.
- Total reading time under 3 hours; each chapter under 15 minutes.

## The FastAPI-refugee track

A dedicated guide: **"Moso for FastAPI developers."** Side-by-side translations of the twenty things
a FastAPI developer does daily - Pydantic model → `#[derive(Schema)]`, `Depends()` → `Inject`/
`Depends`, `BackgroundTasks` → `#[job]`, `TestClient` → `TestApp`, `lifespan` → `on_startup`,
`response_model` → the return type, `HTTPException` → `Error`, SQLAlchemy session → `Db`/`RequestTx`.

Equivalent tracks for **Django/Rails** and for **Axum users** ("you already have an Axum app - here
is how to adopt Moso incrementally, one router at a time"). The Axum track matters
disproportionately: it converts the largest existing population, and incremental adoption is the
lowest-risk ask.

## Reference docs (docs.rs)

- `#![deny(missing_docs)]` in every crate.
- Every public item has a **runnable example**. `cargo test --doc` is a release gate.
- Every trait documents: what implements it, when you would implement it, and what the framework
  does with it.
- Every error type documents the conditions that produce it.
- Module-level docs open with a "you probably want…" paragraph pointing at the guide.
- `#[doc(hidden)]` is used only for `__private`; everything else is either public and documented or
  private.

## Cookbook

Short, complete, copy-pasteable recipes - the format people actually search for:
file upload to S3; Stripe webhooks with signature verification; cursor pagination; soft delete;
multi-tenancy; SSE notifications; rate limiting an endpoint; OAuth with Google; CSV export as a job;
full-text search; scheduled reports; feature flags; blue-green migrations; testing with a real
Redis; running behind a proxy; deploying to Fly/Railway/K8s.

Each recipe: the problem, the complete code, the caveats, and a link to the runnable example in
`examples/`.

## Error-code reference

Every diagnostic and every runtime error type gets a page at `moso.rs/e/<code>`, with the message,
what causes it, and the fix. The URL appears in the error itself only where the explanation is too
long for a `help:` line (`04-devex/41`).

## Documentation for AI-assisted development

This is a deliberate, differentiating investment, not an afterthought. A framework with strong
types, generated OpenAPI, and firm conventions is unusually well-suited to AI-assisted coding - the
types and the generated contract give an agent something checkable - but only if the model can find
accurate, current information.

We ship:
- **`llms.txt` and `llms-full.txt`** at the site root: a condensed, machine-readable index of the
  API surface and conventions.
- **A single-file `moso-reference.md`** (~50 KB) covering every macro, extractor, and common
  pattern, designed to fit in a context window.
- **`AGENTS.md` / `CLAUDE.md` in every generated project**, describing the layout, the conventions,
  the commands (`moso check`, `moso test`), and the layering rules - so an agent working in a Moso
  project has the same map a new team member gets.
- **An MCP server** (`moso mcp`) exposing the project's routes, entities, permissions, config
  schema, and `moso check` results as tools, so an assistant can query the live project rather than
  guess.
- Machine-readable outputs everywhere (`--json` on every CLI command).

The bet: correctness-checkable frameworks win in an AI-assisted world, because the cost of a wrong
guess is a compile error rather than a runtime bug - but only if the docs are legible to both
audiences.

## Migration and upgrade guides

Every breaking change ships with: what changed, why, the mechanical fix, and a `moso migrate`
codemod where possible. The axum 0.6→0.7 and 0.8 upgrades are our reference for how much churn costs
a community; our commitment is that no upgrade requires reading a diff of the framework.

## Website

- Static, fast, searchable (client-side index; no third-party search that breaks offline).
- Versioned docs with a visible version switcher; the version a reader lands on is stated.
- Every code block has a copy button and a "run in playground" link where applicable.
- A live playground (WASM-compiled subset, or a hosted sandbox) for the schema/validation/OpenAPI
  loop - seeing a `#[derive(Schema)]` become an OpenAPI schema in the browser is the single most
  persuasive demo we can build.
- Dark mode; readable on a phone; no CDN dependencies for the core reading experience.

## Contribution documentation

Contributor rules live in [`AGENTS.md`](../../AGENTS.md), not a separate `CONTRIBUTING.md` (removed
with the MIT relicence, [ADR-0018](../adr/0018-mit-relicence.md)). It covers: repository layout, how
to run the test suite (including the UI corpus),
the diagnostics style guide, the ADR process, the RFC process, and a labelled set of
`good-first-issue`s that are genuinely small. A framework with three maintainers and no contributor
onboarding is a framework with three maintainers forever.

## Measurement

| Signal | Target |
| --- | --- |
| Public items without docs | 0 (CI) |
| Doc examples that do not compile | 0 (CI) |
| Tutorial chapters not tested in CI | 0 |
| Time for a new user to first working endpoint (usability test, n=5) | < 15 min |
| Search queries on the site with no result | reviewed monthly, top 10 become docs |
| Issues closed with "this is documented at…" | tracked; a high rate means the docs are unfindable, not that users are lazy |

## Acceptance criteria (WP-29)

1. All twelve tutorial chapters exist, compile, and run in CI against the current release.
2. The FastAPI, Django/Rails, and Axum migration guides exist with side-by-side examples.
3. `#![deny(missing_docs)]` and `cargo test --doc` are release gates.
4. `llms.txt`, `moso-reference.md`, `AGENTS.md` template, and `moso mcp` ship with 0.1.
5. The playground renders a `#[derive(Schema)]` → OpenAPI transformation live.
6. A usability test with five developers unfamiliar with Moso reaches a working endpoint in
   under 15 minutes; findings become issues.
