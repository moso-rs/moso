# `examples/minimal` - the whole loop, in 31 lines

One `#[derive(Schema)]` type, one `#[endpoint]`, one `routes!` table, one `App`. It is the smallest
program that exercises every layer of Moso end to end: extraction, dependency injection, the router,
the error format, and the OpenAPI document - which is generated from those same 31 lines, with no
annotations of its own.

```
cargo run -p example-minimal
```

```
$ curl -s localhost:3000/hello/ada
{"name":"ada","message":"Hello, ada!"}

$ curl -s localhost:3000/openapi.json | jq '.paths'
{"/hello/{name}": {"get": {"summary": "Greet someone by name.", "operationId": "hello", …}}}

$ open http://localhost:3000/docs
```

```
cargo test -p example-minimal
```

## The files

```
examples/minimal/
├── Cargo.toml         # two dependencies: moso, tokio - plus moso-test for the tests
├── src/
│   ├── lib.rs         # the application - everything below is about this file
│   └── main.rs        # a five-line shim over it
└── tests/
    └── hello.rs       # the app, driven over HTTP
```

---

# `src/lib.rs`, line by line

## The import

```rust
use moso::prelude::*;
```

One glob, ~28 names, capped at 40 by rule (`AppBuilder` on the next line is the one thing this file
needs that did not make the cut). `App`, `Router`, `Error`, `Result`, the extractors
(`Json`, `Path`, `Query`, `Form`, `Inject`, `Depends`), the common responses (`Created`,
`NoContent`, `Page`, `Empty`), the model traits (`Schema`, `Email`, `Slug`, `Id`, `Cursor`),
`Config`, and every macro. Anything else is one path away and the path tells you where it came
from: `moso::extract::Cookies`, `moso::response::Sse`, `moso::http_config::ServerConfig`.

## The model

```rust
/// The body `GET /hello/{name}` returns.
#[derive(Schema)]
pub struct Greeting {
    /// Who was greeted.
    pub name: String,
    /// The greeting itself.
    pub message: String,
}
```

`#[derive(Schema)]` is the load-bearing idea of the framework: **one type definition does every
job**. From this one declaration you get

| | from |
| --- | --- |
| `Serialize` / `Deserialize` | delegated to serde's own derive, so `#[serde(…)]`-class features keep working |
| `Validate` | the runtime checks implied by `#[schema(…)]` - there are none here; see below |
| `Schema` | the JSON Schema node that lands in `components/schemas/Greeting` |
| `IntoResponse` + `Describe` | which is why a handler may return `Greeting` directly, not only wrapped in `Json` - and why doing so still documents a `200` with this schema |

The doc comments are not decoration. `/// The body GET /hello/{name} returns.` becomes the schema's
`description`, and `/// Who was greeted.` becomes the `description` of the `name` property. Open
`/docs` and you are reading the comments you wrote next to the fields.

This example has no constraints because a greeting has nothing to constrain. A request body would:

```rust
#[derive(Schema)]
pub struct CreateUser {
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    pub email: Email,                 // a constrained type: parsing *is* validating
    #[schema(range = 13..=130)]
    pub age: Option<u8>,
}
```

Each attribute is parsed once and used twice - `len = 3..=32` emits both the runtime length check
*and* `"minLength": 3, "maxLength": 32` in the published schema. They cannot drift apart, because
they are generated from the same parse.

## The endpoint

```rust
/// Greet someone by name.
#[endpoint]
async fn hello(
    Path(name): Path<String>,
    Inject(config): Inject<AppConfig>,
) -> Result<Json<Greeting>> {
    Ok(Json(Greeting {
        message: format!("{}, {name}!", config.greeting),
        name,
    }))
}
```

`#[endpoint]` **leaves the function exactly as written** - you can still call `hello(…)` from a unit
test - and additionally emits a hidden companion type, `__moso_op_hello`, which carries the
operation's metadata. (Rust cannot attach an associated type to a plain `fn` item; the companion
type is how the metadata gets a place to live. `routes!` and `ep!` know the naming rule, so you
never type that name.)

The parameter list *is* the request contract:

- **`Path(name): Path<String>`** - binds the `{name}` segment, and records a path parameter of type
  `string` in the document. A route whose `{…}` segments do not match what the extractor declares is
  a boot error (`path parameter mismatch`), not a 404 discovered in staging.
- **`Inject(config): Inject<AppConfig>`** - pulls `Arc<AppConfig>` out of the provider map built at
  boot. It is **infallible at the use site**: `App::build()` already proved a provider exists, so
  there is no `?` and no `unwrap` here. Register nothing and the *boot* fails, naming this handler.

The return type *is* the response contract. `Result<Json<Greeting>>` documents `200` with
`application/json` and `$ref: "#/components/schemas/Greeting"`, plus the error responses every
handler can produce (`500`, `503`) as `application/problem+json`. The doc comment becomes the
operation's `summary`; the function name becomes its `operationId`.

Note the order: **body extractors come last**, and there may be at most one. `Path`, `Query`,
`Headers`, `Inject` and `Depends` read only the request head, so they can run in any order; `Json`,
`Form` and `Bytes` consume the body. The macro enforces this with a hand-written error rather than
letting you discover it as a trait-bound failure.

## The configuration

```rust
/// Everything this application can be configured with.
#[derive(Config, Debug)]
pub struct AppConfig {
    /// The word placed before the name. Override with `GREETING=Hei`.
    #[config(default = "Hello")]
    pub greeting: String,
}
```

`#[derive(Config)]` reads each field from a layered stack, highest precedence first: code overrides,
CLI flags, environment variables, `.env` (dev and test only - a production process never reads one),
`config/dev.toml` for the active profile, `config/default.toml`, and finally the declared default.
Every problem is reported at once, with the key, the source it came from and what was expected; you
do not fix configuration one restart at a time.

Try it:

```
$ GREETING=Hei cargo run -p example-minimal
$ curl -s localhost:3000/hello/ada
{"name":"ada","message":"Hei, ada!"}
```

`AppConfig` also happens to be this example's only dependency-injected service, because
`App::new(config)` registers the configuration as a provider. That is what makes
`Inject<AppConfig>` resolve in the handler above.

## The composition root

```rust
pub fn app() -> Result<AppBuilder> {
    Ok(App::new(AppConfig::load()?).mount(moso::routes! { GET "/hello/{name}" => hello }))
}
```

Everything the application *is* is visible in this function. No autoloading, no directory scanning,
no link-time registry - if it is not written here, it is not in the program.

It returns the *builder*, and `main` calls `.build()`. That one-word difference is what lets a test
swap a provider - a fake clock, an in-memory store - into the real application before the boot
checks run, instead of assembling a second, simpler application that proves nothing about this one.
(`AppBuilder` is not in the prelude: the prelude is capped at 40 names and this one appears once per
program.)

`routes!` is a table, not a DSL: `METHOD "path" => handler`. It expands to the plain builder chain

```rust
Router::new().endpoint::<__moso_op_hello>(HttpMethod::Get, "/hello/{name}")
```

so the table form and the builder form (`Router::new().get("/hello/{name}", ep!(hello))`) are the
same program and produce byte-identical documents. Paths use modern `{name}` syntax; the older
`:name` form is rejected at **compile time**.

`build()` is where the boot-time contract is checked, and it collects *every* problem before
failing rather than stopping at the first:

1. configuration loads,
2. providers are registered and frozen into an immutable map,
3. route paths are valid and no two registrations collide,
4. **every `Inject<T>` in every handler has a provider** - this is the check that replaces a runtime
   panic in request 400,000 with a boot error you see immediately,
5. the OpenAPI document assembles without duplicate `operationId`s or conflicting schema names,
6. the middleware stack is consistent.

A failure is one sorted report, naming the missing type, the route that needs it, the line it was
written on, and the edit that fixes it. Delete `.provide(…)` from an app that injects a `Store` and
this is verbatim what you get:

```
error: application failed to build (1 problem)

  x missing provider: `example::Store`
      required by  GET /things  src/routes/things.rs:8
      fix          register it on the `App` builder, usually in src/lib.rs
                   let value: Store = /* construct it */;
                   App::new(config).provide(value)
```

---

# `src/main.rs`

```rust
#[tokio::main]
async fn main() -> moso::Result<()> {
    let app = example_minimal::app()?.build()?;
    println!("docs on http://{}/docs", app.state().server().bind);
    app.serve().await
}
```

The binary is a shim over the library, and it never grows. That is not style, it is what makes the
test in `tests/` able to boot the **real** application instead of a parallel, test-only copy of it -
the single most common way an integration test ends up testing the harness rather than the program.

`serve()` binds the configured address (`0.0.0.0:3000` by default), installs signal handlers, runs
the startup hooks, and drains in-flight requests on `SIGTERM` within the shutdown grace.

The `println!` is here only because a hello world should say where it is. A real application installs
a `tracing` subscriber, and Moso logs the same line - plus one structured event per request - through
`tracing::info!`. Moso deliberately does not install a global subscriber for you.

**To change the port**, hand the builder a `ServerConfig` (the listen address is not read from the
environment by default, because for most deployments it is fixed and the process is behind an
ingress):

```rust
use moso::http_config::ServerConfig;

App::new(config)
    .server_config(ServerConfig { bind: "127.0.0.1:8080".parse().unwrap(), ..Default::default() })
```

---

# What you get without writing it

Nothing below appears anywhere in `lib.rs`.

| Route | |
| --- | --- |
| `GET /healthz` | liveness - is the process up |
| `GET /readyz` | readiness - flips to `503` the instant a shutdown signal arrives, so a load balancer stops sending work before the process stops answering |
| `GET /openapi.json` | the generated document, pre-serialised and `ETag`-ed |
| `GET /docs` | an embedded documentation UI - no CDN, no network access at runtime |

These are mounted on an *outer* router, so they answer even if the application's own router has a
problem, and they are excluded from the access log so probes do not drown it.

The standard middleware stack is a fixed sequence of 15 named **slots**, outermost first:

```
catch_panic  request_id  trace  sensitive_headers  catch_error  request_limits  timeout
body_limit  normalize_path  cors  security_headers  compression  rate_limit  session  metrics
```

Ten are on in a default build: `catch_panic`, `request_id`, `trace` (via the default `tracing`
feature), `sensitive_headers`, `catch_error`, `request_limits` (414 and 431), `timeout` (30 s),
`body_limit` (2 MiB), `normalize_path` and `security_headers`. `cors` and `compression` turn on with their cargo features;
`rate_limit`, `session` and `metrics` are reserved positions that batteries and your own layers fill
by name with `MiddlewareStack::replace`. Slots rather than a list is the point: adding a layer cannot
move any other layer, so the order is knowable instead of emergent - and `moso middleware` can print
it.

Which is why the earlier `curl` came back with

```
x-request-id: 01KYQG8PR71J19X85TMBVNF2Q4
x-content-type-options: nosniff
x-frame-options: DENY
content-security-policy: frame-ancestors 'none'
referrer-policy: strict-origin-when-cross-origin
strict-transport-security: max-age=63072000; includeSubDomains
```

And errors are RFC 9457 problem documents, always:

```
$ curl -si localhost:3000/hello
HTTP/1.1 404 Not Found
content-type: application/problem+json
…
{"type":"about:blank","title":"Not Found","status":404,"detail":"no route matches /hello"}
```

`detail` on a `5xx` is suppressed unless `http.expose_internal_errors` is set, so a stack trace never
leaks to a client; the `x-request-id` is how you find the full error in the log.

---

# `tests/hello.rs`

```rust
use moso::deps::serde_json;      // re-exported, so the test adds no dependency of its own
use moso_test::prelude::*;

async fn spawn() -> TestApp {
    TestApp::builder()
        .app(example_minimal::app().expect("configuration loads"))
        .override_provider(example_minimal::AppConfig { greeting: "Hello".to_owned() })
        .spawn()
        .await
        .expect("the example boots")
}

#[tokio::test]
async fn hello_answers_200_with_the_greeting() {
    let app = spawn().await;

    app.client()
        .get("/hello/ada")
        .send()
        .await
        .assert_status(200)
        .assert_json_eq(serde_json::json!({ "name": "ada", "message": "Hello, ada!" }))
        .assert_matches_openapi();

    app.logs().assert_no_errors();
}
```

Four tests: the handler answers `200` with the expected JSON, an unknown path is a problem document,
the operation is in `/openapi.json` with its summary, parameter and response schema, and the whole
thing also works over a real socket.

Five things worth copying:

- **`TestApp::builder().app(app())` boots the real application.** Not a router assembled for the
  test - the same builder `main` uses, with the same providers, the same middleware and the same
  generated document. A test harness that constructs a parallel, simplified app tests the harness.
- **`override_provider` edits that application rather than replacing it.** Here it pins `AppConfig`
  so that a `GREETING` exported in your shell cannot change what the test expects. In a larger app it
  is where the fake mailer and the stub payment gateway go.
- **The default transport is in-process.** `spawn()` drives the composed `tower::Service` directly -
  the whole middleware stack, no socket - so tests run in microseconds and in parallel.
  `.bind()` opts one test into a real ephemeral port when you want `serve` itself covered, which is
  what `it_also_works_over_a_real_socket` does.
- **`assert_matches_openapi()` validates the body against the schema the document promises** for
  that operation. One line turns a test into a contract test, and it is the assertion that catches
  documentation drift the moment it happens.
- **A failed assertion prints a report, not `left == right`:** the request, the response, a
  structural JSON diff, and the server's own log lines for that request id. `app.logs()` is the same
  capture, available as an assertion - `assert_no_errors()` is a good last line for any test.

The OpenAPI test is the one to keep. It asserts that a summary nobody typed, a parameter nobody
declared, and a schema nobody wrote by hand are all present and correct - that the documentation is
*derived* rather than maintained. If that ever stops being true, this is where you find out.

---

# Where to go next

`examples/crud` is the same ideas at application scale: a request body with real constraints and its
`422` with JSON Pointers, several resources, `Depends` for per-request dependencies, tags, and
pagination.
