# Code organization

Keep the codebase clean by giving each thing **one home** and reusing it — never
restating a fact that already lives somewhere. Read this before adding a file,
and search for an existing home before creating a new one.

`docs/` is the normative specification and it describes more than exists.
**Before asserting a feature is there, check the workspace.**
[`docs/06-reference/63-implementation-status.md`](docs/06-reference/63-implementation-status.md)
is the honest ledger and is written to be pessimistic. Ignore `/loco` entirely —
it is a vendored copy of a different framework kept only for comparison.

## The shape

Every request is **middleware stack → route match → handler adapter → extractors
→ handler → `Result<T, Error>` → `IntoResponse`**. Extraction is where
validation happens, so a handler receives values that already satisfy their
constraints. Handlers hold logic; errors are values, not events.

Dependencies point **downward only**, and `xtask check-deps` rules 1–5 fail a
build that adds an upward edge:

```text
moso (facade)
 └─ batteries   moso-authz, moso-jobs        (moso-kv arrives transitively)
     └─ data    moso-orm ─→ moso-sql          (moso-sql has no Moso deps at all)
         └─ moso-core
             └─ moso-openapi ─→ moso-schema   (moso-schema has no Moso deps at all)
                 └─ substrate   axum · tower · hyper · tokio · serde · sqlx
```

`moso-schema` and `moso-sql` are the two floors: neither depends on any Moso
crate, and `moso-schema` additionally depends on no `http`/`axum`/`sqlx`, because
"usable standalone" is a public promise. `moso-core` depends on `moso-schema`
**and** `moso-openapi` unconditionally — the `openapi` feature controls only
whether `/docs` is mounted, because six public trait signatures name
`OperationBuilder` and a trait that changes shape with a cargo feature is a trap.

Battery-to-battery edges are not free: each is declared one by one, with its
reason, in `xtask/allow/dep-edges.toml`, and an undeclared edge fails the gate.
Adding one decides what a user who wants jobs but not an ORM has to compile.
`moso-auth`, `moso-mail`, `moso-storage` and `moso-migrate` are workspace crates
that are **not yet reachable through any facade feature**.

## One home per concept (the anti-redundancy rule)

A fact is defined once and imported everywhere else — never copied, never
restated. **Derive instead of duplicating.**

| Concept | Its one home |
| --- | --- |
| `App`, `Router`, `Handler`, `Extract`, `Error`, DI, config, middleware | `moso-core` |
| The JSON Schema model (`SchemaNode`, `SchemaGenerator`, …) | `moso-schema::json_schema` — **not** `moso-openapi` |
| The OpenAPI 3.1 document, its builders, the embedded doc UI | `moso-openapi` |
| Every procedural macro | `moso-macros` (entity/query macros in `moso-orm-macros`) |
| Every path a macro expansion may name | `crates/moso/src/private.rs` |
| SQL construction | `moso-sql` — sealed, no foreign type in a public signature |
| A third-party version, declared exactly once | `[workspace.dependencies]`, with its rationale as a comment above it |
| What is actually built | `docs/06-reference/63-implementation-status.md` |
| A hard, contested or expensive-to-reverse decision | `docs/adr/NNNN-short-title.md` |
| Gate definitions | `docs/05-delivery/53-quality-gates.md`; the commands are `.cargo/config.toml` aliases and `xtask` |
| Diagnostic snapshots (one per user-facing compile error) | `crates/moso-ui-tests/tests/ui/` |
| A gate exemption, with a written reason | `xtask/allow/*.toml` |
| Whether a route, response or parameter is documented | the handler's own signature — never a second annotation |

Deriving, not duplicating, in practice: the OpenAPI operation comes from the
handler signature, the 422 schema from `#[derive(Schema)]`, the `.env.example`
from the `Config` type, the changelog from commit subjects. If you write the
same rule in two places, one of them is a bug waiting to happen — collapse it.

## Where new code goes

| You're adding | Put it in |
| --- | --- |
| A runtime primitive every app has | `crates/moso-core/src/<area>/` |
| An extractor, response type, or middleware slot | `moso-core/src/{extract,response,middleware}/`, with a `describe()` and its own test |
| A validation attribute or constrained type | `moso-schema/src/schema.rs` or `moso-schema/src/types/` — plus the JSON Schema keyword, in the same change |
| A proc macro or derive | `moso-macros/src/`, and its exact expansion in `docs/06-reference/62-macro-reference.md` |
| A path a new expansion needs | `crates/moso/src/private.rs`, called out in the PR |
| Anything touching a database | `moso-sql` (construction) or `moso-orm` (execution) — never a raw SQL string above them |
| A new battery | its own crate, behind a facade feature that is **off by default** |
| A unit test | inline `#[cfg(test)] mod tests` at the bottom of the file it tests |
| An integration test | the crate's `tests/`, one binary with `mod` submodules — each file is a separate link |
| A new user-facing compile error | a `trybuild` case in `crates/moso-ui-tests/tests/ui/`, in the same PR |

## Stay clean

- **Reuse before you write.** Find the existing module and extend it; do not
  fork a parallel path. A second way to do something is a maintenance tax and a
  second thing to document.
- **No `todo!()`, no `unimplemented!()`, no stub returning a plausible-looking
  wrong value.** A function that cannot be finished is left out, and its absence
  is recorded in the status ledger. A silent wrong answer costs more than a
  missing feature, because nobody goes looking for it. This is why unbuilt CLI
  subcommands are absent from the command tree rather than printing "coming
  soon". **There is no longer an exception.** `moso-auth` used to be one — its
  public surface was frozen ahead of its implementation behind 38 `todo!()`
  bodies in `routes.rs`, `error.rs`, `throttle.rs` and `config.rs`, guarded by a
  `tests/frozen_surface.rs` that proved the signatures composed without running
  a body. All of them are implemented; that file is now `tests/public_surface.rs`
  and every case in it runs. `grep -rn 'todo!' crates/*/src` returning a hit is a
  defect, not a recorded skeleton.
- **Explanation goes in doc comments, not `//`.** The tree runs 27 % doc-comment
  lines against a 14:1 ratio over plain comments. Every `.rs` file under
  `crates/*/src` opens with a `//!` module doc — often several paragraphs with
  headed sections, tables and a `text` diagram arguing why the module is shaped
  as it is. A bare file is instantly foreign. Reserve `//` for implementation
  notes a user should not see, and write them as full sentences.
- **Small and composable.** One well-named function that does one thing beats a
  branchy monolith. Keep code at ≤ 100 columns; let doc-comment prose and tables
  run over when breaking them would be worse.

# Invariants

## The request path

- At most **one body extractor** per handler (`Json`, `Form`, `Bytes`, `Text`,
  `Multipart`, `OpaqueBody`), and it MUST be the last parameter. Parts
  extractors run left-to-right before it. Violating it is a hand-written compile
  error, not a trait-bound dump.
- **Validation happens inside extraction, never after.** There must be no path
  that hands a handler an unvalidated `T`. A `.validate()?` a handler can forget
  is a validation gap.
- `Extract::describe` / `ExtractBody::describe` have **no default body**. Every
  extractor writes one and declares every parameter, security scheme and status
  it can produce. A silent extractor makes the document lie.
- Read the body with a hard cap **before** deserialising
  (`extract::read_limited(req, ctx.limits().body_max)`), and read every limit
  from `ctx.limits()` — never global or static state. Deserialising first means
  "allocate 4 GB, then fail".
- Exceeding the body limit produces a documented 413, and an overrunning request
  a 504 — never a panic, never a silent truncation. `uri_max`,
  `header_max_count` and `header_max_bytes` are enforced by `Slot::RequestLimits`
  (inside `catch_error`, outside `timeout`), which reads the same `Limits`
  snapshot the extractors read and answers 414 / 431 / 431 with the operator's
  own number in the document. **There is still no 408**, and deliberately so: an
  honest 408 needs a read deadline on the request *body* distinct from the
  whole-request timeout, `HttpConfig` has no key for one, and the `timeout` slot
  already ends an unfinished request with a 504 — a 408 would be a second
  spelling of the same condition.
- Every field error carries an RFC 6901 JSON Pointer rooted at its source: body
  pointers are bare (`/tags/2`), query uses `/query`, path `/path`, header
  `/header`. Report **all** failing fields in one response, not just the first.
- `FieldError::code` is a **documented set** — `required`, `type`, `len`,
  `range`, `pattern`, `format`, `enum`, `unique`, `multiple_of`,
  `custom:<name>` (`codes::ALL` in `moso-schema/src/validate.rs`; `ErrorCode` is
  `#[non_exhaustive]`, so adding one is a minor version and changing one is
  breaking). Clients branch on `code`, never on `message` — `message` is
  localisable and therefore unstable.
- `RequestCtx` is created once by the handler adapter and read from the request
  extensions. Constructing a second one forks the per-request dependency cache,
  so `Depends<T>` memoisation silently resolves twice.

## Types and the document

- **`Router` is not generic over state.** Never introduce `Router<S>`,
  `with_state`, or `FromRef`. App-lifetime values reach handlers only through the
  provider map via `Inject<T>`. This one decision deletes the largest family of
  Axum trait errors and the largest source of monomorphisation.
- A handler's declared response type **is** the documented type. `Raw<T>` is the
  escape hatch *inside* a documented operation, and documents itself as
  `unknown`. Whole operations can also opt out, visibly: a plain `async fn`
  registered without `#[endpoint]` goes through `UndocumentedEndpoint` and is
  marked `x-moso-undocumented`, and anything under `Router::mount_axum` is absent
  from the document entirely. Never synthesise plausible-looking metadata for
  either — a document that is confidently wrong is worse than one that admits a
  gap.
- Route paths are `&'static str` with `{param}` / `{*rest}`. `:param` and `*rest`
  are rejected by `validate_path`, a `const fn`: wrapped in `route_path!` — which
  `routes!`, `ep!` and `#[endpoint]` emit — a routing mistake is a **compile**
  error; a hand-written `Router::get("/users/:id", ..)` runs the same check at
  registration and fails at **boot**. Path parameter names must match the
  `Path<T>` fields — a mismatch is a boot error naming both sides, never a
  runtime 500.
- **Entities are not schemas.** `#[derive(Entity)]` must never implement
  `Schema`, so an entity cannot be returned from a handler. An entity has a
  `password_hash`; the split makes leaking it a compile error rather than a
  review catch. Use `#[schema(from = Entity)]` for the `From` impl.
- Query builders are **shape-stable**: `Select<E>` stays `Select<E>` after any
  number of `.filter()` / `.order_by()` / `.join()` calls.
- Relations never lazy-load. An unloaded relation returns `Err(NotLoaded)`;
  eager loading is explicit and batched. Implicit lazy loading *is* the N+1
  mechanism.
- Never add async or IO-backed validation to `Validate` ("this email is
  unique"). Check-then-act across a network is a race; enforce it in the service
  inside the transaction and surface `Conflict` → 409 with a field pointer.
- Each `#[schema(..)]` attribute generates **both** the runtime check and the
  matching JSON Schema keyword from one declaration. An attribute that produces
  only one half reintroduces the drift the framework exists to delete.

## Boot and lifecycle

- `Inject<T>` is **infallible at the use site** — no `?`, no `expect`. Boot
  proved the provider exists. Making it fallible deletes the guarantee.
- `Depends<T>` is per-request, memoised by `TypeId`, and fallible. Never use it
  for an app-lifetime value; never register a request-scoped concern as a
  provider.
- A hand-written `impl Dependency` MUST declare `PROVIDER_REQ` covering
  everything it reaches transitively. An empty one silently drops the route out
  of the boot check, turning a boot error into a production 500.
- `AppBuilder::build()` reports **every** boot problem in one pass — no
  fail-fast — each with a source location and a concrete `fix` line. The
  provider map is frozen after boot; a `provide_with` cycle is a boot error
  naming the full path.
- `/healthz` never touches the database. `/readyz` returns 503 **immediately** on
  the shutdown signal, before draining begins — that is what lets the load
  balancer remove the instance while it can still serve.
- The default shutdown grace is 25 s and must stay **below** the common 30 s
  orchestrator kill timeout.
- `Metrics` stays the innermost middleware slot: it runs after routing so its
  `route` label is the *pattern*. Moving it outward causes unbounded metric
  cardinality.

## Framework rules that are not negotiable without an ADR

- **`#![forbid(unsafe_code)]` and `#![deny(missing_docs)]`** in every crate,
  restated in each crate root *and* inherited via `[lints] workspace = true`.
  A new crate that forgets the manifest table silently loses all ten lints.
- **No link-time registries.** `inventory` and `ctor` are banned. Everything is
  registered by a statement you can read. The accepted price is that `moso
  routes`, `moso openapi` and `moso db` work by *running* the application binary.
- **No global mutable *application* state**, no hidden singletons. `App` is a
  value and must be constructible twice in one process. The deliberate
  exceptions are process-wide and documented as such: `BlockingPool::global()`
  (one pool per process, also registered as a provider) and the counters behind
  `metrics::requests_total` / `in_flight` and the panic/error counters.
- **No `async_trait` in a Moso trait** — RPITIT for generic traits, a
  hand-written `BoxFuture` for dyn-compatible ones. `RequestCtx::depends` is the
  documented exception (recursive resolution would be an infinitely-sized type);
  `Extract::extract` must stay RPITIT and must not pay for a boxed future.
- **Never add `anyhow`.** It is absent from the whole workspace, the CLI
  included. Handlers return `moso::Result<T>` over the single concrete
  `Error`; battery crates define their own `Error` in `src/error.rs` with
  `thiserror` plus a crate-local `Result` alias. An error is a value — log it
  exactly once, at the boundary, never at the construction site.
- **Macro output names `::moso::__private::X` and nothing else.** Never
  `::moso_core::X`, never a substrate crate directly. Anything an expansion needs
  goes into `crates/moso/src/private.rs` and is called out in the PR.
- **`moso-macros` depends on no runtime Moso crate**, and `moso-schema` depends
  on no `http`/`axum`/`sqlx` — it is usable standalone, and that is a public
  promise.
- **`moso-sql` and `moso-orm` are sealed**: no `sea-query` or other foreign type
  in any public signature. `sqlx` is deliberately *not* sealed —
  `Db::postgres_pool()` / `Db::sqlite_pool()` are the escape hatches, and every
  escaping path is enumerated in `xtask/allow/sealed.toml`.
- **The facade has no default feature that pulls a database driver.** `orm`,
  `authz` and `jobs` are off by default. The `test` feature belongs in
  `[dev-dependencies]` only — a production-reachable dependency override is an
  authentication-bypass primitive.
- **Every Moso abstraction exposes the layer beneath it.** `Router::into_axum`,
  `Router::mount_axum`, `Router::layer`, `Opaque<T>` / `MosoExt<T>`,
  `Db::postgres_pool()`. An abstraction you cannot escape is a cage, and Rust
  developers correctly refuse cages. Shipping one without a hatch is a design
  defect, not a missing nice-to-have.
- **Axum interop is by wrapper in both directions, never a blanket impl** —
  `impl<T: Extract> FromRequestParts<()> for T` is an orphan-rule violation
  (E0210). Neither adapter contributes to OpenAPI, which is the honest behaviour.
- **Every public trait carries `#[diagnostic::on_unimplemented]`; every blanket
  impl carries `#[diagnostic::do_not_recommend]`.** Currently 100 % of public
  traits with one written exemption. A `[[known_gap]]` entry does *not* silence
  the gate, and `--tolerate-known-gaps` must never appear in CI.
- **No magic that cannot be printed.** Every expansion is inspectable with
  `cargo expand` and documented in `62-macro-reference.md`. No runtime
  reflection, no separate codegen step.
- **No stringly-typed APIs where a type will do** — permissions, job names,
  config keys, column names and route names are all typed.
- **Stable Rust only, Tokio only, Postgres first, OpenAPI 3.1.** Reopening any
  of these, or any documented non-goal, requires an ADR.

## Security defaults that must not be weakened

- `expose_internal_errors` off in every profile, with a boot warning if forced
  on. `trusted_proxies` empty by default. CORS off, and never `*` with
  credentials. `/docs` and `/openapi.json` disabled in the production profile
  (there is no admin panel to disable — `moso-admin` does not exist).
- `SecretString` / `SecretBytes` keep `zeroize` and a redacting `Debug`.
  Redaction is **structural**, not a regex — a regex redactor fails open on any
  format it did not anticipate. Request bodies are never logged by default.
- No secret, cookie or `#[schema(secret)]` value may appear anywhere in test
  output. CI exports a canary secret and greps the whole run for it.
- Cookies stay HttpOnly + Secure (prod) + SameSite=Lax + signed.
- **Never implement a cryptographic primitive.** Use `ring` or RustCrypto.
  Security-relevant randomness comes only from the OS CSPRNG, never
  `rand::thread_rng`. Signed cookies and cursors use HMAC-SHA256 with HKDF
  domain separation, so one signature cannot be replayed in another context.
- Never tell a caller whether an identifier exists when the answer is itself the
  secret — a draft invisible to its viewer is a 404, not a 403, because a 403
  confirms the row exists.
- Changing any of these is an RFC-required change.

## In a generated Moso application

- `src/main.rs` is a shim that never grows; `src/lib.rs` is the composition root
  returning `Result<App>`. Everything real lives in the library, so `tests/`
  boots the identical application the binary boots.
- No autoloading, no directory scanning, no convention-based discovery.
  Everything is a `mod` plus an explicit `.mount()` in a ~20-line function.
- Layering: `routes/` may import models, services and `moso::*` but never raw
  SQL; `services/` may import models and `moso::db` but never `moso::extract` or
  `http`; `models/` may import `moso::db` and `moso::schema` but never services
  or routes; `jobs/` may import services and models but never routes.
- Do not create a `services/` file until a handler needs two things
  transactionally, or two handlers need the same logic. Premature service layers
  are the documented Rails-refugee failure mode.
- Anything CPU-bound goes through `moso::task::blocking()`. Blocking the runtime
  is the target user's most common footgun.
- `examples/` deliberately does **not** inherit `[workspace.lints]` — sample apps
  must look exactly like code a user would write. Keep the doc comments: the
  templates use them to teach that a doc comment's first line becomes the
  OpenAPI summary.

# Naming

- Branches: `<type>/<kebab-summary>`, e.g. `feat/orm-preload`.
- Commits and PR titles: conventional-ish `type(scope): imperative summary`,
  lowercase, where **scope is the crate without its `moso-` prefix** —
  `fix(core):`, `feat(orm):`, `docs:`, `ci:`, `refactor(schema):`. The changelog
  is assembled from these.
- Rust files are snake_case; types are PascalCase; every crate root restates
  `#![forbid(unsafe_code)]`, `#![deny(missing_docs)]`, then a `#![doc = "…"]`
  one-sentence summary in that order.
- **Test functions are full lowercase English sentences with no `test_` prefix** —
  `a_tampered_cookie_is_indistinguishable_from_no_cookie`,
  `the_hash_floor_is_owasps_minimum`. The suite reads as a specification.
- Loop indices are spelled out (`index`, not `i`); bindings are nouns. No bare
  `get_` getters — every `get_` in the tree is a compound form (`get_or_init`,
  `get_many`).
- File sections use a full-width rule: `// ` plus 75 dashes, the title as
  `// Title`, then another 78-char rule. Sub-sections and headings inside `mod
  tests` use the lighter box-drawing form `// ── title ──…──`. The two are not
  interchangeable; the level tells a reader where they are.
- Imports are three hand-maintained blocks separated by blank lines: `core`/`std`,
  then third-party and other `moso_*` crates, then `crate`/`super`. rustfmt sorts
  *within* a block and will never create or reorder the blocks — `group_imports`
  is nightly-only and deliberately unset.
- In app code: handler fns are bare verbs (`list`, `create`, `show`, `update`,
  `destroy`); paths are plural kebab-case with `{}` params; entities are singular
  PascalCase over plural snake_case tables; input DTOs are `Create*`/`Update*`/
  `*Params`, output DTOs are `*Out`, jobs are `*Job`, permissions are
  `resource.action`, migrations are `YYYYMMDDTHHMMSS_verb_object`.

# Git & PRs

- Commit and push only when the user asks. Don't create commits as a side effect
  of finishing a change unless requested.
- Branch from `main`, keep branches short-lived, and **rebase rather than merge**.
  Land work through a pull request so CI gates it.
- **Contributions are inbound under the MIT licence** ([ADR-0018](docs/adr/0018-mit-relicence.md)) —
  there is no CLA and no DCO sign-off ceremony, so you keep your copyright.
  Release tags are still signed cryptographically (`-S`).
- **Write the commit subject carefully.** There is no `CHANGELOG.md`
  ([ADR-0018](docs/adr/0018-mit-relicence.md)); `release.yml` assembles the
  release notes from the commit log since the previous tag, so
  `type(scope): summary` *is* the changelog entry.
- **If the code diverges from a design document, update the document in the same
  pull request.** A document that quietly stops describing the code is worse than
  no document, because people plan around it. If the divergence was a *decision*
  rather than an oversight, write the ADR too.
- Small pull requests get reviewed. A 2,000-line diff waits for a reviewer with a
  free afternoon.
- Branch protection points at the single aggregate job named `CI`. A new gate
  must be added to that job's `needs:` list or it is advisory no matter how it is
  written.
- ADRs are **immutable**: a revisited decision gets a new numbered ADR that
  supersedes the old one, and the old one gains a header saying so. Superseded
  ADRs are never deleted — the trail is the value.
- An **RFC** is required *before* code for: any breaking change, any new public
  trait, any new crate, anything that changes the layout `moso new` generates,
  and anything affecting a security default. Not required for bug fixes, docs,
  tests, internal refactors or additive non-trait APIs.
- Never commit a secret or a `.env`; never force-push `main`; the lockfile is
  committed and CI installs frozen.
- Releases are cut only by pushing a signed `vX.Y.Z` tag and letting
  `release.yml` run — never by hand, never from a laptop.

# Workflow

- **Run the gates with the `.cargo/config.toml` aliases, not hand-typed
  equivalents.** Each alias is byte-for-byte what CI runs, so "it passed locally"
  means the same thing as "it passed in CI". Retyping loses `--all-features` and
  produces a green local run that fails CI.

  ```sh
  cargo fmt --all                                 # G1
  cargo lint                                      # G1 — clippy, all targets, all features, -D warnings
  cargo nextest run --workspace --all-features    # G2
  cargo test --workspace --all-features --doc     # G6 — nextest cannot run doctests
  cargo docs                                      # G6 — rustdoc, warnings are errors
  cargo ui                                        # G4 — the diagnostics corpus
  cargo deny check                                # G11
  cargo xt ci                                     # the nine structural gates, in CI order
  ```

- **`cargo xt ci` is not a superset of the aliases.** Its clippy and test gates
  run without `--all-features`. Run both.
- The `xtask` subcommands are exactly `bench-compile`, `expand-size`,
  `check-crates`, `check-sealed`, `check-deps`, `check-diagnostics`, `release`,
  `ci`. `check-crates` is G5: seven rules over every crate — the workspace lint
  declarations, the `[lints] workspace = true` opt-in, the crate-root
  restatement in the documented order, no `unsafe` anywhere, a `//!` on every
  file under `crates/*/src`, no banned direct dependency, and the registry
  metadata a publishable crate needs. The `hygiene` job's greps stay as a second
  line of defence that needs no tooling. Exemptions go in
  `xtask/allow/crates.toml` and each one needs a reason.
- **Database tests skip, never fail, when `DATABASE_URL` is unset.** This is a
  tested property, not a courtesy: the macOS CI leg runs the whole suite with it
  deliberately unset. A test that fails without a database is a broken test.

  ```sh
  ./scripts/test-db.sh up                         # Postgres 17 on 55433, Redis 7 on 56379
  eval "$(./scripts/test-db.sh env)"              # exports DATABASE_URL and REDIS_URL together
  ```

  Export both or neither. `env` prints them as one block for exactly that reason:
  exporting one of the two gives a green run in which the other suite skipped.

- **Never mock the data layer.** Tests run against real Postgres and real
  SQLite; a mocked data-layer test proves nothing about SQL.
- Test through HTTP with `moso_test::test_app!(my_crate::app())` inside
  `#[tokio::test]`, not by calling handler functions directly — constructing
  extractors by hand skips middleware, validation and serialisation, which is
  exactly where the bugs are.
- **Re-recording is not fixing.** `TRYBUILD=overwrite cargo ui` makes any failure
  go away, so a `.stderr` diff is reviewed like an API change: every case carries
  a `help:` line, no line wraps, and the median case stays under 25 lines. Run
  the corpus on stable Linux only — `trybuild` compares compiler output byte for
  byte, so MSRV diffs say nothing about Moso.
- **Write diagnostics in the house shape**: plain-language statement of what is
  wrong; the span on the *user's* file, never a framework file; `note:` giving
  the one-sentence reason the rule exists; `help:` giving the fix as pasteable
  code. Name the user's type (`CreateUser`), never ours
  (`<T as Extract>::Output`). Never print a type over 80 characters. One error,
  not a cascade — emit a well-typed placeholder to suppress downstream noise. At
  most one URL. No blame, no jokes: people read these while frustrated.
- **Do not add messages ending "run `moso check`"** — that command does not
  exist, and several shipped messages already point at it. Advice that goes
  nowhere is worse than no advice.
- Proc-macro code never panics and never returns nothing: on error, re-emit the
  user's function unchanged, then one `compile_error!` per *distinct* mistake,
  then a well-typed placeholder so a downstream `routes!` does not produce a
  second derived error.
- **Adding a dependency:** justify it in the PR in one sentence, add with
  `cargo add -p <crate> <dep>`, hoist the version into `[workspace.dependencies]`
  with a rationale comment above it, check the licence, and run
  `cargo deny check`. Moso is `MIT` and permissive licences are compatible with
  it, so inbound permissive is fine and a copyleft dependency is still refused.
  Refused outright: any runtime but Tokio,
  `inventory`/`ctor`, OpenSSL in place of rustls, a crate implementing its own
  cryptographic primitives.
- **Never add a linker flag to `.cargo/config.toml`.** Cargo config has no
  conditionals, so `-fuse-ld=mold` naming a linker that is not installed is a
  hard error on a stranger's machine. Faster-linker recipes go in the
  contributor's own `~/.cargo/config.toml`; the repo file documents them in
  comments only.
- Attribute public API the way the tree does: `#[must_use]` on value-returning
  methods, `#[non_exhaustive]` on new public enums and literal-constructed
  structs, an `# Errors` rustdoc section on every fallible public fn, `# Panics`
  where it can panic. Do not sprinkle `#[inline]` — there are six in the whole
  workspace.
- Rustdoc examples must actually run; `ignore`/`no_run` needs a comment saying
  why. Eight rustdoc lints are `deny`, listed individually rather than as
  `rustdoc::all` so a new upstream lint cannot break the build without someone
  choosing it.
- **Two gates are red on this tree and were deliberately not closed by lowering
  the number**: `xtask check-deps` rule 6 (155 third-party crates against a
  budget of 90) and `xtask expand-size` (`#[endpoint]` expands to 152–179 lines
  against 60). Do not "fix" them by raising the budget, and do not make them
  worse.
- **Done means** the gate commands above pass; every new public item has a doc
  comment and a runnable example; every new public trait has `on_unimplemented`;
  any new user-facing compile error has its `.stderr` case in the same PR;
  the commit subject is a usable release-note line; and the design document is
  updated if reality diverged from it.
