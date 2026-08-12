# 40 — The `moso` CLI

> **Status: all 19 top-level commands in the tree are built.** Nothing is stubbed. What is *not*
> built is absent from the command tree rather than printing "coming soon" — so `moso --help` is a
> list of things that work, and the gaps are enumerated under
> [What is deliberately absent](#what-is-deliberately-absent) below rather than shown as ⛔ rows a
> user could type.
>
> The commands that interrogate an application (`routes`, `openapi`, `config`, `middleware`,
> `check`, `jobs`, `auth`, `authz`, `db`, `deploy checklist`) work by **running the application
> binary** with a `--dump-*` or `--db-*` flag and reading the one document it answers on stdout.
> That is a direct consequence of ADR-0004: with no link-time registry, the route table is ordinary
> Rust that only exists once `router()` has been called. `moso new` writes `src/dump.rs` into every
> generated project for exactly this reason, and the file is commented so the user can see the
> protocol rather than discover it.

## Distribution (a Loop-1 requirement)

🟡 **`moso self update` reports; it does not replace.** It prints the running version and, with
`--check`, asks the registry what the newest published one is — the only network access this CLI
ever makes, and only when that flag is given. It then names the command that would update *this*
installation and stops. Whatever installed the binary — cargo, a package manager, an archive
somebody unpacked — is what can correctly replace it, and a self-replacing binary that guessed wrong
would leave a user with two `moso`s on their `PATH`.

⛔ The rest of this section is not built: there is no release pipeline, no prebuilt binary, no
Homebrew tap, and no background version-check notice.

`cargo install moso-cli` takes minutes. That is unacceptable as a first impression, so the CLI is
meant to ship three ways:

1. **Prebuilt binaries** for macOS (arm64/x64), Linux (gnu/musl, arm64/x64), Windows (x64), via a
   shell/PowerShell installer and `cargo-binstall`.
2. **Homebrew** (`brew install moso`) and a `.deb`/`.rpm`.
3. `cargo install moso-cli` for people who prefer it.

The binary is a single static file with templates embedded. Target size: < 15 MB.

## Command overview

`moso --help` lists these in this order, which is a lifecycle rather than an alphabet: create a
project, work in it, verify it, move its data, ask it questions, publish its contract, ship it.

```
moso new <name>            create a project                              ✅
moso generate <what>       scaffold code into it                         🟡 6 of 11 kinds

moso dev                   watch, rebuild, restart                       ✅
moso run                   build and run once, forwarding the exit code  ✅

moso test                  run the project's tests, both passes          🟡 no managed database
moso check                 static analysis beyond rustc                  ✅ 10 lints

moso db <sub>              migrations and database tasks                 ✅ needs --with-db

moso routes                list routes                                   ✅
moso middleware            show the composed middleware stack            ✅
moso config                resolved config, .env.example, --check        ✅
moso jobs <sub>            inspect and manage queues                     ✅ needs src/dump.rs
moso auth calibrate        measure argon2id on this machine              ✅ needs src/dump.rs
moso authz <sub>           inspect permissions, explain decisions        ✅ needs src/dump.rs

moso openapi export        write the OpenAPI document                    ✅
moso openapi check         fail if the committed document is stale       ✅
moso client                generate a typed TypeScript or Rust client    ✅

moso build                 release build, reporting the artefact         ✅
moso deploy checklist      audit this project against production         🟡 checklist only

moso doctor                diagnose the environment and the project      ✅
moso self completions      print a shell completion script               ✅
moso self update           report the version and how to update it       🟡 reports only
```

`jobs`, `auth` and `authz` are marked "needs `src/dump.rs`" rather than ✅ outright: the CLI links no
Moso crate, so the commands exist and work against any project whose `src/dump.rs` answers
`--dump-jobs`, `--dump-auth` and `--dump-authz`. `moso new` writes stubs that answer honestly that
the battery is not wired, and the code to paste in its place is in a comment above each one —
except for `--dump-auth`, which `moso new --auth` fills in for real.

`db` is marked the same way and for the same reason. All eight subcommands — `status`, `migrate`
(with `--all-tenants`), `rollback`, `redo`, `make-migration`, `check`, `squash`, `seed` — are built,
and each runs the application with a `--db-*` flag that `src/db.rs` answers by calling
`moso_migrate::command`. That file only exists in a project created with `moso new --with-db`, and
`moso db` says so before it builds anything when it is missing.

## What is deliberately absent

Each of these was decided against rather than deferred, and each has its reason recorded where the
decision was made. None of them is in the command tree.

| Not built | Why, in one line |
| --- | --- |
| `moso worker` | A worker links the application's job bodies, which ADR-0004 forbids the CLI from doing — see [`moso jobs`](#moso-jobs) |
| `moso task <name>` | A task is Rust in `src/tasks/`, so running one means linking the user's crate; the same wall |
| `moso db prune-test` | The pruner lives in `moso-test`, and `src/db.rs` is compiled into the production binary |
| `moso deploy dockerfile\|compose\|k8s\|<provider>` | Moso is not a PaaS; `deploy checklist` is the one useful thing in that family |
| `moso check --fix` | Five of the sketched lints cannot run at all yet; a fixer for the other ten is a separate design |
| `moso new --template <git-url>` | There is no template registry to fetch from |
| `moso test --coverage\|--watch\|--ui`, the managed test database | All need a database front end this build does not have |

`moso db prune-test` deserves the longer version, because it is the one gap with a designed way out.
The pruner is `moso_test::db::prune_test_databases`, and `src/db.rs` is compiled into the
**production** binary — so answering `--db-prune-test` would put the test harness, and the
`dependency_overrides` surface behind `moso/test`, in every deployment. Reimplementing the name
rules in the template would fork the entire safety argument instead. The unblocking design is
`moso-test = { optional = true, default-features = false, features = ["db"] }` behind a `prune-test`
cargo feature that the CLI passes through its existing `--features` plumbing, and that changes what
`moso new` generates, which is RFC-required.

## One word, one meaning

The tree is small enough that a word used twice has to mean the same thing twice.

- **`check`** — as a subcommand (`moso check`, `moso openapi check`, `moso db check`) and as a flag
  (`moso config --check`, `moso client --check`) it always means *verify and report; write nothing*,
  and every one of them exits non-zero when what it verified is wrong, so any can gate CI.
  `moso self update --check` is the one that only reports: a newer CLI having been published is not
  a defect in your project, and failing a build over it would be hostile.
- **`--json`** is global and every command that produces data honours it. The three that do not —
  `dev`, `run` and `self completions` — produce a child process's stream or a shell script rather
  than data, and their `--help` says so.
- **`--out`/`-o`** is always the destination path, and **`--all`** always means *include the entries
  that are normally hidden*.
- **`--release`** is always cargo's profile and **`--profile`** always Moso's `MOSO_PROFILE`. They
  are independent, and the pairing that catches people out is a `--release` build still running
  under the `dev` profile with `/docs` mounted. `moso build` deliberately has neither: it is a
  release build by definition, and `--debug` is the opt-out.
- **`--yes`/`-y`** always means *do not ask*, and only ever appears on something that destroys or
  overwrites.

## `moso new`

```
$ moso new shop
? Database                    › Postgres   SQLite   None
? Include authentication      › Yes
? Include admin panel         › Yes
? Include background jobs     › Yes
? API style                   › JSON API   JSON API + server-rendered pages
? Set up Docker Compose       › Yes

  ✓ created shop/                             (23 files)
  ✓ wrote .cargo/config.toml                  (fast dev builds: rust-lld)
  ✓ wrote compose.yaml                        (postgres:17, redis:8)
  ✓ initialised git, first commit

  next:
    cd shop
    docker compose up -d
    moso dev

  then open http://localhost:3000/docs
```

**As built**, `moso new` is **not interactive**. Two of the questions above are answerable and are
flags — `--with-db` and `--auth` — and the rest select batteries that do not exist, so asking them
would be theatre. One prompt survives, the one that protects something: overwriting a directory that
already has files in it (`--force` skips it, `--yes` accepts every default).

```
$ moso new shop
  ✓ created shop/                    (12 files)
  ✓ initialised git, first commit

  next:
    cd shop
    cargo run
  then open http://localhost:3000/docs
```

Flags: `--path`, `--yes`, `--no-git`, `--force`, `--moso-path <DIR>` (depend on a Moso checkout on
disk instead of the published crate — the path is written verbatim, so a relative path stays
relative), `--with-db`, `--auth`. ⛔ `--template <git-url>` is not implemented.

Generated files: `Cargo.toml`, `.gitignore`, `.env.example`, `.cargo/config.toml`, `Dockerfile`,
`.dockerignore`, `README.md`, `src/lib.rs` (the composition root), `src/main.rs`, `src/routes.rs`,
`src/dump.rs`, `tests/api.rs`. Every one is **plain, readable Rust with comments explaining the
choices**, including the dump protocol the CLI relies on. No hidden framework files. Someone reading
the generated project should learn the framework from it.

### `moso new --with-db`

Adds `src/db.rs` (the `--db-*` protocol `moso db` speaks), a first `migrations/` file, a
`database_url` on `AppConfig`, and the `moso-migrate` dependency. Off by default because it pulls a
database driver, and an application that does not need one should not compile sqlx to find that out.

### `moso new --auth`

The second of the authentication battery's two tiers ([`03-batteries/30-auth.md`](../03-batteries/30-auth.md)):
`moso::auth::routes()` mounts a fixed set of flows over the framework's own `DefaultUser` for
prototyping, and this **copies handlers into your project** over a user type declared in your crate.

```
$ moso new shop --auth
  ✓ created shop/                    (15 files)
  ✓ wrote src/auth.rs                (register, login, logout, sessions, password reset)
  ✓ wrote .env                       (SHOP__SESSION_SECRET, from this machine)

  the hashing parameters are OWASP's floor until you measure this machine:
    moso auth calibrate
```

It writes `src/auth.rs` — the `User` type, an `AccountStore` over one map, the `Outbox` a token is
handed to, and seven handlers — plus `tests/auth.rs`, which drives all five flows over HTTP and is
what the acceptance test runs. `AppConfig` gains `session_secret` and the three argon2id parameters;
the facade's `auth` feature and `moso-kv` are added to the manifest.

Two properties are worth stating because they are the reason the tier exists:

- **every handler carries `#[endpoint]`**, so the flows are in the project's *own* OpenAPI document
  and `moso client` generates a typed client for them. The mounted set cannot be: `moso-auth` sits
  below the facade and a macro expansion may only name `::moso::__private::…`, so `#[endpoint]` is
  unavailable to it and its operations are registered as undocumented;
- **the account store is in memory**, and it is the one thing in the file that is not
  production-shaped. It is eight methods over a `HashMap`, doc-commented with what each becomes
  against a database. The hashing, the signed cookie, the single-use tokens and the enumeration and
  timing defences are all real.

`--auth` is also the only invocation of `moso new` that writes a secret: the session signing key is
required configuration with no default, so it writes a `.env` holding 32 bytes from the operating
system's random number generator. `.gitignore` already excludes `.env`, so the `git add --all` that
follows cannot pick it up.

⛔ There is no `compose.yaml` and no Docker Compose anything: it describes a database front end this
build does not have.

## `moso dev` — the edit loop

✅ **Implemented**: watch, rebuild, restart, and keep serving through a broken edit.

```
$ moso dev
  ✓ watching          src, Cargo.toml, Cargo.lock, config, migrations, .env
  ✓ listening         shop (compiled in 2.41s)

  ~ src/routes.rs changed
  ✓ recompiled        1.83s   ✓ restarted
```

Behaviour as built:
- Watches `src`, `Cargo.toml`, `Cargo.lock`, `build.rs`, `config`, `templates`, `migrations` and
  `.env`, whichever exist. `--watch <PATH>` replaces the set and is repeatable; `--poll <MS>` sets
  the interval (300 ms by default).
- **On a compile error the previous process keeps serving**, so a broken intermediate edit costs the
  compiler's message and nothing else. `--exit-on-error` inverts that for a CI or agent-driven loop
  that wants a non-zero exit instead of a process that stays up.
- Everything after `--` is passed to the application: `moso dev -- --port 8080`.
- The child inherits standard output, so `--json` has nothing of its own to print and the `--help`
  says so.

⛔ Not built, and each would be a real feature rather than a detail: request queueing across a
restart, automatic migration on boot, the entity-change → migration offer, `--browser`, and the
Moso-specific gloss on a known rustc error pattern.

## `moso run`

✅ **Implemented.** Builds with cargo, runs the binary **with the project root as its working
directory**, and exits with whatever the application exited with.

```
moso run
moso run --release --profile production
moso run -- --port 8080
```

Four differences from `cargo run`, each of them a mistake someone has made:

- The **working directory is the project root**, so `.env`, `config/` and every relative path
  resolve the way they will in a deployment rather than the way they happen to from `src/`.
- The package is found the way cargo finds it, so this works from a subdirectory and says which
  package it picked.
- `--profile dev|test|production` sets **`MOSO_PROFILE`**, which is the variable the application
  actually reads and which `cargo run` has no idea exists. It is not cargo's `--profile`;
  `--release` is that one, and the `running` line names both, because "release" alone does not say
  whether `/docs` is mounted.
- The build happens first, so a compile error is not buried under a startup log.

**The exit code is forwarded**, which is the one exception to the four-code contract below: a
wrapper that flattened every application failure to 1 could not be used in the script that is the
reason to have a wrapper. A child killed by a signal reports `128 + signal`, so a Ctrl-C'd server
exits 130. A code of 0 alongside a failure is clamped to 1 — a wrapper must never turn a failure
into "everything went well".

Ctrl-C is not handled and does not need to be: the terminal delivers `SIGINT` to the whole
foreground group, so the application receives it at the same instant and drains on its own schedule.
`moso run` never signals or kills the child. The visible consequence is that the shell prompt
returns while the application is still draining.

## `moso build`

✅ **Implemented.** A release build by default — `--debug` is the opt-out, and there is deliberately
no `--release`, because a flag that was always on would mean nothing.

```
moso build                  # the binary, its path and its size
moso build --openapi        # and the contract, written beside the binary
moso build --openapi-out dist/openapi.json
moso build --debug          # cargo's dev profile, for a quick smoke test
```

`--openapi` calls the same code `moso openapi export` does rather than re-exporting; under `--json`
it drives it with a muted UI, because two JSON documents on one stdout is not JSON.

It prints the thing people forget: cargo's profile is not `MOSO_PROFILE`, and a release binary with
neither set boots as `dev` and mounts `/docs`.

## `moso generate`

🟡 **Six kinds are implemented**: `endpoint`, `schema`, `error`, `middleware`, `test` and
`workspace`. Each writes ordinary code the user owns from the moment it lands, and registers it in
`src/lib.rs` with the smallest edit that could work — one `pub mod`, and for an endpoint one
`.mount(..)` and one `.provide(..)`. Nothing is regenerated and nothing is overwritten without
`--force`. The kinds that are still absent (`resource`, `model`, `migration`, `job`, `policy`) each
need a battery or a database the generated code would have to name.

`generate workspace` is the one kind that takes no name, because it restructures the project rather
than adding to it: the package moves to `crates/<name>/` with `git mv` when the project is a
repository, the root becomes a virtual workspace with `members = ["crates/*"]`, and `[profile.*]` is
lifted to the root — where cargo actually honours it. The package **keeps its name**, so every
`use shop::…` in the binary and in `tests/` still resolves and `target/release/shop` is still where
the binary lands; the split is a file move, and nothing textual has to be right for the project to
go on compiling. Everything the *project* has one of — `.env`, `README.md`, `Dockerfile`,
`.cargo/config.toml` — stays at the root. Two manifest rewrites are performed and only two: the
profiles are lifted, and a relative `path = "…"` dependency is re-rooted by the two directories the
manifest just descended. It refuses rather than half-migrating: an existing `crates/`, a manifest
that is already a workspace root, or (in a git repository, without `--force`) a dirty working tree
each stop it before anything moves, and a move that fails part-way is undone in reverse.

The split leaves you standing in a *virtual* workspace root — a `Cargo.toml` with `[workspace]` and
no `[package]` — and discovery handles it: with one member, that member is the package, so `moso
routes` and everything else keep working from the root exactly as before. Once `crates/` holds
several packages there is a genuine choice to make, and the command says so and lists them rather
than picking one. `moso generate workspace` itself asks the question in the other order, checking
for an existing workspace root *before* discovery, so a second run says "already split" instead of
nesting the project inside itself.

### What it writes today

```
moso generate endpoint post          # src/posts.rs, mounted, with five routes
moso generate schema invoice         # the payload types alone
moso generate error billing          # an RFC 9457 error taxonomy
moso generate middleware observe     # a Layer/Service pair
moso generate test posts             # an end-to-end contract test
moso generate workspace              # split the crate (see 00-foundations/04)
```

`--singular <WORD>` corrects a plural the guesser gets wrong; `--dry-run` prints the plan;
`--force` overwrites. Every kind is compiled and its generated tests run by
`every_generated_kind_compiles_and_its_tests_pass`, so "it writes code" means "it writes code that
builds".

### ⛔ The five kinds that are not built

Design intent, and none of them is in `<KIND>`'s value list, so none of them can be typed:

```
moso generate resource Post title:string body:text published:bool author:ref:User
moso generate model Comment body:text post:ref:Post
moso generate migration add_locale_to_users
moso generate job SendDigest
moso generate policy Post
```

`generate resource` would write: the entity, `Create*`/`Update*`/`*Out` DTOs, a router with five
handlers, a policy stub, an admin registration, a migration, and an integration test — which is
exactly why it is not built: it names an admin panel that does not exist and a migration whose
columns come from a field-type shorthand nothing parses yet.

Field type shorthand, for when they are: `string`, `text`, `int`, `bigint`, `float`, `decimal`,
`bool`, `uuid`, `datetime`, `date`, `json`, `enum:A|B|C`, `ref:Entity`, `refs:Entity` (has_many),
with `?` for nullable, `!` for unique, `@` for indexed (`email:string!@`).

## `moso check`

✅ **Ten lints are implemented** — seven of the fourteen sketched below, plus three the sketch did
not have. This is the command the shipped `#[diagnostic::on_unimplemented]` messages and boot errors
point at when they end with ``run `moso check` ``, so that advice now goes somewhere.

Two sources, and the difference is stated in the command's own module header because it decides how
much a finding is worth:

- **The application's own answer.** `undocumented_endpoint`, `route_not_in_document`,
  `env_example_drift`, `missing_authz` and `unknown_permission` read `--dump-routes`,
  `--dump-openapi`, `--dump-env-example` and `--dump-authz`. These are exact: they are the assembled
  router and the generated document, not a guess about them.
- **A lexical scan of `src/**.rs`.**
  `layering`, `blocking_in_async`, `n_plus_one`, `stale_layer` and half of
  `unhandled_error_variant`. **It is not a `syn` parse.** The CLI depends on no Moso crate and on
  four third-party crates, and adding a parser to it is an ADR rather than a detail of one command.
  The scan blanks comments, string literals and char literals first — so a lint cannot fire on the
  doc comment that documents it — tracks braces to know which function and which loop a line is
  inside, and then matches tokens. It finds the shapes it claims to and will miss one spelled
  unusually.

| Lint | Default | Built | What it catches |
| --- | --- | --- | --- |
| `missing_authz` | warn | ✅ `--authz` | endpoints with no `#[requires]`, `Authorized<..>`, or `#[public]` |
| `unknown_permission` | deny | ✅ `--authz` | a permission named by a string the registry does not declare |
| `layering` | deny | ✅ | `routes/` importing SQL, `services/` importing `http`, `models/` importing either |
| `blocking_in_async` | deny | ✅ | `std::fs`, `std::thread::sleep`, `reqwest::blocking` in an async fn |
| `n_plus_one` | warn | ✅ | `.load(` or `.fetch_` inside a loop |
| `stale_layer` | warn | ✅ | `.layer()` or `.guard()` as the last call in a router fn |
| `unhandled_error_variant` | warn | ✅ 4xx only | a handler constructing a status its operation does not declare |
| `undocumented_endpoint` | warn | ✅ | a visible route registered without `#[endpoint]` |
| `route_not_in_document` | warn | ✅ | a visible route with no operation in the OpenAPI document |
| `env_example_drift` | warn | ✅ | a committed `.env.example` the `Config` type no longer generates |
| `openapi_drift` | deny | ➡ `moso openapi check` | committed `openapi.json` is stale |
| `unfiltered_mutation` | deny | ⛔ | `update_all()`/`delete()` with no filter |
| `missing_index` | warn | ⛔ | a `filter`/`order_by` on a column with no index in the snapshot |
| `schema_drift` | deny | ⛔ | `.schema.json` does not match the entities |
| `secret_in_log` | deny | ⛔ | a `#[schema(secret)]` field in a `tracing` macro |
| `side_effect_in_tx` | warn | ⛔ | HTTP or mail calls inside a `db.transaction` closure |
| `route_conflict` | deny | ⛔ unreachable | duplicate/shadowing routes |

The five ⛔ lints need an entity snapshot or a `#[derive(Schema)]` reflection the CLI has no access
to, and are absent rather than silently passing. `route_conflict` is different: `App::build()`
already detects conflicts, so a binary that answered `--dump-routes` at all has none, and a lint
that can never fire would be decoration. `openapi_drift` has a home already and is not reimplemented
here.

Three departures from the sketch above, each deliberate:

- **`unhandled_error_variant` lints 4xx only.** `Error::internal` is reachable from any handler that
  calls anything, so linting 5xx would fire nearly everywhere and say nothing. `errors = ..`
  declares the *contract*, and the contract is the 4xx a client is expected to handle.
- **Levels come from `[lints]` in `moso.toml`.** That table is documented in
  `00-foundations/04-project-structure.md` and was, until now, read by nothing.
  `31-authorization.md` promises `lints.missing_authz = "deny"` specifically; that works.
- **The exit code follows the level, not the count.** Exit 1 when a finding is at `deny`, so CI
  gates on what a team enforces rather than on every opinion the command holds. `--strict` promotes
  every warning.

`moso check --list` prints the catalogue. `--lint <NAME>` runs one. Output is rustc-shaped
(file:line, note, help) so editors parse it, and `--json` carries the same fields.
⛔ `moso check --fix` is not implemented.

## `moso middleware`

✅ Implemented. Runs the binary with `--dump-middleware`, and `--dump-routes` for the second half.

Two tables, because a Moso request passes through two stacks. The **global** one is
`MiddlewareStack`, printed outermost first with each slot's rendered summary. The **per-route** one
is whatever `.layer()` and `.guard()` attached to individual entries, printed outermost first — the
reverse of the order the dump carries them in, which is the order they were pushed.

Only the second answers the question people actually have. `.layer()` applies to the routes
registered *before* the call, so the chain has to be read positionally, and `--route <PATH>` is the
form that does it for you: one numbered list from outermost slot through the per-route layers to the
handler, with its guard count.

`--all` includes slots that are present but disabled — "`compression` is off" and "`compression` is
not in this stack" are different facts. `--json` carries the fields. A `--route` matching nothing
exits 1 rather than printing an empty stack.

The application sends the structured entries, not `MiddlewareStack::render()`'s text: `--json` needs
the fields, the per-route table interleaves data the stack does not carry, and a formatting change
must not require regenerating `src/dump.rs` in every project that already exists.

## `moso config`

✅ Implemented, in four modes.

```
moso config                            # every key, its value, and where it came from
moso config --env-example --out .env.example
moso config --check                    # in CI: a typo, a drift, a leaked secret
moso config --generate-secret          # 32 bytes from the OS CSPRNG, base64
```

`--generate-secret` is dispatched **before** the project is discovered, because it is entropy rather
than configuration: refusing to produce a key because the working directory is not a Cargo package
would be a rule with no reason behind it, and the first thing a new project needs is the secret that
goes in its `.env`. It is printed to standard output and nowhere else — `--out` is refused for it,
so the secret cannot be written into the repository by a slip of the shell.

`--check` reports the configuration mistakes that are *silent* — the ones that let the process start
and then behave as if you had never configured anything. Six findings, each with a stable slug so a
script can branch on the kind:

| Slug | Level | What it is |
| --- | --- | --- |
| `unread_environment_key` | fail | an environment key no field reads, with a "did you mean" |
| `unread_file_key` | fail | the same typo, in `config/default.toml` or the profile's file |
| `env_example_drift` | fail | the committed example is not what the `Config` type generates |
| `secret_in_tracked_file` | fail | a secret whose value came from a file git tracks |
| `secret_in_file` | warn | …from a file it does not |
| `env_example_missing` | warn | there is no committed example to compare against |

Exit 1 on any failure, 0 on warnings alone. The split is by whether a human had to decide: the first
four are facts, and "this looks like a file you would normally commit" is a judgement.

A key with no value and no default, or a value that fails its type, is reported **by delegation**.
`src/main.rs` builds the application before it answers a dump, so both of those stop `--dump-config`
itself; `--check` exits non-zero and lets the application's own boot report — which names the key,
its type, every environment spelling that would have supplied it, and the line to write — stand as
the explanation. It cannot report them from a *successful* resolution either, because
`FieldDescriptor::is_required()` is `default.is_none()`, so flagging a null origin would
false-positive on every `Option<T>` field.

`env_example_drift` appears here and as a `moso check` lint, and there is exactly one implementation
of the comparison — `config_check::example_drift`. Two commands that report the same named problem
and disagree about whether it is present would be worse than either of them not having the check,
because whichever one a team runs is the one they believe.

## `moso jobs`

✅ Implemented, over a `--dump-jobs` document the application answers.

```
moso jobs list                    # the registered job types
moso jobs status                  # depth, in flight, retrying, dead, oldest-ready latency
moso jobs schedules               # the cron table with each entry's next occurrence
moso jobs dlq --job send_welcome  # page through the dead letters
moso jobs retry   --job send_welcome --limit 50
moso jobs discard --queue mail    --limit 50 --yes
```

`status` prints latency beside depth deliberately: a queue of ten thousand that drains in a second
is healthy and a queue of four whose oldest job has waited an hour is not, and depth alone is the
number that gets watched and says least.

`retry` and `discard` change something, so they could have been a separate flag family like
`--db-*`. They are not, because the filter used to *look* at a page is the filter then acted on, and
one request document means the two cannot be spelled differently. What keeps them safe is the limit:
always sent, 50 by default, capped at 10,000, and `discard` asks before it runs unless `--yes`.

**There is no `moso worker` and there is not going to be one.** A worker links the application's job
bodies, so a CLI that ran one would have to link the user's crate — ADR-0004 says it cannot. It is
also a process with its own lifecycle: concurrency, lease duration, drain mode and queue weights are
deployment decisions, and hiding them behind a subcommand would mean re-exposing every one as a
flag. `Worker::run` in the application's own binary is shorter than the flags would be.

## `moso authz`

✅ Implemented, over a `--dump-authz` document.

```
moso authz permissions [--group NAME]   # the registry, with its fingerprint
moso authz roles                        # each role and what it grants — no --group here
moso authz explain --actor usr_1 --action publish --resource Post#7 [--scope KEY]
```

`explain` is the offline entry point `Explanation::render` never had — and offline is how the
question usually arrives, because "why can't Alice publish" is a support ticket rather than a
request you can re-issue with an extra header.

The block is printed **verbatim**. The format is `Explanation::render`'s, snapshot tested in
`moso-authz`; a second renderer here would be a second thing to keep in step whose first divergence
nobody would notice. `--json` carries the structured `Explanation`.

**`explain` is refused in the production profile** unless `--allow-production` is passed, mirroring
the `X-Moso-Authz-Explain` header, which is honoured in a development profile and nowhere else. The
refusal is enforced *in the application*, because that is the half that knows its own profile and a
check living only in the CLI is a check an older CLI does not have. `moso new` writes it into the
stub, ahead of anything that could assemble a trace.

`moso check --authz` is the third question in this family and lives with the other lints.

## `moso auth`

✅ Implemented, over a `--dump-auth` document. One subcommand, and it will stay small: everything
else about authentication is a decision the composition root makes, and a `moso auth create-user`
would be inventing a user model for somebody who already has one.

```
moso auth calibrate [--target-ms MS]    # 250 ms by default, 50..=2000 accepted
```

```
$ moso auth calibrate
  ✓ one hash takes 243 ms here      (target 250 ms)

  PARAMETER    VALUE   NOTE
  memory_kib   65536   64 MiB, 3.4× OWASP's minimum
  iterations   3
  parallelism  1

  paste into .env, or your platform's configuration:

    SHOP__HASH_MEMORY_KIB=65536
    SHOP__HASH_ITERATIONS=3
    SHOP__HASH_PARALLELISM=1

  or, where you build the AuthConfig:

    config.hash_params = Some(HashParams::new(65536, 3, 1));

      measured on this machine; run it on the one that will serve logins
```

Three things about it are load-bearing:

- **It runs the application binary** rather than answering from a table. Argon2id's cost is a
  property of the hardware the hash will run on: parameters that take 250 ms on a laptop take three
  times that in a container with half a CPU, so a constant compiled into the CLI would be wrong on
  every machine but one. `moso_auth::calibrate` is called *inside the process that will do the
  hashing*, which is the only place the answer means anything.
- **It refuses to print a downgrade.** Anything below `HashParams::OWASP_MINIMUM` is exit 1 naming
  every dimension that fell short, because a calibration that recommends weaker parameters is worse
  than none: it is a plausible instruction to make an application less safe, with a tool's authority
  behind it.
- **The floor travels with the answer.** OWASP's minimum has one home, `HashParams::OWASP_MINIMUM`,
  and the CLI depends on no Moso crate — so `src/dump.rs` reports the floor it read from that
  constant and the CLI checks against it, rather than keeping a second copy of three numbers that
  could drift. The `config` lines are the application's own keys for the same reason: only it knows
  what it calls them.

## The three dumps that carry a request

`--dump-jobs`, `--dump-authz` and `--dump-auth` take one JSON **request document** as the next
argument, which the first five `--dump-*` flags do not. The first five are pure functions of an
application that has already been built; these three carry parameters (`--job`, `--limit`,
`--actor`, `--action`, `--target-ms`), one of them mutates, and one of them measures. A request
document rather than a flag per parameter means adding a filter is a field, and the two halves
cannot drift over argument order.

All three are answered by `src/dump.rs` even in a project that uses none of the batteries, with
`{"available": false, "reason": .., "help": ..}`. That is not pretence, it is the alternative to a
hang: a flag `main` does not recognise falls through to `serve()`, and the command would sit there
until its timeout and then report a stuck binary rather than a battery nobody wired. `available` is
the one field the CLI branches on, and a `false` is exit 1 with the sentence naming what to add —
never an empty table, which reads as an answer.

`dump::run` is `async` for these three: a queue's depth, a role source and an argon2 measurement all
happen *now* rather than being facts about an already-assembled application.

## `moso doctor`

✅ Implemented, over the checks this build can make: toolchain version and MSRV, `cargo`,
`cargo-nextest`, the linker, the workspace layout, whether the project depends on `moso`, and the
configuration. ⛔ The Docker, `DATABASE_URL` and migration checks in the example below need a
database layer. Exit code 3 on any failed check, as specified.

```
$ moso doctor
  ✓ rustc 1.97.1                    (MSRV 1.94 satisfied)
  ✓ cargo-nextest 0.9.90
  ✗ linker                          using the default; `mold` would save ~40% of link time
                                    → brew install mold, then add it to ~/.cargo/config.toml
  ⚠ cranelift backend               not installed (dev builds could be ~2× faster)
                                    → rustup component add rustc-codegen-cranelift-preview
  ✓ docker                          running; postgres:17 healthy on :5432
  ✓ DATABASE_URL                    reachable, 12 migrations applied
  ✗ .env                            missing SHOP__SECRET_KEY (required)
                                    → moso config --generate-secret
  ✓ disk                            41 GB free (target/ is 3.2 GB — `cargo clean` to reclaim)
```

`doctor` is the first thing support asks a user to run, so it must be thorough and every fix line it
prints must be a command that works. It has no `--fix` of its own: there is no `moso doctor
--fix-config`, and the linker advice deliberately points at the contributor's own
`~/.cargo/config.toml` rather than the repository's, because cargo config has no conditionals and a
`-fuse-ld=` naming a linker a stranger has not installed is a hard error on their machine.

## `moso client`

✅ **Implemented** for TypeScript and Rust, from a document read either from the application
(`--dump-openapi`) or from a file.

```
moso client --out ../web/src/api                 # TypeScript, from your app
moso client --lang rust --out ../sdk/src/api     # Rust, transport-agnostic
moso client --input openapi.json --out src/api   # from a committed document, no Rust project needed
moso client --out ../web/src/api --check         # in CI
```

`--input` skips project discovery entirely, so the command works in a front-end repository that has
only the committed document.

**TypeScript** has zero dependencies — `fetch`, `Headers`, `URLSearchParams` — and is
erasable-syntax only (no `enum`, no `namespace`), so it passes esbuild, swc and
`node --experimental-strip-types` untouched. Every method resolves to `ApiResult<T, P>`, where `P`
is the union of the schemas *that operation* declares for its error statuses, so branching on
`problem.type` needs no cast.

**Rust is transport-agnostic** rather than `reqwest`-shaped, which is the one decision in this
command worth arguing. Naming an HTTP crate also names a TLS stack, and Moso has an opinion there
(rustls, never OpenSSL) that a generator has no business imposing on somebody else's binary; a
program that already has a configured client wants *that* one. The cost is one ~15-line
`impl Transport`, written out in the generated `mod.rs`.

**Output is deterministic** — same document, byte-identical files — so it is meant to be committed
and gated. `--check` regenerates into memory, compares byte for byte, names the stale files and
exits 1; files it does not produce are left alone.

Nothing is ever silently dropped. A construct that cannot be represented becomes an opaque type
carrying the reason where the reader will meet it (`unknown /* … prefixItems … */`,
`serde_json::Value /* … */`); a construct that is only partly carried is reported by the command
*and* repeated in the generated file's header. A non-3.x document is a user error, not a guess.

## `moso test`

🟡 **Implemented as a two-pass runner. The managed database is not built.**

```
moso test                   # nextest when installed, cargo test otherwise
moso test users             # only tests whose name contains `users`
moso test --workspace       # after `moso generate workspace`
moso test -- --nocapture
```

Two passes, always. Pass one is `cargo nextest run`, or `cargo test --all-targets` when nextest is
absent — `--all-targets` is every target *except* doctests, so both runners cover the same ground
with no double run. Pass two is always `cargo test --doc`, because no runner but `cargo test` can
run doctests. The command says which runner it used and at what version.

**The headline is the skip trap.** `DATABASE_URL` and `REDIS_URL` are reported before the run and
again after it, as a warning, because "skipped" is what gets mistaken for "passed". The asymmetric
case gets its own wording: exporting one of the two produces a green run in which the other suite
silently skipped. The fix line names `./scripts/test-db.sh up` only when the checkout actually has
that script.

⛔ Not built: the template database and per-test clone of `43-testing.md`, `--coverage`, `--watch`,
and `--ui`. Each needs a database front end this build does not have, and inventing one here would
fork that document's strategy into the CLI.

## `moso deploy`

🟡 **`moso deploy checklist` is implemented. It is the only subcommand, and deliberately so.**

Moso is not a PaaS. This command tree exists so that the one useful thing in the family — a
pre-production audit — has a home, and it must never grow something that pushes an artefact
anywhere. It writes nothing, uploads nothing and connects to nothing.

```
moso deploy checklist               # exits non-zero on any failed check
moso deploy checklist --strict      # …and on any warning
```

It reads the configuration the application resolves under `MOSO_PROFILE=production` — which is what
`Project::dump_with_env` exists for, since auditing development values before a production
deployment answers a question nobody asked — plus the project on disk. Two sources, and the
difference is worth knowing when a finding surprises you: `HttpConfig` and `ServerConfig` are handed
to the builder in code rather than resolved through the configuration stack, so `--dump-config`
never sees them and they are read from `src/**/*.rs` line by line, with every finding naming the
file and line so the reader confirms rather than trusts.

**A check that cannot be answered says so and is reported as informational.** It never guesses: a
checklist that invents a ✓ is worse than no checklist, because it is the thing that gets trusted at
2 a.m.

⛔ `deploy dockerfile`, `compose`, `k8s` and the provider targets are not implemented and are absent
from the tree.

## Design rules for the CLI itself

- **Every command works offline** except `moso self update --check`, which is the only network
  access this binary ever makes and happens only when that flag is given. ✅
- **Every destructive command asks**, unless `--yes`. ✅ — `moso new` over a non-empty directory,
  `moso jobs discard`, `moso db squash`, and `moso generate --force`. A non-terminal stdin without
  `--yes` is a usage error rather than a silent yes. `db reset` and `db drop` do not exist.
- **Output is human-first, machine-optional.** ✅ — every command that produces data honours
  `--json`, and when it is on, nothing but the document reaches stdout. The three that produce a
  child's stream or a shell script instead say so in their `--help`.
- **Exit codes are meaningful**: 0 ok, 1 user error, 2 usage error, 3 environment problem. ✅ — with
  one documented exception, `moso run`, which forwards the application's own code.
- **No telemetry, opt-in or otherwise.** ✅ — nothing is collected and there is no first-run prompt
  to decline. If that ever changes it is an ADR.
- Shell completions via `moso self completions`. ✅ — bash, zsh, fish, elvish and powershell, which
  is what `clap_complete` supports. Nushell is ⛔.
- Respects `NO_COLOR`, `--quiet`, `--verbose`, and non-TTY output. ✅

## Acceptance criteria (WP-23)

| # | Criterion | State |
| --- | --- | --- |
| 1 | `moso new` → `moso dev` → browsable `/docs` in under 60 s warm | 🟡 the loop works; the 60 s budget is unmeasured |
| 2 | A compile error keeps the old process serving | ✅ |
| 2b | `moso dev` restart replays queued requests | ⛔ not built |
| 3 | `moso generate` output compiles and its generated test passes | ✅ for the six kinds that exist, proven by `every_generated_kind_compiles_and_its_tests_pass` |
| 4 | Every `check` lint has a positive and a negative fixture | 🟡 each of the ten has unit coverage; a fixture project per lint is not built |
| 5 | `doctor` detects each listed condition | 🟡 for the checks this build can make; the Docker and database ones need a database layer |
| 6 | Generated Dockerfile builds an image < 60 MB | ⛔ unmeasured |
| 7 | All commands respond to `--help` with examples; completions install cleanly | ✅ proven by `completions_are_produced_for_every_shell` and the command-tree tests in `cli.rs` |
| 8 | `moso new --auth` produces working flows, not a sketch | ✅ proven by `an_auth_project_builds_its_flows_pass_and_calibration_measures_it`, which compiles the project and runs its own end-to-end tests |

The `#[ignore]`d tests in `crates/moso-cli/tests/new_builds.rs` are what make criteria 3, 7 and 8
more than a claim: they scaffold a project with `moso new`, compile it, and drive the real commands
against the real binary. Run them with `cargo test -p moso-cli -- --ignored`.
