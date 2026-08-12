# 04 — Application Project Structure & Conventions

> 🟡 **Status: the composition-root shape is implemented; the batteries are not.** `moso new`
> generates `Cargo.toml`, `.env.example`, `README.md`, `src/lib.rs` (the `app()` composition root),
> `src/main.rs`, `src/routes.rs` and `src/dump.rs`. Directories in the layout below that hold
> entities, jobs, migrations, policies or admin registrations are ⛔ not generated, because the
> crates behind them do not exist. `moso generate workspace` is 🟡 implemented as the mechanical
> half: it moves the package into `crates/<name>/`, makes the root a workspace and lifts the
> profiles, but it does not decide which of your types belong in which crate — see the section
> below.

> FastAPI's single biggest structural weakness is that it leaves layout to the user, so every
> production guide invents its own and `main.py` becomes a monolith. Moso ships an opinion.

## The default layout (`moso new shop`)

```
shop/
├── Cargo.toml
├── .cargo/config.toml           # fast dev linker/codegen (see 04-devex/42)
├── .env                         # local-only, gitignored
├── .env.example                 # committed, documents every variable
├── moso.toml                    # framework config: profiles, features, generators
├── Dockerfile                   # multi-stage, cargo-chef cached
├── compose.yaml                 # postgres + redis for local dev
├── openapi.json                 # committed, drift-tested (see 01-http/14)
├── migrations/
│   ├── .schema.json             # schema snapshot — the diff source of truth
│   └── 20260729T101500_create_users.sql
├── src/
│   ├── main.rs                  # ~8 lines. A shim. Never grows.
│   ├── lib.rs                   # `pub fn app() -> App` — the testable entry point
│   ├── config.rs                # #[derive(Config)] AppConfig
│   ├── error.rs                 # app-specific error variants
│   ├── routes/
│   │   ├── mod.rs               # `pub fn router() -> Router` — composes sub-routers
│   │   ├── health.rs
│   │   └── users.rs             # handlers for one resource
│   ├── models/
│   │   ├── mod.rs
│   │   └── user.rs              # #[derive(Entity)] User  +  its Schema DTOs
│   ├── services/
│   │   └── mod.rs               # business logic; no HTTP types, no SQL strings
│   ├── jobs/
│   │   └── mod.rs
│   └── tasks/                   # one-off CLI tasks (`moso task seed`)
│       └── mod.rs
└── tests/
    ├── common/mod.rs            # TestApp helpers
    └── users.rs                 # integration tests, real DB
```

### `main.rs` never grows

```rust
// spec — src/main.rs, generated, and this is the whole file
#[tokio::main]
async fn main() -> moso::Result<()> {
    shop::app().await?.serve().await
}
```

Rationale (this is the Zero-to-Production pattern, made mandatory): the binary is a shim over a
library, so integration tests construct the *real* application rather than a parallel test-only
copy. It also means the binary crate recompiles trivially.

### `lib.rs` is the composition root

```rust
// spec — src/lib.rs, generated
pub mod config;
pub mod error;
pub mod jobs;
pub mod models;
pub mod routes;
pub mod services;

use moso::prelude::*;

pub async fn app() -> Result<App> {
    let cfg = config::AppConfig::load()?;          // layered config, 18-configuration.md
    let db  = moso::db::connect(&cfg.database).await?;
    let kv  = moso::kv::connect(&cfg.kv).await?;

    Ok(App::new(cfg.clone())
        .provide(db)
        .provide(kv)
        .provide(moso::mail::from_config(&cfg.mail)?)
        .mount(routes::router())
        .mount_jobs(jobs::registry())
        .with_admin(moso::admin::default())
        .build()?)                                  // <- boot-time DI validation happens here
}
```

Everything an application *is* is visible in one 20-line function. There is no hidden autoloading,
no directory scanning, no link-time registry. This is a deliberate contrast with Rails/Django and a
concession to Rust's culture: **explicit beats implicit when the explicit version is 20 lines.**

## The resource module pattern

One file per resource in `routes/`, one in `models/`. A resource file has exactly four sections in
this order, and the generator emits the headers:

```rust
// example — src/routes/users.rs

use moso::prelude::*;
use crate::models::user::{User, CreateUser, UpdateUser, UserOut};

// ── Router ───────────────────────────────────────────────────────────────
pub fn router() -> Router {
    Router::new()
        .get("/users", list)
        .post("/users", create)
        .get("/users/{id}", show)
        .patch("/users/{id}", update)
        .delete("/users/{id}", destroy)
        .tag("users")                     // OpenAPI tag applied to all of the above
}

// ── Handlers ─────────────────────────────────────────────────────────────
/// List users.
///
/// Supports cursor pagination and full-text search on name and email.
#[endpoint]
async fn list(
    Inject(db): Inject<Db>,
    Query(q): Query<ListUsers>,
) -> Result<Page<UserOut>> {
    let page = User::query()
        .filter_opt(q.search.as_ref().map(|s| User::NAME.ilike(format!("%{s}%"))))
        .order_by(User::CREATED_AT.desc())
        .paginate(q.cursor, q.limit)
        .fetch(&db)
        .await?;
    Ok(page.map_into())
}
/* ... */

// ── Params / query types ─────────────────────────────────────────────────
#[derive(Schema)]
struct ListUsers {
    #[schema(max_len = 100)]
    search: Option<String>,
    cursor: Option<Cursor>,
    #[schema(min = 1, max = 100, default = 20)]
    limit: u32,
}

// ── Tests ────────────────────────────────────────────────────────────────
#[cfg(test)]
mod tests { /* unit tests for pure helpers only; behaviour tests live in tests/ */ }
```

### Where does business logic go?

| Layer | May import | May NOT import | Rule of thumb |
| --- | --- | --- | --- |
| `routes/` | `models`, `services`, `moso::*` | raw SQL | HTTP in, HTTP out. No branching on business rules beyond guard clauses. |
| `services/` | `models`, `moso::db`, `moso::jobs` | `moso::extract`, `http` | Owns transactions and invariants. Returns domain errors. |
| `models/` | `moso::db`, `moso::schema` | `services`, `routes` | Data shape, validation, queries scoped to one entity. |
| `jobs/` | `services`, `models` | `routes` | Same body as a service call, different trigger. |

`moso check` enforces these edges with a lint (`moso::layering`) and can be configured or disabled
in `moso.toml`. Enforcement is **on by default in generated projects, off for existing projects**
that add Moso incrementally.

**Guidance we put in the docs:** do not create a `services/` file until a handler needs to do two
things transactionally, or two handlers need the same logic. Premature service layers are the
Rails-refugee failure mode. The generator does *not* create a service per resource.

## The DTO convention (this is the FastAPI parity point)

Three kinds of type, clearly named, and the generator scaffolds all three:

| Kind | Name pattern | Derives | Purpose |
| --- | --- | --- | --- |
| Entity | `User` | `Entity` | Maps to a table. Never leaves the data layer. |
| Input DTO | `CreateUser`, `UpdateUser` | `Schema` | Request bodies. Carries validation constraints. |
| Output DTO | `UserOut` | `Schema` | Response bodies. Controls what is exposed. |

**Why not return the entity directly?** Because `User` has a `password_hash`. Moso makes the safe
path the easy path: `Entity` does **not** implement `Schema`, so you *cannot* accidentally return an
entity from a handler. The compiler stops you, with this message:

```
error[E0277]: `User` cannot be used as a response body
  --> src/routes/users.rs:31:6
   |
31 | ) -> Result<User> {
   |      ^^^^^^^^^^^^ `User` is an entity, not a response schema
   |
   = note: entities may contain fields that must not be exposed (e.g. password hashes)
   = help: define an output DTO and convert:
             #[derive(Schema)]
             #[schema(from = User)]
             pub struct UserOut { pub id: Uuid, pub email: Email, pub name: String }
   = help: then `Ok(user.into())`
   = help: to opt out for a genuinely public entity, add `#[entity(expose)]` to `User`
```

`#[schema(from = User)]` generates the `From<User> for UserOut` impl by matching field names, and
errors at compile time if a field is missing or mistyped. This removes the boilerplate objection to
DTOs, which is the reason people skip them and leak data.

## Growing up: the workspace layout

At roughly 20k lines / 100 endpoints / 4+ engineers, a single crate starts to hurt incremental
builds. `moso generate workspace` performs the split mechanically:

```
shop/
├── Cargo.toml                # workspace
├── crates/
│   ├── shop-domain/          # entities, DTOs, pure logic. No I/O. Compiles in ~3s.
│   ├── shop-db/              # queries, migrations, repositories
│   ├── shop-web/             # routes, extractors, OpenAPI
│   ├── shop-jobs/
│   └── shop-app/             # composition root; `pub fn app()`
└── src/main.rs               # still 8 lines
```

Why this order: `shop-web` is the crate that changes most often and depends on the most stable
crates, so a route edit recompiles only `shop-web` + `shop-app` + the binary. Measured guidance and
a decision table are in `04-devex/42-compile-times.md`.

**`moso generate workspace` MUST be mechanical and reversible** — it moves files and splits
`Cargo.toml`, and the result must build without hand-editing. This is a differentiating feature:
every other framework tells you to "consider splitting into crates" and leaves you to it.

**As built**, it does the half that is mechanical, and one decision keeps it that way: *the package
keeps its name*. It moves to `crates/shop`, not to `crates/shop-app`. A rename would mean rewriting
`use shop::…` in the binary and in every integration test — the only places in a project that name
the library rather than reaching it through `crate::` — and it would move `target/release/shop`,
which the generated `Dockerfile` copies by name. Nothing textual has to be right for the project to
go on compiling, which is what makes the command safe to run on a repository it has never seen.

Two rewrites are performed and only two: `[profile.*]` is lifted to the root, because cargo ignores
a profile declared in a non-root manifest, and a relative `path = "…"` dependency is re-rooted by
the two directories the manifest descended. Everything else — every comment, every version, every
feature list — moves byte for byte. `.env`, `README.md`, the `Dockerfile` and `.cargo/config.toml`
stay at the root.

What it does **not** do is split one crate's contents across five, because that means deciding which
declarations in a file are domain types and which are handlers, and a parser-free tool cannot. The
remaining crates are `cargo new --lib crates/shop-domain` and a path dependency; the `crates/*` glob
picks each one up with no further edit. Rewriting `use` paths across arbitrary user code stays
future work and needs `syn`, which `moso-cli` deliberately does not depend on.

## `moso.toml`

Framework-level project configuration, distinct from runtime app config (`src/config.rs`).

```toml
# spec — moso.toml
[project]
name = "shop"
edition = "2024"

[dev]
watch = ["src", "templates", "migrations"]
ignore = ["target", ".git"]
port = 3000
open_docs = true                 # open /docs on first successful boot

[database]
default = "postgres"
url_env  = "DATABASE_URL"
test_strategy = "template"       # template | migrate | transaction

[openapi]
path = "openapi.json"
check_drift = true               # `moso check` fails if the committed file is stale
title = "Shop API"
servers = ["http://localhost:3000"]

[generate]
dto_suffix_in = ""               # CreateUser / UpdateUser
dto_suffix_out = "Out"           # UserOut
service_layer = false            # don't scaffold services/ per resource

[lints]
layering = "deny"
blocking_in_async = "deny"
missing_doc_comment_on_endpoint = "warn"
```

## Naming conventions (enforced by `moso check`, fixable by `moso fix`)

| Thing | Convention | Example |
| --- | --- | --- |
| Handler fn | verb, lower_snake, no resource prefix | `list`, `create`, `show`, `update`, `destroy` |
| Route path | plural, kebab-case, `{}` params | `/users/{id}/api-keys` |
| Entity | singular PascalCase; table is plural snake_case | `User` → `users` |
| Input DTO | `Create*` / `Update*` / `*Params` | `CreateUser` |
| Output DTO | `*Out` | `UserOut` |
| Job | `*Job`, snake_case queue name | `SendWelcomeEmailJob` |
| Permission | `resource.action` | `posts.publish` |
| Migration | `YYYYMMDDTHHMMSS_verb_object` | `20260729T101500_create_users` |
| Config struct | `AppConfig` + nested `*Config` | `MailConfig` |

## What we borrow and what we refuse

**Borrowed from Django/Rails:** generators, an admin, migrations tied to models, an opinionated
directory layout, a `tasks/` concept.

**Refused:**
- *Autoloading / convention-based discovery.* Everything is a `mod` declaration and an explicit
  `.mount()`. Rust developers will not accept files being loaded by name.
- *Fat models / ActiveRecord `save()` on arbitrary mutation.* Moso entities are plain structs;
  persistence is explicit (`User::insert(...)`, `user.update()...`). No dirty tracking magic.
- *`before_action` chains.* Middleware is Tower layers, applied visibly at the router.
- *A global `App` singleton.* `App` is a value you construct and can construct twice in one test
  process.
