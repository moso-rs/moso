---
title: Quick start
description: From an empty directory to a running, validated, self-documenting HTTP endpoint, with every command and every file shown in full.
order: 3
---

Ten minutes, most of which is the first compile. At the end you will have an API that validates its
input, rejects bad requests with a machine-readable problem document pointing at the offending
field, serves its own OpenAPI 3.1 document, and drains in-flight requests on `SIGTERM`. You will not
have written an OpenAPI annotation, a validation call, or a line of shutdown handling.

Follow it literally. Every command and every output on this page was run against the tree this site
documents.

## Before you start

- A stable Rust toolchain, 1.90 or newer. Check with `rustc --version`.
- A Moso checkout and the `moso` CLI installed from it. [Installation](./installation.md) covers
  both in about two minutes.
- `curl`, or any HTTP client you prefer.

You do **not** need Postgres, Redis, Docker, or a network connection after the first build. The
application in this quick start has no external dependencies at all.

Throughout, `/absolute/path/to/moso` means wherever you cloned the repository.

## 1. Create the project

```bash
moso new shop --yes --no-git --moso-path /absolute/path/to/moso/crates/moso
```

```text
  ✓ created shop/                   (12 files)
  ✓ wrote .cargo/config.toml        (build settings; `moso doctor` explains them)
  ✓ wrote .env.example              (SHOP__GREETING)

  next:
    cd shop
    cargo test
    cargo run

  then open http://localhost:3000/
```

> [!WARNING]
> `--moso-path` is required today, because Moso is unreleased and `moso = "0.1"` does not resolve
> from crates.io. Point it at `<checkout>/crates/moso`, not at the checkout root: the root is a
> virtual workspace manifest and Cargo will say so. The path is written verbatim into the generated
> `Cargo.toml`, so use an absolute one. Drop `--no-git` if you want `moso new` to run `git init` and
> make a first commit.

```bash
cd shop
```

## 2. What you got

Twelve files, all of them plain Rust or plain TOML. Nothing is hidden in a framework directory.

```text
shop/
├── Cargo.toml
├── .cargo/config.toml      build settings, all commented, safe to delete
├── .env.example            generated from your Config type
├── .gitignore
├── Dockerfile              multi-stage, builds a single deployable image
├── .dockerignore
├── README.md
├── src/
│   ├── lib.rs              the composition root: what the application IS
│   ├── main.rs             four lines over the library
│   ├── routes.rs           the payload types, the handlers, the route table
│   └── dump.rs             how the moso CLI asks this binary questions
└── tests/
    └── api.rs              five integration tests against the real application
```

The manifest names two dependencies:

```toml title="Cargo.toml"
[package]
name = "shop"
version = "0.1.0"
edition = "2024"
rust-version = "1.90"
publish = false

[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso" }
# Moso does not pick your runtime for you: `#[tokio::main]` is written in your
# `main`, in your crate, with a version you control.
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }

# Your own code stays unoptimised so it compiles fast; everything you depend on
# is optimised, because it is compiled once and then runs on every request.
[profile.dev.package."*"]
opt-level = 2
```

`src/main.rs` is the whole entry point:

```rust title="src/main.rs"
use shop::{build, dump};

#[tokio::main]
async fn main() -> moso::Result<()> {
    // `moso routes`, `moso openapi export` and `moso config` run this binary
    // with a `--dump-*` flag and read one document off stdout.
    if let Some(requested) = dump::requested() {
        return dump::run(requested, &build()?);
    }

    // Binds the address from configuration, installs signal handlers, runs the
    // startup hooks, serves, and drains in-flight requests on SIGTERM.
    build()?.serve().await
}
```

The file worth reading closely is `src/routes.rs`. It is the whole HTTP surface, and it is short
enough to quote in full:

```rust title="src/routes.rs"
//! The HTTP surface.
//!
//! One type definition per payload drives four things at once: deserialisation,
//! validation, the OpenAPI schema, and the JSON Pointer in the 422 a client
//! gets when it sends something wrong. There is no second place to keep in sync.

use moso::prelude::*;

/// What `POST /greetings` accepts.
#[derive(Schema, Debug, Clone)]
pub struct NewGreeting {
    /// Who to greet.
    ///
    /// `len` is one attribute and it produces two things: the runtime check
    /// that rejects an empty name with `422` and `/name`, and the
    /// `minLength`/`maxLength` in the published schema. They cannot disagree.
    #[schema(len = 1..=64)]
    pub name: String,
}

/// What the greeting endpoints return.
#[derive(Schema, Debug, Clone)]
pub struct Greeting {
    /// The rendered message.
    pub message: String,
}

/// Greet the world.
///
/// The first line of this doc comment becomes the operation's `summary`, and
/// the rest becomes its `description`. Writing the documentation *is* writing
/// the code.
#[endpoint]
async fn hello(Inject(config): Inject<crate::AppConfig>) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, world", config.greeting),
    }))
}

/// Greet someone by name.
///
/// `Json<NewGreeting>` cannot exist unless the body parsed *and* validated, so
/// this function never has to check its own input.
#[endpoint]
async fn greet(
    Inject(config): Inject<crate::AppConfig>,
    Json(body): Json<NewGreeting>,
) -> Result<Created<Greeting>> {
    Ok(Created::at(
        "/greetings",
        Greeting {
            message: format!("{}, {}", config.greeting, body.name),
        },
    ))
}

/// Every route this application serves.
///
/// `routes!` rewrites each name to the operation type `#[endpoint]` generated,
/// which is how the documentation travels with the handler instead of being
/// repeated here.
pub fn router() -> Router {
    moso::routes! {
        GET  "/"          => hello,
        POST "/greetings" => greet,
    }
    .tag("greetings")
}
```

`src/lib.rs` is the composition root. With its explanatory comments trimmed, this is all of it:

```rust title="src/lib.rs"
pub mod dump;
pub mod routes;

use moso::prelude::*;

/// The prefix every environment variable for this application carries.
pub const ENV_PREFIX: &str = "SHOP";

/// Everything shop reads from its environment.
#[derive(Config, Debug, Clone)]
pub struct AppConfig {
    /// The greeting `GET /` returns.
    #[config(default = "hello")]
    pub greeting: String,

    /// The address to listen on.
    #[config(default = "0.0.0.0:3000")]
    pub bind: std::net::SocketAddr,
}

/// The configuration stack, in precedence order.
pub fn loader() -> Result<moso::config::ConfigLoader> {
    Ok(moso::config::ConfigLoader::standard()?.with_prefix(ENV_PREFIX))
}

/// Assemble the application: configuration, providers, routes.
pub fn build() -> Result<App> {
    let config = AppConfig::load_from(&loader()?)?;

    // Read what the server needs before the configuration moves into the
    // builder, where it becomes a provider handlers reach with `Inject`.
    let bind = config.bind;

    App::new(config)
        .server_config(moso::http_config::ServerConfig {
            bind,
            ..Default::default()
        })
        .mount(routes::router())
        .build()
}
```

The lib/bin split is not a style preference. Because everything real lives in a library function,
`tests/api.rs` boots the identical application that `main` boots, with the same providers, the same
middleware stack and the same document. [Project layout](./project-layout.md) goes further into
this, and covers the four files not quoted here.

## 3. Run the tests

```bash
cargo test
```

The first build compiles Moso and its dependencies and takes a couple of minutes on a cold cache.
Then:

```text
running 5 tests
test every_route_is_documented ... ok
test a_greeting_is_created ... ok
test an_invalid_body_is_rejected_with_a_pointer_to_the_field ... ok
test an_unknown_path_is_a_problem_document ... ok
test the_root_greets_the_world ... ok

test result: ok. 5 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Those tests drive `App::into_service()`, which is the composed Tower service: the real router, the
real middleware stack, the real dependency graph, without binding a port. One of them,
`every_route_is_documented`, asserts that no route was registered without an `#[endpoint]`
description, which is the kind of contract test that is cheap to keep and expensive to add later.

## 4. Start it

```bash
cargo run
```

The process prints nothing and does not return. That is correct: the generated project installs no
tracing subscriber, so the framework's boot line has nowhere to go.
[Observability](../guides/observability.md) shows how to add one when you want logs.

It is now listening on `0.0.0.0:3000`. Leave it running and open a second terminal.

## 5. Call it

```bash
curl -i localhost:3000/
```

```text
HTTP/1.1 200 OK
content-type: application/json
content-length: 26
x-content-type-options: nosniff
referrer-policy: strict-origin-when-cross-origin
content-security-policy: frame-ancestors 'none'
x-frame-options: DENY
x-request-id: 01KYZA37YNCVJ3Q4F1PVET5SR3

{"message":"hello, world"}
```

Four security headers and a correlation id you did not ask for. They come from the default
middleware stack, which is on unless you replace it. See
[security](../guides/security.md) and [middleware](../guides/middleware.md).

Now the endpoint that takes a body:

```bash
curl -i -X POST localhost:3000/greetings \
  -H 'content-type: application/json' \
  -d '{"name":"ada"}'
```

```text
HTTP/1.1 201 Created
content-type: application/json
location: /greetings
content-length: 24

{"message":"hello, ada"}
```

The `201` and the `Location` header came from the return type, `Created<Greeting>`. Nothing set them
by hand, and the same type put them in the document.

## 6. Break it on purpose

Send a name that violates `#[schema(len = 1..=64)]`:

```bash
curl -i -X POST localhost:3000/greetings \
  -H 'content-type: application/json' \
  -d '{"name":""}'
```

```text
HTTP/1.1 422 Unprocessable Entity
content-type: application/problem+json
content-length: 308
```

```json
{
  "type": "https://moso.rs/errors/validation",
  "title": "Validation Failed",
  "status": 422,
  "detail": "1 field failed validation",
  "instance": "/greetings",
  "errors": [
    {
      "pointer": "/name",
      "code": "len",
      "message": "must be between 1 and 64 characters",
      "params": { "max": 64, "min": 1 }
    }
  ],
  "request_id": "01KYZA3DSV217WXTRMRVRN6BZZ"
}
```

`greet` never ran. Extraction reads the body under a byte cap, deserialises it, validates it, and
only then calls the handler, so there is no code path that hands a handler an invalid value.

The `pointer` is an RFC 6901 JSON Pointer, so a client can map the failure to a form field without
parsing English. `params` carries the numbers, so a client can render its own message. `request_id`
matches the `x-request-id` header, so the failure is greppable in a log.

Three other failures, each with its own status, so a client can tell them apart.

A body that is not the right shape at all:

```bash
curl -s -X POST localhost:3000/greetings \
  -H 'content-type: application/json' -d '{"nam":"ada"}'
```

```json
{
  "type": "https://moso.rs/errors/bad-request",
  "title": "Bad Request",
  "status": 400,
  "detail": "missing field `name`",
  "instance": "/greetings",
  "errors": [
    { "pointer": "/name", "code": "required", "message": "missing field `name`" }
  ],
  "request_id": "01KYZA8A5T9ZXWS8JT50CX1TXW"
}
```

`400`, not `422`. A body that could not become a `NewGreeting` at all is a deserialisation failure;
a body that parsed and then broke a rule is a validation failure. The distinction lets a client tell
"my serialiser is wrong" from "my data is wrong".

A body in a media type the operation does not accept:

```bash
curl -s -X POST localhost:3000/greetings -d 'name=ada'
```

```json
{
  "type": "https://moso.rs/errors/unsupported-media",
  "title": "Unsupported Media Type",
  "status": 415,
  "detail": "This operation does not accept `application/x-www-form-urlencoded`",
  "instance": "/greetings",
  "request_id": "01KYZASV8WXBZMSWFM1YW997H9"
}
```

And a path nothing serves:

```bash
curl -s localhost:3000/nope
```

```json
{"type":"about:blank","title":"Not Found","status":404,"detail":"no route matches /nope"}
```

Every one of those is `application/problem+json` under RFC 9457. The full taxonomy is in
[errors](../guides/errors.md).

## 7. Read the documentation it wrote

Open **http://localhost:3000/docs** in a browser. That is a rendered, interactive API reference,
served from the binary with no network access and no JavaScript build step.

The raw document is at `/openapi.json`:

```bash
curl -s localhost:3000/openapi.json | head -c 400
```

```json
{"openapi":"3.1.1","info":{"title":"API","version":"0.0.0"},"jsonSchemaDialect":"https://json-schema.org/draft/2020-12/schema","paths":{"/":{"get":{"tags":["greetings"],"summary":"Greet the world.","description":"The first line of this doc comment becomes the operation's `summary`, and\nthe rest becomes its `description`.
```

The title is `API` and the version is `0.0.0` because the generated project has not declared them.
Add them where the rest of the application is declared, in `build()`:

```rust title="src/lib.rs"
App::new(config)
    .server_config(moso::http_config::ServerConfig { bind, ..Default::default() })
    .mount(routes::router())
    .openapi(|document| {
        document.title("Shop API").version(env!("CARGO_PKG_VERSION"));
    })
    .build()
```

Two probes are mounted for you and are not in the document, because a liveness probe is not part of
your API:

```bash
curl -s localhost:3000/healthz
curl -s localhost:3000/readyz
```

```json
{"status":"up"}
{"status":"up","checks":{},"version":"0.1.0","uptime_s":7}
```

`/healthz` touches nothing, so a database blip cannot turn into a rolling restart. `/readyz` runs
every readiness check you registered and flips to `503` the moment shutdown begins.
[Health and shutdown](../guides/health-and-shutdown.md) covers both.

## 8. Change one attribute, watch four things move

This is the claim the framework is built on, and it takes one line to test. Stop the server with
`Ctrl-C` and tighten the constraint on `name`:

```rust title="src/routes.rs"
    #[schema(len = 1..=64, trim, pattern = r"^[A-Za-z ]+$")]
    pub name: String,
```

```bash
cargo run
```

The runtime check moved, and the reported `code` changed with it:

```bash
curl -s -X POST localhost:3000/greetings \
  -H 'content-type: application/json' -d '{"name":"ada99"}'
```

```json
{
  "type": "https://moso.rs/errors/validation",
  "title": "Validation Failed",
  "status": 422,
  "detail": "1 field failed validation",
  "instance": "/greetings",
  "errors": [
    {
      "pointer": "/name",
      "code": "pattern",
      "message": "must match ^[A-Za-z ]+$",
      "params": { "pattern": "^[A-Za-z ]+$" }
    }
  ],
  "request_id": "01KYZASV7D49BEQ12B5ZS1SSDJ"
}
```

The published schema moved with it, including a sentence about `trim` appended to the description:

```bash
curl -s localhost:3000/openapi.json | python3 -c \
  "import json,sys; print(json.dumps(json.load(sys.stdin)['components']['schemas']['NewGreeting'], indent=2))"
```

```json
{
  "type": "object",
  "description": "What `POST /greetings` accepts.",
  "properties": {
    "name": {
      "type": "string",
      "description": "Who to greet.\n\n...\n\nThe value is trimmed of leading and trailing whitespace when it is received.",
      "minLength": 1,
      "maxLength": 64,
      "pattern": "^[A-Za-z ]+$"
    }
  },
  "required": ["name"]
}
```

And the behaviour moved: `trim` runs before validation, so a padded name is accepted and arrives
trimmed.

```bash
curl -s -X POST localhost:3000/greetings \
  -H 'content-type: application/json' -d '{"name":"  ada  "}'
```

```json
{"message":"hello, ada"}
```

One attribute, four outputs, from one parse. There is nowhere for them to drift apart. The full
vocabulary is in [schemas](../guides/schemas.md) and [validation](../guides/validation.md).

## 9. Add an endpoint

Add a handler to `src/routes.rs`, above `router()`:

```rust title="src/routes.rs"
/// Greet someone named in the path.
///
/// `Path<String>` names the `{name}` capture, and the OpenAPI parameter comes
/// from the same declaration.
#[endpoint]
async fn greet_path(
    Inject(config): Inject<crate::AppConfig>,
    Path(name): Path<String>,
) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, {name}", config.greeting),
    }))
}
```

Then add one row to the table:

```rust title="src/routes.rs"
pub fn router() -> Router {
    moso::routes! {
        GET  "/"                 => hello,
        POST "/greetings"        => greet,
        GET  "/greetings/{name}" => greet_path,
    }
    .tag("greetings")
}
```

```bash
cargo run
curl -s localhost:3000/greetings/ada
```

```json
{"message":"hello, ada"}
```

The path uses `{name}`, not `:name`. The old Axum and Actix spelling is a compile error at the
literal, with a help line repeating your own path corrected. Everything else about path templates,
including nesting, catch-alls and conflict detection, is in [routing](../guides/routing.md).

## 10. Ask the CLI what it built

These commands build and run your binary themselves, so stop the server first if it holds the port.
In the project directory:

```bash
moso routes
```

```text
METHOD  PATH               HANDLER     AUTH  TAGS       SOURCE
GET     /                  hello       -     greetings  src/routes.rs:34
POST    /greetings         greet       -     greetings  src/routes.rs:45
GET     /greetings/{name}  greet_path  -     greetings  src/routes.rs:62
```

That is not parsed from your source. The CLI runs your binary with `--dump-routes` and reads one
JSON document back, which is why a route registered inside a loop, a `nest`, or a function in a
dependency still shows up. `src/dump.rs` in your project is the code that answers, and it is yours
to change.

```bash
moso config
```

```text
  profile: dev

KEY       ENVIRONMENT     VALUE           FROM
greeting  SHOP__GREETING  "hello"         default
bind      SHOP__BIND      "0.0.0.0:3000"  default
```

One row per configuration key, with the value that won and where it came from. Run it again with an
override to watch the `FROM` column change:

```bash
SHOP__GREETING=hei moso config
```

Commit the document so API changes become reviewable diffs:

```bash
moso openapi export --out openapi.json
moso openapi check
```

```text
  ✓ wrote openapi.json              (2 operations)
  ✓ openapi.json is up to date      (2 operations)
```

`moso openapi check` compares parsed JSON rather than bytes, so reindenting the file is not a
failure and a change in meaning is. It exits `1` when they differ and prints an RFC 6901 pointer for
each difference, which makes it a one-line CI gate against silent contract drift. See
[OpenAPI](../guides/openapi.md).

Every command takes `--json`, which puts exactly one document on stdout and moves prose to stderr,
so a script can consume it:

```bash
moso routes --json | jq '.routes[] | select(.security == [])'
```

For the edit loop, `moso dev` rebuilds and restarts on every save. It builds *before* it stops the
old server, so a broken intermediate edit costs you the compiler message and nothing else:

```bash
moso dev
```

## 11. Stop it

Press `Ctrl-C` in the terminal running the server, or send it `SIGTERM`.

What happens, in order: `/readyz` starts answering `503` within milliseconds so a load balancer
takes the instance out of rotation, while `/healthz` keeps answering `200` so an orchestrator does
not kill the process mid-drain. In-flight requests are allowed to finish. A shutdown grace of 25
seconds bounds the whole thing, deliberately under the 30 seconds an orchestrator typically allows
before `SIGKILL`. Then shutdown hooks run in reverse registration order and the process exits.

None of that needed code.

## What you built

An HTTP service that parses and validates its input from one type definition, rejects bad requests
with RFC 9457 problem documents carrying JSON Pointers, serves a correct OpenAPI 3.1 document and an
interactive reference, exposes liveness and readiness probes, sets security headers, correlates
requests with an id, and shuts down gracefully. It has five tests. It depends on no external
service.

Every one of those came from a type in a signature or from the default stack, not from a
configuration file you had to learn.

## Doing it without the CLI

If you would rather see every line, this is the same thing in two files. It compiles and serves
exactly as above.

```toml title="Cargo.toml"
[package]
name = "hand"
version = "0.1.0"
edition = "2024"
rust-version = "1.90"

[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

```rust title="src/main.rs"
//! A single-file Moso application.

use moso::prelude::*;

/// What `POST /greetings` accepts.
#[derive(Schema, Debug)]
pub struct NewGreeting {
    /// Who to greet.
    #[schema(len = 1..=64)]
    pub name: String,
}

/// What the greeting endpoints return.
#[derive(Schema, Debug)]
pub struct Greeting {
    /// The rendered message.
    pub message: String,
}

/// Greet someone by name.
#[endpoint]
async fn greet(Json(body): Json<NewGreeting>) -> Result<Created<Greeting>> {
    Ok(Created::at("/greetings", Greeting {
        message: format!("hello, {}", body.name),
    }))
}

/// Everything this application reads from its environment.
#[derive(Config, Debug)]
pub struct AppConfig {
    /// The address to listen on.
    #[config(default = "0.0.0.0:3000")]
    pub bind: std::net::SocketAddr,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = AppConfig::load()?;
    let bind = config.bind;

    App::new(config)
        .server_config(moso::http_config::ServerConfig { bind, ..Default::default() })
        .mount(moso::routes! { POST "/greetings" => greet })
        .openapi(|d| { d.title("Greeting API").version("0.1.0"); })
        .build()?
        .serve()
        .await
}
```

```bash
cargo run
curl -s -X POST localhost:3000/greetings \
  -H 'content-type: application/json' -d '{"name":"ada"}'
```

`AppConfig::load()` here uses no environment prefix, so the address comes from `BIND` rather than
`SHOP__BIND`. Note the shape of `main`: read what the listener needs out of the configuration
**before** the value moves into `App::new`, where it becomes a provider that handlers reach with
`Inject<AppConfig>`. Moso does not read a bind address out of your config struct by magic, because
it does not know what you called the field.

## When it does not work

**`failed to select a version for the requirement moso = "^0.1"`.** You skipped `--moso-path`, so
Cargo is looking on crates.io. Edit the `moso` line in `Cargo.toml` to point at your checkout.

**`found a virtual manifest at .../Cargo.toml instead of a package manifest`.** `--moso-path`
pointed at the checkout root. It wants `<checkout>/crates/moso`.

**`error: application failed to build (N problems)`.** That is the boot report, and it is the good
failure. Read the `fix` block under each problem; each one names a file and line in your code.

**`Address already in use`.** Something owns port 3000. Change it without touching the source:
`SHOP__BIND=127.0.0.1:8080 cargo run`.

**`cannot find type __moso_op_greet in this scope`.** A name in the `routes!` table does not match a
function carrying `#[endpoint]`. The underline is on the name you wrote.

**`moso routes` waits and then reports that the binary ignored the flag.** Your `main` calls
`serve()` before checking `dump::requested()`. The flag check has to come first.

**The server prints nothing at all.** Expected. There is no tracing subscriber in the generated
project.

## Where to go next

- [Project layout](./project-layout.md) explains every file you just generated, the composition root
  pattern, and when to split into a workspace.
- [Schemas](../guides/schemas.md) and [validation](../guides/validation.md) are the two guides that
  pay off fastest, because the attribute vocabulary is where most of the leverage is.
- [Migrations](../guides/migrations.md) when you are ready for persistence.
