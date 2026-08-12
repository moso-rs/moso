---
title: Project layout
description: What a Moso application looks like on disk, what each generated file is for, the composition root pattern, and when to split into a workspace.
order: 4
status: shipped
---

A Moso project is an ordinary Cargo package with a library target and a binary target. There is no
framework directory, no magic file that gets loaded by convention, and no registry that discovers
your code. Everything reaches the application through a statement someone wrote, which is why
reading one function tells you what the application is.

This page describes what `moso new` generates today, the pattern that shape encodes, and how the
layout grows from one file of handlers to a workspace. It is grounded in the CLI templates and in
`examples/crud`, not in the design documents, which specify a richer layout than the one that is
generated.

## What `moso new` writes

Twelve files. `--with-db` adds two more.

```text
shop/
├── Cargo.toml              the package, the moso dependency, the dev profile
├── .cargo/config.toml      build settings, all commented out, safe to delete
├── .env.example            committed, generated from your Config type
├── .gitignore              ignores /target and .env
├── Dockerfile              multi-stage, produces one deployable image
├── .dockerignore
├── README.md
├── src/
│   ├── lib.rs              the composition root, plus AppConfig and the loader
│   ├── main.rs             the binary: dump dispatch, then serve
│   ├── routes.rs           payload types, handlers, and the route table
│   └── dump.rs             how the moso CLI asks this binary questions
└── tests/
    └── api.rs              five integration tests over the real application
```

| File | What it is for | Do you edit it? |
| --- | --- | --- |
| `src/lib.rs` | Loads configuration, registers providers, mounts routers, sets document metadata, calls `build()`. The whole application in one expression | Constantly |
| `src/main.rs` | Four lines. Answers `--dump-*` if asked, otherwise serves | Almost never |
| `src/routes.rs` | The HTTP surface: `#[derive(Schema)]` payloads, `#[endpoint]` handlers, a `routes!` table | Constantly, until you split it |
| `src/dump.rs` | The protocol the CLI speaks to your binary. Roughly twenty lines and it is yours | Only to add a dump kind |
| `tests/api.rs` | Boots the real application through `into_service()` and speaks HTTP to it | As you add endpoints |
| `.env.example` | Committed sample environment, regenerated from `AppConfig` | Never by hand, see below |
| `.cargo/config.toml` | Turns on the future-incompatibility report; carries commented fast-linker stanzas | To uncomment a linker |
| `Dockerfile` | Multi-stage build to a single image, running as an unprivileged user with the binary as PID 1 | When your deployment differs |

Two dependencies, and no `[dev-dependencies]` at all: `tests/api.rs` reaches Axum, `http` and Tower
through `moso::deps`, so the test harness needs nothing extra in the manifest. Add `moso-test` when
you want `TestApp` and `TestClient`.

`.env.example` is generated, not written. After adding a configuration field:

```bash
moso config --env-example --out .env.example
```

That rewrites the file byte for byte from the doc comments on `AppConfig`, so the committed example
cannot drift from the struct. Pair it with `git diff --exit-code` in CI and the drift becomes a
failing job rather than a stale file. `.env` itself is gitignored and is only read in the `dev` and
`test` profiles, never in production.

## The composition root

This is the one pattern worth internalising. Everything the application *is* lives in one function
in the library, and the binary is a shim over it.

```rust title="src/lib.rs"
pub fn build() -> Result<App> {
    let config = AppConfig::load_from(&loader()?)?;

    // Read what the listener needs before the configuration moves into the
    // builder, where it becomes a provider handlers reach with `Inject`.
    let bind = config.bind;

    App::new(config)
        .server_config(moso::http_config::ServerConfig { bind, ..Default::default() })
        .mount(routes::router())
        .build()
}
```

```rust title="src/main.rs"
use shop::{build, dump};

#[tokio::main]
async fn main() -> moso::Result<()> {
    if let Some(requested) = dump::requested() {
        return dump::run(requested, &build()?);
    }
    build()?.serve().await
}
```

The split is not stylistic. Because the real assembly is a public library function, `tests/api.rs`,
the `--dump-*` flags, and `main` all boot the *same* application: the same provider map, the same
middleware stack, the same OpenAPI document. A test that constructs a parallel router proves
nothing about what you ship. This one cannot drift.

A grown composition root reads like an inventory. From `examples/crud`:

```rust title="src/lib.rs"
App::new(config)
    .provide(Store::new())
    .provide(Metrics::default())
    .mount(routes::router().layer(ObserveLayer::new()))
    .server_config(ServerConfig { bind, ..ServerConfig::default() })
    .health_check("store", StoreIsReachable)
    .openapi(move |document| {
        document
            .title("Moso blog API")
            .version(env!("CARGO_PKG_VERSION"))
            .server(public_url, "this instance")
            .security_scheme(auth::API_KEY_SCHEME, SecurityScheme::api_key_header(auth::API_KEY_HEADER))
            .tag_description("posts", "Everything you can do with a post.");
    })
    .build()
```

Twenty lines that show the configuration, the providers, the routes, the middleware, the readiness
probe and the API metadata. Then `.build()` proves the whole thing: every `Inject<T>` has a
provider, every path template is well formed, no two routes collide, no two operations share an id,
no route is shadowed by a framework path. Delete a `.provide(..)` line and the process refuses to
start, naming every route that wanted it:

```text
error: application failed to build (1 problem)

  x missing provider: `example_crud::store::Store`
      required by  GET /status                      src/routes/health.rs:36
                   GET /api/v1/posts                src/routes/posts.rs:57
                   POST /api/v1/posts               src/routes/posts.rs:87
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Store = /* construct it */;
                   App::new(config).provide(value)
```

### Return the builder, not the App

There are two shapes and the difference matters for testing.

```rust title="src/lib.rs"
// Shape A: what `moso new` generates. Returns a built, validated App.
pub fn build() -> Result<App> { /* ... */ }

// Shape B: what `examples/minimal` uses. Returns the builder.
pub fn app() -> Result<AppBuilder> {
    Ok(App::new(AppConfig::load()?).mount(routes::router()))
}
```

Shape B lets a test edit the application before the boot checks run:

```rust title="tests/hello.rs"
use moso_test::prelude::*;

async fn spawn() -> TestApp {
    TestApp::builder()
        .app(shop::app().expect("configuration loads"))
        .override_provider(shop::AppConfig { greeting: "Hello".to_owned() })
        .spawn()
        .await
        .expect("the application boots")
}
```

Provider registration is last-write-wins, so `override_provider` is how a test swaps a real
dependency for a fake without a second composition root. If you expect to do that, return the
builder. If your tests drive `into_service()` and never override anything, the generated shape is
fine. See [testing](../guides/testing.md).

> [!NOTE]
> If your composition root uses `provide_with` (a provider built at boot, fallibly and
> asynchronously), `build()` must be called from inside a multi-threaded Tokio runtime. That is why
> `#[tokio::main]` wraps `build()` rather than the reverse. A current-thread runtime, or no runtime,
> is a boot error naming the fix.

## Why `src/dump.rs` lives in your project

`moso routes`, `moso openapi export`, `moso openapi check` and `moso config` do not link your crate
and do not parse your source. They build your binary, run it with a flag, and read exactly one JSON
document off standard output.

| Flag | Standard output |
| --- | --- |
| `--dump-openapi` | the OpenAPI document, as JSON |
| `--dump-routes` | `{"routes": [ .. ]}` |
| `--dump-config` | `{"profile": .., "entries": [ .. ]}` |
| `--dump-env-example` | the text of `.env.example` |

That is a consequence of a deliberate decision: nothing registers itself at link time, so a route
table is ordinary Rust and cannot be read without running it. A route registered inside a loop, a
`nest`, or a function in a dependency is invisible to any source scanner and visible to this. The
cost is that the protocol has to live somewhere, and it lives in your project where you can read it
and change it.

Two rules follow from it. **Everything except the one document goes to standard error**, so a stray
`println!` in a startup path breaks `moso routes` with a parse error. And **the flag check must come
before `serve()`**, or the CLI waits out its 60 second timeout and reports that the binary ignored
the flag and started serving.

A project with no `src/dump.rs` is not broken; it just cannot be interrogated. Neither
`examples/minimal` nor `examples/crud` implements the protocol, so `moso routes` does not work
against them.

## How the layout grows

`src/routes.rs` is fine until it is not. The next shape is one module per resource, each exporting a
`router()`, assembled in one place. This is `examples/crud`:

```text
src/
├── lib.rs               the composition root
├── main.rs
├── config.rs            #[derive(Config)] AppConfig, nested sections
├── auth.rs              a Guard, a Dependency, a derived Dependency
├── middleware.rs        #[middleware] plus the Metrics it injects
├── error.rs             #[derive(moso::Error)] BlogError
├── models/
│   ├── mod.rs           re-exports
│   └── post.rs          the domain type, three DTOs, the pagination key
├── store.rs             persistence, the only file a real database touches
└── routes/
    ├── mod.rs           Router::new().merge(..).nest("/api/v1", ..)
    ├── health.rs        one endpoint reading three providers
    └── posts.rs         six handlers and the route table
```

| Module | Holds | Why separate |
| --- | --- | --- |
| `config.rs` | one `#[derive(Config)]` struct, with nested sections | Every key in one file is what makes `moso config` and `.env.example` useful |
| `models/<aggregate>.rs` | the domain type, the DTOs the API speaks, the conversions | A reader who wants to know what a post *is* reads one file |
| `routes/<resource>.rs` | handlers plus a `pub fn router() -> Router` | The route table sits next to the handlers it names |
| `routes/mod.rs` | merges and nests the resource routers | One place shows the whole URL space |
| `store.rs` | persistence | The seam a database replaces without touching handlers |
| `error.rs` | one error enum per domain | `#[endpoint(errors = BlogError)]` folds it into the document |
| `auth.rs` | guards and request-scoped identities | Reused by several route modules |
| `middleware.rs` | `#[middleware]` functions and their state | Mounted once, in the composition root |

The assembly file stays small:

```rust title="src/routes/mod.rs"
pub mod health;
pub mod posts;

/// Every route this application serves.
pub fn router() -> Router {
    Router::new()
        .merge(health::router())
        .nest("/api/v1", api_v1())
}

/// Version 1 of the API.
fn api_v1() -> Router {
    posts::router().responds(429, ResponseSpec::problem("Too many requests."))
}
```

`nest` rewrites the paths and pushes the outer router's accumulated metadata down onto what it
absorbs, so `/posts` registered in `posts.rs` is documented as `/api/v1/posts` and the `429` is
stated once instead of copied into six handlers. `merge` composes paths unchanged and does not push
metadata down, because a merged router is a sibling that already described itself. Versioning is
`nest` plus separate modules; there is no versioning DSL, deliberately.
[Routing](../guides/routing.md) covers both, including the rule that `.tag()`, `.guard()` and
`.layer()` apply to the routes registered *before* the call.

The domain type not being a `Schema` is the other half of this layout. `examples/crud` keeps `Post`
out of the API surface entirely and returns `PostOut`, so renaming a field on the domain type stops
the projection compiling rather than silently changing the contract. See
[entities are not schemas](../guides/schemas.md).

## What `moso generate` writes, and where

`moso generate` scaffolds into a project that already exists. Everything it produces is ordinary
code you own from the moment it lands: no registry, no marker comment it comes back and rewrites, no
second invocation that "updates" its own output.

| Command | Writes | Also edits `src/lib.rs` |
| --- | --- | --- |
| `moso generate endpoint post` | `src/posts.rs` (payloads, a store, five handlers, a router) | `pub mod posts;`, `.mount(posts::router())`, `.provide(posts::PostStore::default())` |
| `moso generate schema invoice` | `src/invoices.rs` (payload types only) | `pub mod invoices;` |
| `moso generate error billing` | `src/billing_error.rs` (an RFC 9457 taxonomy) | `pub mod billing_error;` |
| `moso generate middleware observe` | `src/observe.rs` (a Layer and Service pair) | `pub mod observe;` |
| `moso generate test posts` | `tests/posts.rs` (an end-to-end contract test) | nothing; a file under `tests/` is its own crate |

`endpoint` and `schema` pluralise the name you give them; `error` and `middleware` do not, so
`generate error billing` writes `src/billing_error.rs` and `BillingError`. Correct a wrong guess
with `--singular`. The name can be spelled however is natural: `post`, `posts`, `blog-post`,
`blog post` and `BlogPost` all reach the same module.

Use `--dry-run` to see the plan without writing:

```bash
moso generate endpoint product --dry-run
```

```text
  ✓ would write                     src/products.rs
  ✓ would edit                      src/lib.rs, add `pub mod products;`
  ✓ would edit                      src/lib.rs, add `.mount(products::router())`
  ✓ would edit                      src/lib.rs, add `.provide(products::ProductStore::default())`
```

The edits are found by matching text `moso new` wrote. If you have restructured the project the
match fails, and the command tells you the exact line to add by hand rather than guessing; that is
an exit code of 0, not a failure. An existing target file *is* a failure naming `--force`, and the
existence check runs before `--dry-run`, so a dry run against a file that exists still exits 1. A
generator that clobbers is a generator nobody runs twice.

The store the endpoint generator writes is an in-memory `Arc<Mutex<..>>`. It is meant to be replaced.

## Configuration on disk

`ConfigLoader::standard()` builds this stack, highest precedence first:

| Level | Source | Notes |
| --- | --- | --- |
| 1 | programmatic overrides | what a test sets |
| 2 | command line | `--key=value` style arguments |
| 3 | environment | `SHOP__DATABASE__URL`, prefix from `with_prefix` |
| 4 | `.env` | **dev and test only**, skipped entirely in production |
| 5 | `config/<profile>.toml` | `dev.toml`, `test.toml` or `production.toml` |
| 6 | `config/default.toml` | shared across profiles |
| 7 | `#[config(default = ..)]` | declared on the field |

The `config/` directory is optional and `moso new` does not create one; a file that does not exist
is not an error, though a file that exists and does not parse is. Point somewhere else with
`MOSO_CONFIG_DIR`. Nesting a section adds another `__` to the environment name, so a `database.url`
field under prefix `SHOP` is `SHOP__DATABASE__URL`.

`moso config` prints which level won for every key, which is the fastest way to answer "why is it
using that value". See [configuration](../guides/configuration.md).

## The database variant

`moso new --with-db` adds the migration story:

```text
src/db.rs                              answers the --db-* protocol moso db speaks
migrations/20260101T000000_init.sql    a first migration that does something real
```

plus a `database_url: SecretString` field on `AppConfig`, a `moso-migrate` dependency, and a branch
in `main` that dispatches `--db-status`, `--db-migrate`, `--db-rollback <N>` and `--db-redo` before
serving. `.env.example` grows a `SHOP__DATABASE_URL=` line marked `[required]`. It is off by default
because it pulls a database driver, and an application that does not need one should not compile
sqlx to find that out.

A migration file is one `up` section and one `down` section separated by `-- +migrate up` and
`-- +migrate down` markers, named `<YYYYMMDDTHHMMSS>_<name>.sql`. The ledger table records a
checksum of each applied file, so editing one that has already run is reported by `moso db status`
rather than silently ignored.

`src/db.rs` is yours for the same reason `src/dump.rs` is: a migration that needs application logic,
such as backfilling a column by calling your own code, is registered there with `Runner::register`,
and the CLI cannot link your crate to find it. `moso db` checks for `src/db.rs` before it builds
anything, so a project without it fails immediately rather than after a long compile. See
[migrations](../guides/migrations.md).

## When to split into a workspace

Not yet. A single package handles far more than people expect, and the cost of splitting early is
paid on every build.

Split when one of these is true:

- **You have a second deliverable.** A worker binary, a CLI, a shared client library. Note that a
  separate worker *process* is not a reason on its own: `app.serve_workers()` runs the same binary
  with no HTTP listener, which is deliberate, because a worker that cannot drift from what the web
  process proved at boot is worth more than a smaller binary.
- **Compile times are dominated by one area you rarely touch.** Splitting a stable domain crate out
  means editing a handler stops recompiling it.
- **Teams need enforceable boundaries.** Crate visibility is the only boundary Rust enforces.

A workspace split looks like this:

```text
shop/
├── Cargo.toml           [workspace] members = ["crates/*"]
└── crates/
    ├── shop-domain/     types and rules, no HTTP, no database
    ├── shop-data/       entities, queries, migrations
    ├── shop-api/        handlers, routers, the composition root
    └── shop-server/     the binary: four lines over shop-api
```

Keep the composition root in exactly one crate. Two crates that both assemble an `App` is the shape
that produces "it works in the test but not in production".

`moso generate workspace` performs the mechanical half of that split:

```bash
moso generate workspace --dry-run    # what it would move
moso generate workspace
```

The package moves to `crates/<name>/` (with `git mv`, so the history follows), the root becomes a
workspace with `members = ["crates/*"]`, and `[profile.*]` is lifted to the root, which is the only
place cargo honours it. The package keeps its name, so `use shop::…` in your binary and your tests
still resolves and the binary still lands at `target/release/shop`. `.env`, `README.md`, the
`Dockerfile` and `.cargo/config.toml` stay at the root, where every tool already looks for them.

It refuses rather than half-migrating: an existing `crates/`, a manifest that is already a workspace
root, or a dirty working tree in a git repository each stop it before anything moves (`--force`
skips the last one), and a move that fails part-way is undone in reverse.

What it does not do is decide which of your types are domain types. Create the next crate with
`cargo new --lib crates/shop-domain`, add it to `crates/shop/Cargo.toml` as a path dependency, and
move code across; the `crates/*` glob picks it up with no further edit.

> [!NOTE]
> After the split, the `moso` commands that run your application look for the nearest package. Run
> them from `crates/<name>`, or pass `--manifest-path crates/<name>/Cargo.toml`.

## Where this differs from the design documents

The generated layout is flatter than the specified one, and this page describes what is generated.
The design documents show `src/config.rs`, `src/error.rs`, `src/routes/`, `src/models/`,
`src/services/`, `src/jobs/`, `src/tasks/`, a committed `openapi.json` and `tests/common/mod.rs`.
`moso new` writes a flat `src/lib.rs`, `src/main.rs`, `src/routes.rs`, `src/dump.rs`, with
`AppConfig` in `lib.rs` rather than `config.rs`, and the composition root named `build()` rather
than `app()`. There is also no `moso.toml`: no file of that name is read anywhere, and no
`compose.yaml` is generated.

`examples/crud` is the closest thing to the specified layout that exists, and growing into it by
hand is a five minute job.

## Things that trip people

**Creating a project inside another workspace.** `moso new` walks up looking for a manifest with a
`[workspace]` table. If it finds one, the generated `Cargo.toml` gets an empty `[workspace]` stanza
of its own, with a comment saying why, so the new project is a workspace root rather than an
unlisted member of yours. Delete that stanza to join the outer workspace instead.

**Project names.** ASCII alphanumerics, hyphens and underscores, at most 64 characters, not starting
with a digit, not a Rust keyword, not `test`, `core`, `std`, `alloc`, `proc-macro`, `build` or
`deps`. The library name is the crate name with hyphens turned into underscores, and the environment
prefix is that upper-cased: `my-shop` becomes `my_shop` and `MY_SHOP`. A rejected name comes back
with a corrected `moso new ...` command you can paste.

**Dumping after serving.** In `main`, the `--dump-*` branch has to come before `serve()`, or the CLI
waits out its timeout on a process that is busy listening.

**Committing `.env`.** The generated `.gitignore` excludes it and commits `.env.example`. Keep it
that way; `.env` is where secrets go.

**Regenerating over existing files.** `moso new --yes` implies `--force` for the non-empty-directory
check, and files with the same names are overwritten with no diff and no per-file confirmation.
Unrelated files are left alone.

**Mounting an Axum router and expecting boot validation.** `mount_axum` routes contribute nothing to
the OpenAPI document and are invisible to boot validation, so a mistake in one is a runtime failure
rather than a boot error. That is the documented trade for the escape hatch. `Router::into_axum()`
is different again: it drops the application state, so `Inject<T>` inside it fails at runtime. Use
`App::into_service()`, which keeps the state.

## See also

- [Quick start](./quick-start.md) generates the project this page describes.
- [Configuration](../guides/configuration.md) for the loader, profiles and secrets.
- [Routing](../guides/routing.md) for `nest`, `merge` and metadata scoping.
- [Dependency injection](../guides/dependency-injection.md) for what belongs in the composition root.
- [Testing](../guides/testing.md) for the harness the lib/bin split exists to serve.
