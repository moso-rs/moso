# moso-test

**A harness that boots the real application, and assertions that print a report
instead of `left == right`.**

```toml
[dev-dependencies]
moso-test = "0.1"
```

```rust
use moso_test::prelude::*;

#[tokio::test]
async fn creating_a_user_returns_201() -> moso::Result<()> {
    let app = TestApp::builder().app(my_crate::app()).spawn().await?;

    app.client()
        .post("/users")
        .json(&serde_json::json!({ "username": "ada", "email": "ada@example.com" }))
        .send()
        .await
        .assert_status(201)
        .assert_header_present("location")
        .assert_json_path("/username", "ada")
        .assert_matches_openapi();

    app.logs().assert_no_errors();
    Ok(())
}
```

## The principle

A harness that constructs a parallel, simplified application tests the harness.
`TestApp` boots the **real** `moso::App`: the real provider map, the real
middleware stack, the real boot-time validation, the real OpenAPI document. A
missing provider fails the test with the same grouped boot report `main` would
have printed — so a test catches the misconfiguration that would otherwise be
found in staging.

## Two transports, one API

`TestClient` speaks to the application either **in process** — calling the
composed `tower::Service` directly, with no socket, no TCP handshake and no
second runtime — or over a **real bound port** with `reqwest`. The builder
chooses; the assertions do not change.

In-process is the default because it is roughly an order of magnitude faster and
removes a class of flake: no port to be taken, no accept queue to be full, no
connection to be reset. The socket transport (`server` feature, on by default)
exists for the tests that genuinely need the wire.

## What is in it

| Module | Contents |
| --- | --- |
| `app` | `TestApp`, `TestAppBuilder`, provider and dependency overrides |
| `client` | `TestClient`, `RequestBuilder`, `Multipart` |
| `response` | `TestResponse` and its assertions |
| `contract` | `assert_matches_openapi` — the response really is what the document claims |
| `battery` | `db()`, `kv()`, `jobs()`, `storage()`, `mail()` — typed handles to the batteries the app booted |
| `logs` | `LogAssertions` over the lines the application produced |
| `clock` | `TestClock`, so expiry and rate limits are assertions rather than sleeps |
| `diff` | the JSON diff a failed assertion renders |

`RequestBuilder::send` does not return a `Result`: a transport failure in a test
is a test failure, and it is reported the same way an assertion failure is —
with the request, the response and the server's own log lines. Use `try_send`
when the failure is what the test is about.

## Licence

MIT — see the root [`LICENSE`](../../LICENSE).
