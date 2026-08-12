---
title: Testing
description: Boot the real application in a test, drive it with a typed client, assert on responses and server logs, give every test its own database, and check bodies against the OpenAPI document.
order: 34
status: shipped
---

`moso-test` boots the application your `main` actually serves, in the same process as the test, and
gives you a client to drive it over HTTP. Not a parallel simplified app, not a handler called
directly with hand-built extractors: the real provider map, the real middleware stack, the real
boot-time validation and the real generated OpenAPI document. A test that passes has exercised the
things that actually break.

The other half of the crate is the failure output. When an assertion fails you get the request line,
the request headers, the request body, the response with its timing and transport, the response
headers, the response body, a JSON diff where one applies, and the server's own log lines for that
request id, in one block. That block is the product.

> [!IMPORTANT]
> The harness is shipped and tested. `#[moso::test]` is deliberately **not** provided (decision D11):
> a plain `#[tokio::test]` plus one helper is the whole ceremony, and the crate documentation says so
> rather than treating it as a gap. The battery accessors `app.db()`, `app.kv()`, `app.mail()`,
> `app.jobs()` and `app.storage()`, mail capture with `assert_sent`, and a server-sent-events client
> all exist and are covered below. A couple of things sit outside the harness by design: snapshot
> assertions are left to your assertion crate of choice, and WebSocket testing goes through Axum,
> matching the SSE-first push story. Test scaffolding is generated as plain `#[tokio::test]` rather
> than a `moso test` command, and `moso new` and `moso generate test` emit raw axum and tower tests.
> Each is called out inline where you would otherwise go looking.

## Install it

```toml title="Cargo.toml"
[dev-dependencies]
moso-test = "0.1"

# Databases, factories and `assert_queries!` as well:
# moso-test = { version = "0.1", features = ["db"] }
```

| Feature | Default | What it adds | What it costs |
| --- | --- | --- | --- |
| `server` | on | `TestAppBuilder::bind` and the real-socket transport | pulls in `reqwest` and `rustls` |
| `db` | **off** | `moso_test::db`, `moso_test::factory`, `skip_without_database!`, `assert_queries!` | pulls in `sqlx`, whose bundled SQLite compiles the C amalgamation, a multi-minute build |

`db` is off by default for exactly that build cost. A suite that only tests HTTP must not pay it.
Turn it on in `[dev-dependencies]` where it is used.

Your crate must expose a composition root: a function returning an `AppBuilder`, conventionally
`lib.rs::app()`, with the binary as a thin shim over it. See
[project layout](../start/project-layout.md). The harness cannot find that function on its own, so
the test names it.

## The smallest working test

```rust title="tests/hello.rs"
use moso::deps::serde_json;
use moso_test::prelude::*;

/// Boot the example, with the greeting pinned so that a `GREETING` in the
/// developer's own environment cannot change what the tests expect.
async fn spawn() -> TestApp {
    TestApp::builder()
        .app(example_minimal::app().expect("configuration loads"))
        .override_provider(example_minimal::AppConfig {
            greeting: "Hello".to_owned(),
        })
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
        .assert_header("content-type", "application/json")
        .assert_json_eq(serde_json::json!({
            "name": "ada",
            "message": "Hello, ada!",
        }))
        .assert_matches_openapi();

    app.logs().assert_no_errors();
}
```

That is the whole ceremony: `#[tokio::test]` plus one helper. There is no `#[moso::test]` attribute
in this build and the crate documentation says there does not need to be.

When your composition root takes no configuration argument and is named `app`, the `test_app!` macro
shortens the boot to one line. It expands to an expression of type
`impl Future<Output = moso::Result<TestApp>>`.

```rust
#[tokio::test]
async fn the_zero_argument_macro_calls_app_in_scope() {
    let app = moso_test::test_app!().await.expect("boots");
    app.client().get("/users").send().await.assert_status(200);
}
```

`moso_test::test_app!(builder_expression)` is the one-argument form. Reach for `TestApp::builder()`
the moment the test needs to override a provider or bind a real port.

## Booting the application

`TestAppBuilder` wraps your `AppBuilder` and edits it before calling the real `build()`. Everything
it does is an edit to the application you already have, never a substitute for it.

| Method | Effect |
| --- | --- |
| `app(AppBuilder)` | the application under test; required |
| `mount(Router)` | mount an extra Moso router at the root, for the test only |
| `mount_at(&'static str, Router)` | the same, under a prefix |
| `override_provider(value)` | register a provider that wins over the application's own |
| `override_provider_dyn(Arc<T>)` | the same for a `dyn Trait` provider |
| `customise(FnOnce(AppBuilder) -> AppBuilder)` | arbitrary edit: `on_startup`, `with_middleware`, `openapi`, `health_check`, `mount_axum` |
| `http_config(HttpConfig)` | replace the framework's HTTP section wholesale |
| `http_config_with(FnOnce(&mut HttpConfig))` | edit it in place; edits compose |
| `expose_internal_errors()` | turn on 5xx detail disclosure for a debugging session |
| `server_config(ServerConfig)` | replace the listener and shutdown settings |
| `profile(Profile)` | run under a profile other than `Profile::Test` |
| `inherit_profile()` | leave the application's own profile alone |
| `bind()` | bind a real ephemeral `127.0.0.1` port instead of running in process |
| `paused_time()` | let `advance_time` also move Tokio's clock |
| `clock_at(SystemTime)` | pin the `TestClock` at an absolute instant |
| `assert_openapi(ContractOptions)` | check every response in this app against the document |
| `default_header(&str, &str)` | send a header on every request the app's client makes |
| `log_limit(usize)` | bound the captured log ring (default 4096) |
| `spawn()` | build and boot; returns `moso::Result<TestApp>` |

The defaults are `Profile::Test`, in process, time not paused, no contract options.

Inside `spawn` the order is fixed: your builder first, then every registered edit in registration
order, then the profile, then the HTTP section, then `provide(TestClock)` last of all. Provider
registration is last-write-wins, which is why `override_provider` really does override, and why the
harness's clock beats an application that happens to provide one.

`default_header` validates its name and value eagerly and **panics** on an illegal one, because a
malformed header in a test is a mistake, not a condition to handle.

### Overriding a provider

This is the whole substitution story. A capturing mailer replaces an SMTP one, a fixed clock
replaces a real one, a pinned configuration replaces one read from the environment, and no handler
changes.

```rust
#[tokio::test]
async fn a_provider_override_wins_over_the_applications_own() {
    let app = TestApp::builder()
        .app(app())
        .override_provider(Greeting("overridden".to_owned()))
        .spawn()
        .await
        .expect("boots");
    app.client()
        .get("/greet")
        .send()
        .await
        .assert_json_path("/username", "overridden");
}
```

There is no `config(|c: &mut AppConfig| ...)`. Your application's configuration is a type the
harness has never heard of, already constructed before `App::new` saw it. Pass an edited value to
your own `app()`, or replace it with `override_provider` as above. See
[dependency injection](./dependency-injection.md).

### Mounting test-only routes

```rust
let app = TestApp::builder()
    .app(app())
    .mount_at("/testing", moso::routes! { GET "/probe" => probe })
    .spawn()
    .await
    .expect("boots");

app.client().get("/testing/probe").send().await.assert_status(204);
assert!(app.openapi().paths.contains_key("/testing/probe"));
```

A mounted route is a real route: it goes into the OpenAPI document too. For an axum router that
Moso's macros never saw, go through `customise(|builder| builder.mount_axum("/shadow", extra))`.

### A boot failure is your own boot report

`build()` runs inside the harness's log span, so a dependency-injection graph that does not validate
fails the test with the same grouped report `main` would have printed, at the line that spawned the
app. You find the missing provider in the test run, not in staging.

### Two apps in one process

Nothing is registered globally, so two applications can run side by side in one test binary with
separate provider maps and separate log buffers.

```rust
#[tokio::test]
async fn two_apps_run_side_by_side_without_seeing_each_other() {
    let one = TestApp::builder()
        .app(app())
        .override_provider(Greeting("one".to_owned()))
        .spawn()
        .await
        .expect("boots");
    let two = TestApp::builder()
        .app(app())
        .override_provider(Greeting("two".to_owned()))
        .spawn()
        .await
        .expect("boots");

    one.client().get("/greet").send().await.assert_json_path("/username", "one");
    two.client().get("/greet").send().await.assert_json_path("/username", "two");
}
```

That property is a direct consequence of Moso refusing link-time registries. See
[routing](./routing.md).

### In process or over a socket

By default the client calls the composed `tower::Service` directly. No socket, no accept loop, no
second runtime, roughly an order of magnitude faster, and an entire class of flake removed: no port
to be taken, no accept queue to be full, no connection to be reset.

`bind()` binds `127.0.0.1:0`, spawns `App::serve_on`, and drives it with `reqwest` over the wire.
It polls `GET /healthz` until the application answers before handing the `TestApp` back, because the
listener is bound before the startup hooks run and a TCP connect therefore succeeds long before the
application is serving. The probe times out after five seconds.

```rust
#[tokio::test]
async fn it_also_works_over_a_real_socket() {
    let app = TestApp::builder()
        .app(example_minimal::app().expect("configuration loads"))
        .bind()
        .spawn()
        .await
        .expect("the example boots and binds");

    assert!(app.local_addr().is_some(), "expected a bound port");

    app.client().get("/healthz").send().await.assert_status(200);

    app.shutdown().await;
}
```

Both transports go through the identical middleware stack. There is one behavioural difference and
it matters:

> [!WARNING]
> `on_startup` hooks and lifespan guards run **only** in bound mode. The in-process path uses
> `App::into_service()`, which by definition does not run them. A test whose application depends on
> a startup hook must call `.bind()`, which needs the `server` feature. Without that feature `bind()`
> returns an error saying so rather than panicking.

Other differences to know: `local_addr()` is `Some` only in bound mode, and `service()` is `None`
there because `serve_on` consumed the application. In process, the base URL is the constant
`http://localhost/`, which nothing resolves.

### Reaching past HTTP

| Accessor | Gives you |
| --- | --- |
| `client()` | the baseline `&TestClient` |
| `as_anonymous()` | a client with no credentials |
| `as_bearer(token)` | a client carrying `Authorization: Bearer …` |
| `logs()` | `&LogAssertions` for the whole app |
| `base_url()` | the `Url` requests are built from |
| `local_addr()` | the bound `SocketAddr`, in bound mode |
| `openapi()` | the real generated `moso::openapi::Document` |
| `db()`, `kv()`, `mail()`, `jobs()`, `storage()` | the corresponding battery the booted app resolved, each behind its `moso-test` feature |
| `state()` | `&Arc<AppState>` |
| `resolver()` | the dependency-injection `Resolver` |
| `clock()` | the `TestClock` this app was given |
| `service()` | the composed axum `Router`, in process only |
| `advance_time(Duration)` | move the clock |
| `shutdown()` | drive the real shutdown signal and wait for the drain |

`mail()` hands you a capturing `Mail` when the application resolves one: `capture_mail` records what
handlers send, and `Mail::assert_sent::<Welcome>(1)` and `assert_none_sent()` assert on it by message
type rather than by scraping an SMTP log. `db()`, `kv()`, `jobs()` and `storage()` read the same
battery the booted app resolved, so an assertion can inspect what a request actually wrote.

Dropping a `TestApp` triggers the shutdown signal, aborts the server task and deregisters the log
buffer, so nothing leaks between tests. `shutdown().await` is the deterministic form, and the one to
use when the test wants to assert on what shutdown logged. See
[health and shutdown](./health-and-shutdown.md).

## Driving it

`TestClient` has one method per HTTP method plus `request(Method, path)` for anything else. Each
returns a `RequestBuilder`.

Derived clients never mutate the one they came from, which is what makes the "and now as a stranger"
half of an authorisation test one line:

```rust
let admin = app.client().with_bearer(&admin_token);
admin.get("/admin/users").send().await.assert_status(200);
admin.anonymous().get("/admin/users").send().await.assert_status(401);
```

| Client method | Effect |
| --- | --- |
| `with_header(name, value)` | a copy sending that header on every request |
| `with_bearer(token)` | a copy sending `Authorization: Bearer …` |
| `with_cookie(name, value)` | a copy carrying that cookie |
| `anonymous()` | a copy with `Authorization`, `Cookie` and every accumulated cookie dropped |
| `with_timeout(Duration)` | a copy with a deadline on every request |
| `asserting_openapi(Option<ContractOptions>)` | a copy that checks (or stops checking) every response |

### Building a request

| Builder method | Effect |
| --- | --- |
| `header(name, value)` | add a header; replaces the client's |
| `bearer(token)` | set `Authorization` for this request |
| `cookie(name, value)` | add a cookie; the client's come first |
| `query_pair(name, value)` | append one query-string pair |
| `query(&T)` | append a serializable struct as query pairs |
| `json(&T)` | JSON body, `Content-Type: application/json` |
| `json_value(Value)` | JSON body from a raw `serde_json::Value` |
| `form(&T)` | `application/x-www-form-urlencoded` body |
| `multipart(Multipart)` | multipart body with the generated boundary |
| `body(bytes)` | raw bytes, no content type |
| `text(string)` | `text/plain; charset=utf-8` |
| `timeout(Duration)` | a deadline on this request |
| `request_id(id)` | override the correlation id |
| `asserting_openapi(Option<ContractOptions>)` | contract check for this request |
| `url()` | the assembled `Url`, before sending |
| `send()` | send; a transport failure panics with the report |
| `try_send()` | send; a transport failure comes back as `Err(SendFailure)` |
| `sse()` | send and open the response as a server-sent-events stream, reading parsed events |

`json_value` is how you test a body a typed struct could not express: a missing field, an extra one,
the wrong type. `Content-Length` is set automatically when there is a body.

```rust
#[tokio::test]
async fn headers_cookies_and_bearer_tokens_reach_the_server() {
    let app = spawn().await;
    let response = app
        .client()
        .with_header("x-tenant", "acme")
        .with_cookie("session", "abc")
        .get("/users")
        .bearer("token-1")
        .cookie("theme", "dark")
        .send()
        .await;

    let sent: Vec<(String, String)> = response.request().headers.clone();
    let value = |name: &str| {
        sent.iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    assert_eq!(value("x-tenant").as_deref(), Some("acme"));
    assert_eq!(value("authorization").as_deref(), Some("Bearer token-1"));
    assert_eq!(
        value("cookie").as_deref(),
        Some("session=abc; theme=dark"),
        "client cookies come first, then the request's"
    );
}
```

Multipart bodies are encoded by hand rather than by `reqwest`, so both transports send byte-identical
bytes, and the default boundary is derived from a counter rather than randomness so a captured
failure body is reproducible.

```rust
let form = Multipart::new()
    .text("title", "hello")
    .file("avatar", "a.png", "image/png", &b"\x89PNG"[..]);
app.client().post("/uploads").multipart(form).send().await.assert_status(201);
```

### Failures, deadlines and redirects

`send()` panics with the rendered report rather than returning an error, because in a test a
transport failure is a failure and `?` on it would hide the useful output behind a `Debug` impl.
`try_send()` is for tests that are about the transport:

```rust
#[tokio::test]
async fn a_timeout_is_reported_as_a_send_failure() {
    let app = spawn().await;
    let failure = app
        .client()
        .get("/slow")
        .timeout(Duration::from_millis(50))
        .try_send()
        .await
        .expect_err("the deadline fires before the handler returns");
    assert!(failure.message.contains("did not complete"), "{failure}");
    let report = failure.render(app.logs());
    assert!(report.contains("GET http://localhost/slow"), "{report}");
}
```

`SendFailure` carries the `RequestRecord`, the message, the elapsed time and the request id, and
`render(&LogAssertions)` produces the same report a failed assertion would have printed.

Redirects are **never** followed, and there is no `follow_redirects()`. Following a `3xx` would make
the two transports disagree (there is no client library on the in-process path to do it), and it
would turn a test that meant to assert "this returns a 302 to `/login`" into a test that quietly
asserts something about `/login`.

Every request carries an `x-request-id` the harness generated as 26 Crockford base32 characters, so
that Moso's request-id middleware, which only adopts a client-supplied id when it parses as a ULID,
keeps it. An id the middleware discarded would file the server's log lines under a different key
from the one the client remembers, silently emptying the most useful section of the failure report.

## Asserting on the response

Every assertion returns `&Self` so they chain, and every one panics with the full report.

| Assertion | Checks |
| --- | --- |
| `assert_status(impl IntoStatus)` | exact status; takes `201` or `StatusCode::CREATED` |
| `assert_ok()` | any 2xx |
| `assert_header(name, value)` | exact header value |
| `assert_header_present(name)` | the header exists |
| `assert_no_header(name)` | the header does not exist |
| `assert_header_contains(name, needle)` | substring of the header value |
| `assert_empty_body()` | zero bytes |
| `assert_text_contains(needle)` | substring of the body as text |
| `assert_json_path(pointer, value)` | value at an RFC 6901 JSON Pointer |
| `assert_no_json_path(pointer)` | nothing at that pointer |
| `assert_json_matches(value)` | subset match |
| `assert_json_eq(value)` | exact match |
| `assert_problem(code)` | RFC 9457 problem type, by slug or full URI |
| `assert_field_error(pointer, code)` | one entry of a validation rejection |
| `assert_matches_openapi()` | body against the documented schema, strict |
| `assert_matches_openapi_with(ContractOptions)` | the same with explicit options |

Accessors are there for everything the assertions do not cover: `status()`, `headers()`,
`header(name)`, `body()`, `text()`, `request()`, `elapsed()`, `request_id()`, `logs()` and
`openapi()`. Typed decoding is `json::<T>()` (fails the test with the whole report),
`try_json::<T>()` (returns the serde error), `json_value()` and `problem()`.

### Subset or exact

`assert_json_matches` is a subset match and `assert_json_eq` is not. Prefer the subset: it is what
makes an assertion survive the day a field is added, and a test that breaks when the API gains a
member is a test that will be deleted.

```rust
let response = app.client().get("/users/7").send().await;
response
    .assert_json_matches(json!({ "id": 7 }))
    .assert_json_eq(json!({ "id": 7, "username": "user7" }))
    .assert_no_json_path("/email");
```

Arrays are compared positionally in **both** modes. A subset match on an array would have to decide
whether `[1]` matches `[1, 2]` at index 0 or means "some element is 1", and a rule that needs a
paragraph is a rule that will be misread in a test.

### Errors and validation

`assert_problem` takes the full type URI or just its last segment, which is what `ErrorKind::slug`
returns. `assert_field_error` asserts on one entry of the `errors[]` array a validation rejection
produces, so the test says "a 422 about the username being too short" rather than merely "a 422".

```rust
#[tokio::test]
async fn an_invalid_body_is_a_422_with_a_field_error() {
    let app = spawn().await;
    app.client()
        .post("/users")
        .json(&json!({ "username": "A", "email": "ada@example.com" }))
        .send()
        .await
        .assert_status(422)
        .assert_problem("validation")
        .assert_field_error("/username", "len");
}
```

```rust
app.client()
    .get("/users/0")
    .send()
    .await
    .assert_status(404)
    .assert_problem("not-found")
    .assert_problem("https://moso.rs/errors/not-found");
```

See [errors](./errors.md) and [validation](./validation.md).

## The failure report

This is what a failed `assert_status(201)` actually prints. Nothing here is reconstructed: it is the
output of the crate's own suite.

```text
── moso-test: assertion failed ─────────────────────────────────────────
  expected status 201 Created, got 200 OK

  request:
    GET http://localhost/users

  request headers:
    x-request-id: 00000000013SHG000000000001

  response:
    200 OK  (0.2 ms, in-process)

  response headers:
    content-type: application/json
    content-length: 27
    x-content-type-options: nosniff
    x-request-id: 00000000013SHG000000000001

  response body:
    [
      {
        "id": 1,
        "username": "ada"
      }
    ]

  server logs for request_id 00000000013SHG000000000001:
    INFO  moso::http  request  status=200  method=GET  path=/users  duration_ms=0.126  request_id=00000000013SHG000000000001

──────────────────────────────────────────────────────────────────────────
```

A JSON assertion adds a structural diff, by pointer, with `-` for missing, `+` for unexpected and
`~` for changed:

```text
  json diff:
    ~ (root)
        expected: 8
        actual:   7
```

The report is plain text with no colour, because test output is read in CI logs, in terminals with
every possible background, and in editors that strip ANSI. Bodies are pretty-printed when they are
JSON and elided past 8 KiB.

`TestResponse::report(headline, extra)` renders the same block without panicking, for a test that is
about the harness itself.

## Contract tests against the OpenAPI document

`assert_matches_openapi()` validates the body against the schema your own application publishes for
that operation and that status. The document is the real one, assembled at boot from your handler
signatures, so this is a genuine drift check and not a second hand-written spec. See
[OpenAPI](./openapi.md).

It is **strict by default**: a property the document does not describe is drift, and the violation
reads `the response carries a property the document does not describe`. JSON Schema itself says an
object with `properties` and no `additionalProperties` accepts any extra member, which is the right
default for an input schema and precisely the wrong one for a contract test. `ContractOptions::lax()`
restores the literal reading.

Turn it on for every response in the app and no test needs the call at all:

```rust
#[tokio::test]
async fn every_response_can_be_checked_automatically() {
    let app = TestApp::builder()
        .app(app())
        .assert_openapi(ContractOptions::strict())
        .spawn()
        .await
        .expect("boots");
    // No explicit `assert_matches_openapi`: the client does it on every send.
    app.client().get("/users/7").send().await.assert_status(200);
}
```

`TestClient::asserting_openapi(Some(options))` scopes it to one client and
`RequestBuilder::asserting_openapi(None)` turns it off for a single request.

The validator checks `$ref` (local, against `components/schemas`), `type` with JSON Schema 2020-12
integer semantics, `const`, `enum`, `minLength` and `maxLength` counted in code points, `pattern`,
the numeric bounds and `multipleOf`, the array keywords including `prefixItems` and `uniqueItems`,
the object keywords including `required` and `additionalProperties`, and `allOf`, `anyOf`, `oneOf`
and `not`. It guards against a cyclic `$ref` at depth 128.

It deliberately does **not** check `format`. Format is an annotation, not a constraint, and a
validator that rejected a string for failing `"format": "email"` would fail tests for something the
document does not actually assert. Formats are checked on the way in, by `moso_schema`'s validation.
See [schemas](./schemas.md).

Failure modes worth knowing:

- A path the document does not describe fails with ``no path matching `/nope` ``. An exact path wins
  over a template, so `/users/me` beats `/users/{id}`, matching the order the router resolves them.
- A status the operation does not document falls back exact, then `4XX`, then `default`, then a
  lower-case `4xx`, and fails if none of those exist.
- A documented non-JSON media type is checked for its **type** only and then returns. Asserting a
  JSON Schema over a PNG would be theatre.

## Capturing logs

The harness installs a `tracing` subscriber the first time any `TestApp` spawns and files every
event under the application, and the request, that produced it.

```rust
app.client().get("/noisy").send().await.assert_status(204);

assert!(app.logs().is_capturing());
app.logs()
    .assert_contains(Level::WARN, "rate limit")
    .assert_contains(Level::ERROR, "something went wrong")
    .assert_contains_at_least(Level::WARN, "rate limit")
    .assert_none_containing(Level::WARN, "no such line");
```

| Method | Effect |
| --- | --- |
| `assert_contains(level, needle)` | a line at **exactly** that level contains the needle |
| `assert_contains_at_least(level, needle)` | that level or more severe |
| `assert_none_containing(level, needle)` | no line at that level contains it |
| `assert_no_errors()` | nothing was logged at `ERROR` |
| `records()` | every captured `LogRecord` |
| `for_request(id)` | only the records for one request id |
| `clear()` | forget the arrange phase before the act phase |
| `dump()` | the whole buffer, as the report renders it |
| `is_capturing()` | whether capture is running at all |

`LogRecord::contains` matches the message, the target **and** every field key and value, because
`tracing::error!(%error, "…")` puts the interesting half in a field.

`TestResponse::logs()` narrows to the lines for that one request, which is the same set the failure
report prints:

```rust
let loud = app.client().get("/noisy").send().await;
assert!(loud.logs().iter().any(|record| record.contains("rate limit")));
assert!(loud.logs().iter().all(|record| record.request_id.as_deref() == Some(loud.request_id())));
```

> [!WARNING]
> `tracing` allows one global subscriber per process. If your test binary installs its own first (a
> `tracing_subscriber::fmt().init()` somewhere), every buffer stays empty for the whole run and
> `is_capturing()` returns `false`. The harness records the fact rather than failing quietly: the
> failure report says capture is unavailable instead of showing an empty log section, and
> `assert_queries!(&app, …)` says the same. The fix is to remove the other subscriber.

See [observability](./observability.md) for the fields Moso's own trace layer records.

## Controlling time

Every `TestApp` provides a `TestClock`. Application code that takes `Inject<TestClock>` reads a clock
the test drives.

```rust
#[tokio::test]
async fn advancing_time_moves_the_provided_clock() {
    let app = spawn().await;
    let clock: Arc<TestClock> = app.resolver().get().expect("the harness provides one");
    let before = clock.now();

    app.advance_time(Duration::from_secs(3600)).await;

    assert_eq!(clock.now(), before + Duration::from_secs(3600));
    assert_eq!(app.clock().now(), clock.now());
}
```

`TestClock` has `now`, `base`, `advance`, `rewind`, `set`, `offset` and `is_rewound`. It is cheap to
clone and every clone reads and writes the same offset. Advancing saturates rather than overflowing.
`TestAppBuilder::clock_at(SystemTime)` pins the base at boot.

To move Tokio's own clock as well, opt in on both sides: `paused_time()` on the builder and
`start_paused = true` on the test. Both are required because `tokio::time::advance` panics on a
runtime whose time is not paused, and a harness must not turn a missing annotation into a panic
inside an unrelated assertion.

```rust
#[tokio::test(start_paused = true)]
async fn paused_time_also_advances_tokios_clock() {
    let app = TestApp::builder()
        .app(app())
        .paused_time()
        .spawn()
        .await
        .expect("boots");

    let fired = Arc::new(AtomicU64::new(0));
    let flag = Arc::clone(&fired);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        flag.store(1, Ordering::SeqCst);
    });
    // Let the task register its timer; `advance` only fires timers that exist.
    tokio::task::yield_now().await;

    app.advance_time(Duration::from_secs(120)).await;
    tokio::task::yield_now().await;

    assert_eq!(fired.load(Ordering::SeqCst), 1);
    assert_eq!(app.clock().offset(), Duration::from_secs(120));
}
```

> [!WARNING]
> `advance_time` does **not** move framework internals. `moso-core` reads `Instant::now()` and
> `SystemTime::now()` directly rather than through an indirection a harness can replace, so request
> timeouts, `Retry-After` and the shutdown grace period are unaffected by either clock. Closing that
> gap is a `moso-core` change, not a harness one.

## A database per test

Everything in this section needs the `db` feature.

`TestDb` gives each test its own database, created from a prepared template in about fifty
milliseconds and dropped when the handle goes away. Sharing a database between tests is the single
most common cause of a suite that passes alone and fails in parallel, and every workaround for it
(truncating between tests, serialising the suite, prefixing every fixture with the test name) is
worse than the disease.

SQLite needs nothing running at all:

```rust
#[tokio::test]
async fn a_sqlite_test_database_needs_no_environment_at_all() {
    let db = TestDb::builder()
        .sqlite()
        .migrator(widgets())
        .acquire()
        .await
        .expect("a SQLite database");

    assert!(db.url().starts_with("sqlite://"));
    db.execute("insert into widget (name) values ('a')")
        .await
        .expect("insert");
    assert_eq!(db.count("widget").await.expect("count"), 1);
    db.close().await;
}
```

PostgreSQL comes from `DATABASE_URL`, or from an explicit `url()`:

```rust
let db = TestDb::builder()
    .strategy(Strategy::Template)
    .template("moso_test_widgets_template")
    .migrator(SqlMigrator::from_dir("migrations").expect("the migrations read"))
    .acquire()
    .await
    .expect("a template database");
```

`url()` clears `sqlite`, and `sqlite()` clears `url`. The last call wins.

### The three strategies

| Strategy | How it isolates | Cost and constraint |
| --- | --- | --- |
| `Template` (default) | copies a prepared template: `CREATE DATABASE … TEMPLATE` on PostgreSQL, a file copy on SQLite | needs a migrator; roughly fifty milliseconds per test |
| `Transaction` | opens **one** connection on the configured database and issues `begin`; the rollback is the isolation | fastest, but the code under test may not commit and may not use a second connection; creates no database at all |
| `Migrate` | creates an empty database and replays the whole migrator | slowest; this is the "does the chain still apply from empty" check |

Set it with `TestDbBuilder::strategy`, or globally with `MOSO_TEST_STRATEGY`. The parser accepts
`template`; `transaction` or `tx`; `migrate`, `migration` or `migrations`, trimmed and lower-cased.
An unrecognised value in the environment variable is ignored rather than fatal.

The template's correctness rests on the migrator's fingerprint: when it changes, the template is
dropped and rebuilt, so an edited migration cannot be tested against yesterday's schema.
`SqlMigrator::from_dir` folds the file *names* into the fingerprint as well as the contents, so a
rename rebuilds too.

### Supplying the schema

`SqlMigrator::from_dir("migrations")` reads every `.sql` file sorted by name.
`SqlMigrator::new([...])` takes scripts inline. For anything else, implement `Migrator`:

```rust
use moso_test::db::{BoxFuture, MigrationError, MigrationTarget, Migrator};

/// The two tables these tests need.
struct Schema;

impl Migrator for Schema {
    fn fingerprint(&self) -> String {
        "widgets-v1".to_owned()
    }

    fn migrate<'a>(&'a self, target: &'a MigrationTarget)
        -> BoxFuture<'a, Result<(), MigrationError>>
    {
        Box::pin(async move {
            target.execute("create table widget (id int primary key)").await?;
            Ok(())
        })
    }
}
```

`moso-migrate` supplies its own `Migrator`, so a project using it does not write one. See
[migrations](./migrations.md).

> [!NOTE]
> `TestDb::acquire()` with no builder supplies an **empty** migrator, so `Strategy::Template`
> produces an empty schema rather than failing. That is deliberate, so `TestDb::acquire()` stays
> meaningful for tests whose fixtures are their own DDL, and it is surprising the first time.

### Pointing the application at it

`TestApp::db()` exists, behind the `db` feature, and hands you the `moso::db::Db` the booted app
resolved so an assertion can read what a request wrote. Pointing the application at a test database in
the first place is the separate step below: the test wires it in through the provider map, the same
way your application does.

```rust
use moso_test::db::{SqlMigrator, TestDb};
use moso_test::prelude::*;

#[tokio::test]
async fn posts_are_listed() {
    moso_test::skip_without_database!();

    let db = TestDb::builder()
        .migrator(SqlMigrator::from_dir("migrations").expect("the migrations read"))
        .acquire()
        .await
        .expect("a test database");

    let handle = db.orm().await.expect("a pool on the test database").clone();

    let app = TestApp::builder()
        .app(my_crate::app().expect("configuration loads"))
        .override_provider(handle)
        .spawn()
        .await
        .expect("boots");

    app.client().get("/posts").send().await.assert_status(200);
}
```

`db.orm()` opens a `moso::db::Db` on the test database, and `db.config()` gives the
`DatabaseConfig` behind it if your application builds its own pool. Override whichever of the two
your composition root resolves. Under `Strategy::Transaction` that pool is pinned to one connection.

`skip_without_database!()` gates the test on `DATABASE_URL` being set, prints the module path and
the reason on standard error, and returns. Use `skip_without_database!(Ok(()))` in a test that
returns a `Result`. It gates on the environment variable only, so a SQLite-backed test never needs
it.

### Environment and cleanup

| Variable | Effect |
| --- | --- |
| `DATABASE_URL` | the server URL `TestDb::acquire` reads |
| `MOSO_TEST_STRATEGY` | overrides the default strategy |
| `MOSO_TEST_KEEP_DB` | `1`, `true`, `yes` or `on` keeps every test database for inspection |
| `MOSO_TEST_TEMPLATE` | overrides the template database's name |

Template name resolution is: an explicit `template()`, then `MOSO_TEST_TEMPLATE`, then
`<database>_template`, then `moso_test_template`.

Keep one database after a failure with `TestDbBuilder::keep()` or `TestDb::keep()`, which prints its
URL. Every generated name carries the creation time, the process id and an ordinal in base 36, which
is how `prune_test_databases(url, &PruneOptions)` can tell a database abandoned by yesterday's
crashed run from one a running test is using right now. `PruneOptions` has `older_than`, `dry_run`,
`with_templates` and `force`, and it refuses to touch a database owned by the current process unless
`force()` is set. `prune_test_files(&PruneOptions)` is the SQLite equivalent.

> [!NOTE]
> Pruning is a library function. There is no `moso db prune-test` command in this build, despite the
> function's own documentation referring to one. Call it from an `xtask` or a `#[test]` if you want
> it in CI.
>
> It cannot move to `moso-migrate` with the [other `moso db` entry
> points](./migrations.md#the-command-entry-points): the naming convention that tells an abandoned
> test database from a live one (the prefix, creation time, process id and ordinal, all in base 36)
> lives in `moso-test`, and `moso-migrate` neither depends on it nor should. The entry point belongs
> next to the convention, in `moso_test::db`.

Running the PostgreSQL-backed tests needs a server. The repository provisions one on host port 55433
(deliberately not 5432) with `fsync=off` and the cluster on a tmpfs, so `CREATE DATABASE … TEMPLATE`
is fast and teardown is total:

```bash
docker compose -f compose.test.yaml up -d --wait
export DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test
cargo test --workspace
docker compose -f compose.test.yaml down -v
```

## Counting statements, and the N+1 guard

`assert_queries!` fails the test unless the block ran the number of statements you expected. It needs
the `db` feature.

```rust
use moso_test::{QueryLog, assert_queries};

let log = QueryLog::new();
assert_queries!(&log, 2, {
    log.record_sql("select 1");
    log.record_sql("select 2");
});
```

The source is anything implementing `QuerySource`: a `&TestDb` for statements the test itself ran, a
`&TestApp` for statements the **server** ran, or a `&QueryLog`. The realistic shape is the second:

```rust
assert_queries!(&app, 2, {
    app.client().get("/posts").send().await.assert_status(200);
});
```

Four forms:

```rust
assert_queries!(source, 2, { … });                    // exactly two
assert_queries!(source, at most 5, { … });            // a budget
assert_queries!(source, 2, + transactions, { … });    // count begin/commit/rollback too
assert_queries!(source, at most 5, + transactions, { … });
```

Transaction control is not counted by default, because a number that changes when the pool decides
to open a transaction is a number nobody can assert on. `at most` is for when the precise count is
an implementation detail but "one per row" is a bug. The block's value is the macro's value, so it
can wrap an expression that produces something.

On a mismatch the report prints every statement, numbered and truncated to 96 characters, and when
one of them repeats it says how many times, calls it an N+1, and adds a `help:` line suggesting you
preload the relation with `Post::query().with(Post::AUTHOR)` instead of touching it in a loop. See
[relations](./relations.md).

> [!WARNING]
> `assert_queries!(&app, …)` reads the server's statements out of captured `sqlx::query` log lines,
> not out of a hook. It therefore depends on `sqlx` logging being enabled and on the harness having
> won the process's `tracing` subscriber race. When capture is unavailable the rendered report says
> so instead of quietly reporting zero. A `&TestDb` source records statements directly and has
> neither dependency.

`QueryLog` holds 4096 statements by default and evicts the oldest past that. `total()` counts
everything ever recorded and `since(mark)` compensates for eviction, but `statements()` on a very
long block will be short of the head.

## Fixtures and factories

A fixture should say only what the test is about. Everything else, the email, the display name, the
password hash, the organisation row a `NOT NULL` foreign key insists on, is invented. This needs the
`db` feature.

### Deterministic fake data

```rust
use moso_test::factory::{Faker, Seed};

// Seeded from the test's own name, so a failure reproduces exactly.
let mut faker = Faker::for_test("users::create_returns_201");
let email = faker.email();
assert!(email.contains("@example."), "{email}");

// The same seed always produces the same data.
let mut again = Faker::for_test("users::create_returns_201");
assert_eq!(again.email(), email);
```

Nothing in `Faker` reads the clock or the operating system's entropy. A test that fails on a fake
email address fails on *that* address every time, on every machine, which is the difference between a
bug and a haunting. `Seed` prints as `{:#018x}` so a failure message shows something you can paste
back into `Seed::new`.

It generates first names, last names, full names, usernames, emails, domains, URLs, words, slugs,
titles, sentences, paragraphs, UUIDs, timestamps, decimals, byte strings, JSON, booleans, integers,
floats and a choice from a slice. Details that matter in practice:

- Every email is on `example.com`, `example.org` or `example.net`, reserved by RFC 2606. A test
  suite that mails strangers is a bug that only shows up in someone else's inbox.
- The word lists are small on purpose: a fixture wants plausible, not varied. `username` and `slug`
  fold in a per-generator discriminator and a sequence number, which removes the possibility of
  violating a unique index rather than making it rarer.
- `uuid` sets the version 4 and RFC 4122 variant bits, so the value round-trips through drivers that
  validate them. `timestamp` is anchored to `2024-01-01T00:00:00Z` plus up to a year, so a fixture
  does not move with the wall clock.
- `one_of` panics on an empty slice.

### Password hashes

```rust
use moso_test::factory::PasswordHash;

let hash = PasswordHash::test();               // "correct horse battery staple"
let other = PasswordHash::of("hunter2");
assert!(other.verify("hunter2"));
assert!(PasswordHash::is_test_hash(hash.as_str()));
```

Argon2 in a fixture makes a suite unusable: a hundred users at 100 ms each is ten seconds per test
file spent re-proving a property nobody doubts, and it is the most common reason a Rails or Django
suite becomes too slow to run. `PasswordHash::test()` is fast and says so in its own text, which
starts `$moso-test$v1$` and is not a valid PHC identifier for any real algorithm.
`PasswordHash::is_test_hash` lets an authentication backend refuse one outside tests. See
[passwords and sessions](./passwords-and-sessions.md).

### Building rows from an entity

`EntityFactory<E>` reads the entity's own descriptor and invents a value for every column the
database will not fill in: not a framework-managed column (`created_at`, `updated_at`, `version`),
not one with a default or a generated expression, not a serial primary key, not a nullable column.
Everything else.

```rust
#[test]
fn a_factory_fills_every_column_the_database_will_not() {
    let row = EntityFactory::<Account>::new().row();
    let names: Vec<&str> = row.iter().map(|(name, _)| name.as_str()).collect();

    assert!(names.contains(&"email"));
    assert!(names.contains(&"display_name"));

    assert!(!names.contains(&"id"), "a serial primary key is the database's");
    assert!(!names.contains(&"created_at"), "the ORM writes this one");
    assert!(!names.contains(&"status"), "it has a default");
    assert!(!names.contains(&"bio"), "a nullable column needs no invention");
}
```

Values are chosen by column **name** first and SQL type second. A text column whose name contains
`password` or `passwd` gets `PasswordHash::test()`; `email` gets an address; `username`, or exactly
`handle` or `login`, gets a username; `slug`, `url`/`uri`/`website`, `title`/`subject`, and
`body`/`content`/`description`/`summary` each get the matching generator; exactly `name`, or a name
ending `_name` or starting `name_`, gets a full name. Failing that, the SQL type decides. Types the
faker has no answer for (networks, ranges, vectors, enums, user types) become `NULL`, so the database
says what it needs rather than the driver producing a confusing error. Generated strings are
truncated to a `varchar(n)`'s length.

Pin what the test is about, and stop wherever you like:

| Method | Effect |
| --- | --- |
| `set(column, value)` | pin a column to a bound value |
| `set_expr(column, Expr)` | pin it to an expression: `now()`, a cast, a subquery |
| `set_null(column)` | pin it to `NULL` |
| `count(n)` | how many rows `create_many` produces |
| `sequence(\|index, row\| …)` | vary each row by its index |
| `seeded(Seed)` / `faker_mut()` | control the seed and the generator |
| `registry(&'static FactoryRegistry)` | where parent factories are looked up |
| `row()` | the columns and values, without touching the database |
| `insert_statement(&row)` | the `moso_sql::Statement` |
| `relation_plan()` | the parents that would be created |
| `create(&db)` / `create_many(&db)` | insert and return the entity or entities |

```rust
let factory = EntityFactory::<Account>::new()
    .count(3)
    .sequence(|index, row| {
        *row = row.clone().set("display_name", format!("Account {index}"));
    });
```

The sequence closure runs on a clone per row, so nothing it does leaks into the next row or back into
the factory. Row 4 of a run of ten is the same as row 4 of a run of a hundred, because each row's
seed is derived from the index rather than drawn in order.

`set` and `set_expr` **panic** on a column name that is not a valid SQL identifier, with a `help:`
line reminding you that a factory setter takes the column, not the field. Pinning the same column
twice keeps the last. `create_many` stops at the first failure and leaves the rows already created,
because a test that fails half way is easier to debug with the evidence still there.

### Parent rows

`plan_relations` works out which foreign keys need a parent: **every** part of the key must be `NOT
NULL`, have no default, and not have been supplied. A key that can legally be `NULL` needs no parent,
because the row is valid without one and inventing an organisation for every user is exactly the
surprise that makes people distrust factories. A composite key or a required self-reference is
reported as `Unsatisfiable` rather than guessed at.

Where a parent is needed, a `FactoryRegistry` says how to make one. Registration is a line of code,
never magic, and the error a missing one produces names the table:

```rust
use moso_test::db::BoxFuture;
use moso_test::factory::{ParentFactory, Result};
use moso_sql::{TableRef, Value};

/// Organisations, for tests that only care about their users.
struct Organisations;

impl ParentFactory for Organisations {
    fn table(&self) -> TableRef {
        TableRef::from_static("organisations")
    }

    fn create_parent<'a>(&'a self, _db: &'a moso_orm::Db) -> BoxFuture<'a, Result<Value>> {
        Box::pin(async move { Ok(Value::I64(1)) })
    }
}
```

`FactoryRegistry::global()` is the default lookup; `register`, `get`, `tables` and `clear` are the
rest of it.

### Two factory APIs, and how to tell them apart

> [!CAUTION]
> `#[derive(Factory)]` and `moso_test::factory` are **independent**. The derive lives in
> `moso-orm-macros`, has no dependency on `moso-test`, and generates its own typed builder. Doc
> comments in the harness that claim the derive produces an `impl Factory` targeting `EntityFactory`
> describe an intention, not this build.

| | `#[derive(Factory)]` | `moso_test::factory` |
| --- | --- | --- |
| Entry point | `User::factory()` returning `UserFactory` | `EntityFactory::<User>::new()`, or `factory()` on the `Factory` trait |
| Setters | typed, named after the `New…` struct's fields | `set("column", value)`, string-keyed |
| Defaults | `#[factory(email = "format!(\"user{n}@example.com\")")]`, where `n` is the row index | the faker, chosen by column name and type |
| Faker | none; the string is an ordinary Rust expression | built in and deterministic |
| Unsaved rows | `build()`, `build_many()` | `row()`, `insert_statement()` |
| Insert | `create(impl Executor<'_>)`, `create_many(…)` | `create(&Db)`, `create_many(&Db)` |
| Sequence closure | `Fn(usize, NewUser) -> NewUser` | `Fn(usize, &mut EntityFactory<E>)` |
| Parent rows | not handled | `plan_relations` and `FactoryRegistry` |

Because the derive generates an inherent `User::factory()`, that call resolves to the derive's
builder even when `moso_test::factory::Factory` is in scope. Write `EntityFactory::<User>::new()`
when you mean the harness's. Do not mix the two in one file expecting them to compose.

## Snapshots

There is **no snapshot API in `moso-test`**. No `assert_snapshot`, no golden-file helper, no `insta`
integration. The pattern that works today is a committed artefact plus an environment-variable
re-record, and it is worth writing for the OpenAPI document, because it turns an accidental API
change into a reviewable diff:

```rust title="tests/openapi.rs"
#[test]
fn the_committed_document_matches_the_application() {
    let generated = generated();

    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(committed_path(), &generated).expect("openapi.json is writable");
        return;
    }

    let committed = std::fs::read_to_string(committed_path()).expect(
        "openapi.json is missing; regenerate it with `UPDATE_OPENAPI=1 cargo test --test openapi`",
    );

    assert_eq!(
        committed, generated,
        "the committed `openapi.json` is out of date. If the change was intentional, \
         regenerate it and commit the diff."
    );
}
```

`TestApp::openapi()` gives you the document to serialise. The same shape works for any artefact you
want reviewed rather than silently regenerated. For compile-failure snapshots, the framework's own
suite uses `trybuild` with recorded `.stderr` files, re-recorded with `TRYBUILD=overwrite`.

## Failure modes

| Symptom | Cause |
| --- | --- |
| Log assertions all fail and `is_capturing()` is `false` | another global `tracing` subscriber was installed before the first `TestApp` spawned |
| `assert_queries!(&app, …)` reports zero statements | same cause, or `sqlx` query logging is off; use a `&TestDb` source instead |
| A startup hook never ran | the in-process transport does not run them; call `.bind()` |
| `bind()` returns an error about the `server` feature | `default-features = false` on `moso-test` |
| `advance_time` does not affect a timeout | framework internals read the real clock; only `Inject<TestClock>` code moves |
| A test passes alone and fails in the suite | shared state; give each test its own `TestDb` rather than truncating between tests |
| `Strategy::Transaction` breaks the code under test | it commits, or opens a second connection; switch to `Template` |
| The template has yesterday's schema | it should not: the fingerprint rebuilds it. If you built it by hand, drop it |
| `set("displayName", …)` panics | setters take the **column** name, not the field name |
| A 302 assertion sees the target page | it does not; redirects are never followed, and there is no option to |
| `assert_matches_openapi` says no path matches | the route is mounted but not documented, or the path is not in the document at all |

## See also

- [Errors](./errors.md) for the RFC 9457 problem documents `assert_problem` decodes.
- [Validation](./validation.md) for the field error codes `assert_field_error` takes.
- [OpenAPI](./openapi.md) for the document contract assertions read.
- [Dependency injection](./dependency-injection.md) for what `override_provider` is overriding.
- [Migrations](./migrations.md) for the migrator `TestDb` replays.
- [Relations](./relations.md) for the preloading `assert_queries!` points you at.
- [Observability](./observability.md) for the log fields the harness captures.
