---
title: Installation
description: The toolchain Moso needs, how to depend on it while it is unpublished, every Cargo feature, installing the CLI, and a check that proves it worked.
order: 2
status: shipped
---

Moso needs a stable Rust toolchain and nothing else. The optional pieces (Postgres for the ORM,
Redis for the cache and the job queue) are only needed when you turn them on, and this page says
where each one starts to matter.

> [!IMPORTANT]
> Moso is not released. The workspace is at an unpublished `0.1.0` with no release tags, so
> `cargo add moso` and `moso = "0.1"` do not resolve to the code this site documents. Until the
> first release you install from a checkout. That is what the rest of this page does.

## The toolchain

Moso targets **stable Rust**, with a minimum supported version of **1.94** and **edition 2024**. The
workspace pins `channel = "stable"` in `rust-toolchain.toml` rather than a patch version,
deliberately: pinning `1.97.1` would force a second toolchain download on every clone for no
benefit, and the minimum version is tested by a dedicated CI leg instead.

```bash
rustup toolchain install stable
rustup default stable
rustc --version
```

Anything at 1.94 or above works. If `rustc --version` reports less than that, run `rustup update`.

You also need `git`, and a linker. On Linux install `build-essential` or the equivalent; on macOS
install the Xcode command line tools with `xcode-select --install`.

Moso itself needs no C toolchain and no code generator. The one place a cold build gets long is the
`orm` feature, which pulls `sqlx` with a bundled SQLite that compiles an amalgamation from C. That
is why `orm` is off by default.

## Get the source

```bash
git clone https://github.com/lowsbarrel/moso.git
cd moso
```

Note the absolute path of that checkout. You will pass it to the CLI, and the CLI writes it verbatim
into the manifests it generates.

## Depend on Moso from your application

Point at the checkout by path. This is the only route that is guaranteed to work today.

```toml title="Cargo.toml"
[package]
name = "shop"
version = "0.1.0"
edition = "2024"
rust-version = "1.94"

[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

Two things about that block are worth reading twice.

`moso` is the **facade**. It is the only Moso crate an application normally names: it re-exports the
runtime, owns the prelude, and owns the hidden module every macro expansion resolves against. Naming
`moso-core` directly instead will appear to work and then fail in confusing ways when a macro cannot
find its own paths.

`tokio` is **yours**. Moso does not pick your runtime, start it, or hide it. You write
`#[tokio::main]` in your own `main`, with a version you control.

Once there is a published release the same block becomes `moso = "0.1"` and this section gets
shorter. A git dependency works as soon as the repository is reachable from your machine, and pins
with `rev = "..."` the way any git dependency does:

```toml title="Cargo.toml"
[dependencies]
moso = { git = "https://github.com/lowsbarrel/moso.git" }
```

## Cargo features

The facade defaults to `["http", "openapi", "tracing"]`. Everything else is opt in, because the cost
of a feature is compile time and dependency count, and a stateless JSON service should not compile a
database driver to find out it does not need one. The architecture rule is explicit: no default
feature of the facade reaches a database.

| Feature | Default | What it turns on | What it costs |
| --- | --- | --- | --- |
| `http` | on | nothing. Accepted and inert; `moso-core` is unconditional | nothing |
| `openapi` | on | mounts `/docs` and `/openapi.json` | `moso-openapi` and the embedded docs UI in the binary |
| `tracing` | on | the trace layer and request-id spans in the default middleware stack | nothing new; `tower-http` and `tracing` are already in the graph |
| `compression` | off | response compression in the default stack | brotli and gzip encoders |
| `cors` | off | the CORS layer, which is what answers preflight | `tower-http/cors` |
| `multipart` | off | makes `moso::extract::Multipart` exist | `axum/multipart` |
| `ws` | off | re-exposes Axum's WebSocket surface | `axum/ws` |
| `orm` | off | `moso::db`, `moso::sql` and `#[derive(Entity)]` | sqlx, sea-query, a database driver |
| `authz` | off | `permissions!`, `roles!`, `#[requires]`, `#[public]`. Implies `orm` | `moso-authz` plus the `orm` cost |
| `jobs` | off | `#[job]` and transactional enqueue. Implies `orm` | `moso-jobs` plus the `orm` cost |
| `test` | off | the `dependency_overrides` table used by the test harness | nothing at runtime |

Three notes that catch people.

The OpenAPI document is generated in **every** build. The `openapi` feature only decides whether the
two routes that serve it are mounted, which is why `moso openapi export` works even in a build with
the feature off.

`orm` has to be one feature and not three. `#[derive(Entity)]` expands to impls of `moso-orm` traits
written in `moso-sql` value types, all named through the facade's hidden module, so turning on any
one of the three alone gives you a half-wired derive.

`test` belongs in `[dev-dependencies]` and never in `[dependencies]`, so a production build of the
same crate cannot resolve an overridden dependency:

```toml title="Cargo.toml"
[dev-dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso", features = ["test"] }
moso-test = { path = "/absolute/path/to/moso/crates/moso-test" }
```

## The battery crates you name separately

`orm`, `authz` and `jobs` are features of the facade. The rest are separate crates you add to
`[dependencies]` yourself, each with its own default feature set, so that a project using the cache
does not compile an S3 client.

| Crate | Default features | Other features |
| --- | --- | --- |
| `moso-kv` | `memory` | `redis`, `pg-kv` |
| `moso-auth` | none | none |
| `moso-mail` | `console`, `memory` | `file`, `mail-smtp`, `mail-ses`, `mail-sendgrid`, `mail-postmark`, `mail-resend`, `mail-mailgun` |
| `moso-storage` | `local`, `memory` | `s3`, `gcs`, `azure` |
| `moso-migrate` | `postgres`, `sqlite` | pick one to drop the other |
| `moso-orm` | `postgres`, `sqlite`, `tls` | pick a subset |
| `moso-jobs` | `jobs-pg`, `jobs-memory` | `jobs-redis` |
| `moso-test` | `server` | `db` |

`moso-orm` keeps both backends on by default because `Backend` is a runtime enum and a
`DATABASE_URL` decides which one a process opens; turning one off is for a deployment that wants a
smaller binary, not a compile-time choice about which SQL to write. `tls` is on because every
managed Postgres (RDS, Cloud SQL, Neon, Supabase) refuses a plaintext connection, and a default
without it turns "it works locally" into a deployment-day failure.

`moso-auth` depends on `moso-orm` and `moso-kv`, so adding it pulls a database driver whether or not
you asked for one. That is a real cost and it is worth knowing before the first build rather than
after it.

`moso-test`'s default `server` feature binds a real ephemeral TCP port and drives the application
over a real socket with an HTTP client. Turn it off with `default-features = false` and `TestClient`
calls the composed tower service in process, with no socket and no client, which is faster. Both
paths run the same middleware stack. The `db` feature adds per-test database strategies and
`assert_queries!`, and pulls `sqlx`.

## Install the CLI

The `moso` binary scaffolds projects, lists routes, exports the OpenAPI document, prints resolved
configuration, drives migrations, and runs a watching dev loop. It is not required: everything it
does is also doable with `cargo` and a text editor. It is worth having.

```bash
cargo install --path crates/moso-cli --locked
moso --version
```

```text
moso 0.1.0
```

`--locked` uses the committed `Cargo.lock` rather than resolving fresh, which is what you want for a
tool. The CLI has five dependencies and no library target, so that build takes about twenty seconds
on a warm cargo cache. Installing puts the binary in `~/.cargo/bin`; make sure that is on your
`PATH`.

If you would rather not install it globally while trying Moso out, every command works through cargo
from inside the checkout:

```bash
cargo run -p moso-cli -- new shop --yes
```

The CLI and the framework are released together: `moso new` pins the generated `Cargo.toml` to the
CLI's own minor version, so a mismatched pair scaffolds a project that will not build. Reinstall the
CLI after you pull the checkout.

Shell completions are generated from the same clap tree the CLI parses with, so they cannot disagree
with what it accepts. `bash`, `elvish`, `fish`, `powershell` and `zsh` are offered; Nushell is not.

```bash
moso self completions zsh  > ~/.zfunc/_moso
moso self completions bash > /etc/bash_completion.d/moso
moso self completions fish > ~/.config/fish/completions/moso.fish
```

## Optional services

Nothing on this page so far needs a server running. A Moso application with no database, no cache
and no queue boots, serves, validates, documents itself, answers `/healthz` and `/readyz` and drains
on `SIGTERM` with zero external dependencies. The [quick start](./quick-start.md) binds nothing.

### Postgres

Needed by `moso-orm`, `moso-migrate`, the SQL job backend, the Postgres KV backend, the
authorization storage, and the `moso db` subcommands. The workspace ships a throwaway cluster on a
non-default port, so it cannot collide with a Postgres you already run on 5432:

```bash
./scripts/test-db.sh up                       # postgres:17-alpine on localhost:55433
export DATABASE_URL="$(./scripts/test-db.sh url)"
./scripts/test-db.sh status                   # reachability, version, privileges
./scripts/test-db.sh down                     # stop and delete, data included
```

The same thing through compose, if you prefer:

```bash
docker compose -f compose.test.yaml up -d --wait
export DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test
```

That cluster runs with `fsync=off` and its data directory in RAM. It is disposable by design. Never
point anything you care about at it.

SQLite is also supported and needs nothing installed: the driver is bundled. It is the cheapest way
to try the data layer.

### Redis

Needed by the `redis` feature of `moso-kv` and by the Redis job backend. The compose file does not
provision it, so start one yourself:

```bash
docker run -d --name moso-test-redis -p 56379:6379 redis:7-alpine
export REDIS_URL=redis://localhost:56379
```

The database and Redis test suites **skip silently** when `DATABASE_URL` and `REDIS_URL` are unset.
A skipped test still passes, so a green `cargo test` on a machine with no servers running proves
less than it looks like it does: 8,461 tests run without them, against 8,898 with both set.

## Verify the install

Three checks, in increasing order of how much they prove.

### The CLI is on your path

```bash
moso --version
```

### This machine can build a Moso project

```bash
moso doctor
```

```text
  ✓ rustc                           1.97.1 (MSRV 1.94 satisfied)
  ✓ cargo                           cargo 1.97.1 (c980f4866 2026-06-30)
  ⚠ linker                          using the default; a faster one would save link time
      → brew install llvm, then uncomment the macOS stanza in .cargo/config.toml
    rust-lld                        shipped with this toolchain (-C link-arg=-fuse-ld=lld)
    cargo-nextest                   not installed (cargo test still works)
      → cargo install cargo-nextest --locked
  ✓ disk                            215.5 GB free
```

`doctor` runs with or without a project present. Only five conditions exit non-zero: `rustc`
missing, `rustc` older than the project MSRV, `cargo` missing, a `.cargo/config.toml` that does not
parse, and less than 2 GiB free. Everything else is a warning or an informational row, and every
actionable row carries a command you can paste. Run it inside a project and it adds rows for the
`moso` dependency, `.cargo/config.toml`, the size of `target/`, and which `.env.example` keys your
`.env` does not supply.

### A generated project builds, tests and serves

This is the check that proves the whole chain, including that your `--moso-path` is right.

```bash
moso new shop --yes --no-git --moso-path /absolute/path/to/moso/crates/moso
cd shop
cargo test
```

You should see five passing integration tests:

```text
running 5 tests
test every_route_is_documented ... ok
test a_greeting_is_created ... ok
test an_invalid_body_is_rejected_with_a_pointer_to_the_field ... ok
test an_unknown_path_is_a_problem_document ... ok
test the_root_greets_the_world ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

That single command exercised the whole stack: the macros expanded, `App::build()` ran its boot
checks, the middleware stack composed, the OpenAPI document assembled, and the application answered
four HTTP requests including a 422 with a JSON Pointer.

> [!WARNING]
> `--moso-path` is not optional today. Without it the generated `Cargo.toml` says `moso = "0.1"`,
> which resolves against crates.io and fails. The flag writes a path dependency instead. If you
> forget, edit the one line in `Cargo.toml` by hand.

## Common problems

| Symptom | Cause | Fix |
| --- | --- | --- |
| `failed to select a version for the requirement moso = "^0.1"` | Resolving against crates.io | Use a path or git dependency, or pass `--moso-path` |
| `failed to load source for dependency 'moso'`, `found a virtual manifest` | `--moso-path` pointed at the checkout root | Point it at `<checkout>/crates/moso` |
| A relative `--moso-path` does not resolve | The path is written verbatim into the generated `Cargo.toml`, so it is relative to the *project* directory, one level below where you ran the command | Use an absolute path, or prefix it with `../` |
| `package requires rustc 1.94 or newer` | Old toolchain | `rustup update stable` |
| `moso: command not found` | `~/.cargo/bin` is not on `PATH` | Add it, or re-run rustup's shell setup |
| `cannot find type __moso_op_greet in this scope` | A name in a `routes!` table does not match a function carrying `#[endpoint]` | Fix the typo; the underline is on the name you wrote |
| A macro path fails to resolve, or `moso::__private` is missing | You depended on `moso-core` instead of `moso` | Depend on the facade |
| `Address already in use` on `cargo run` | Something else holds port 3000 | `SHOP__BIND=127.0.0.1:8080 cargo run` |

The build being slow on every rebuild rather than only the first is usually the linker rather than
the compiler. `moso doctor` prints the install command for a faster one on your platform, and the
generated `.cargo/config.toml` has a commented stanza ready to uncomment.

## Next

[Quick start](./quick-start.md) takes the project you just generated and gets a validated,
documented endpoint answering a `curl`, in under ten minutes. [Project layout](./project-layout.md)
explains every file it wrote.
