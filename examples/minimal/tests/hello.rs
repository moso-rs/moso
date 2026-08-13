//! The example, tested the way an application should be tested: over HTTP,
//! against the app `main` actually serves.
//!
//! `example_minimal::app()` here *is* the composition root the binary runs — the
//! binary is a five-line shim over it — so everything in between is real: the
//! provider map, the middleware stack, the router, and the generated OpenAPI
//! document. That is the whole reason for the lib/bin split.

use moso::deps::serde_json;
use moso_test::prelude::*;

/// Boot the example, with the greeting pinned so that a `GREETING` in the
/// developer's own environment cannot change what the tests expect.
///
/// `override_provider` replaces the configuration the application registered,
/// which is the general shape: the test edits the real builder rather than
/// assembling a second, simpler application.
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
        // Validate the body against the schema the document promises for this
        // operation. Every test is a contract test for the price of one line.
        .assert_matches_openapi();

    app.logs().assert_no_errors();
}

#[tokio::test]
async fn an_unknown_path_is_an_rfc_9457_problem_document() {
    let app = spawn().await;

    app.client()
        .get("/hello")
        .send()
        .await
        .assert_status(404)
        .assert_header("content-type", "application/problem+json")
        .assert_json_path("/status", 404)
        .assert_json_path("/title", "Not Found")
        .assert_json_path("/detail", "no route matches /hello");
}

#[tokio::test]
async fn the_operation_appears_in_the_openapi_document() {
    let app = spawn().await;

    // Served, and identical to `app.openapi()` — the document is assembled once,
    // at boot, and the route serves a pre-rendered copy of it.
    let response = app.client().get("/openapi.json").send().await;
    response.assert_status(200);
    let document = response.json_value();

    let operation = document
        .pointer("/paths/~1hello~1{name}/get")
        .unwrap_or_else(|| panic!("GET /hello/{{name}} is missing from the document: {document}"));

    // The summary is the handler's doc comment. Nothing in `lib.rs` says
    // "summary", "parameters" or "responses" anywhere.
    assert_eq!(operation["summary"], "Greet someone by name.");
    assert_eq!(operation["operationId"], "hello");
    assert_eq!(operation["parameters"][0]["name"], "name");
    assert_eq!(operation["parameters"][0]["in"], "path");
    assert_eq!(
        operation["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
        "#/components/schemas/Greeting"
    );

    // And the response schema is the `#[derive(Schema)]` type, field for field,
    // with the field doc comments as descriptions.
    let schema = &document["components"]["schemas"]["Greeting"];
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["properties"]["name"]["description"],
        "Who was greeted."
    );
    assert_eq!(schema["properties"]["message"]["type"], "string");
    assert_eq!(schema["required"], serde_json::json!(["name", "message"]));
}

/// The same request, over a real socket.
///
/// Everything above drives the composed `tower::Service` in process, which is
/// the fast path and covers the whole middleware stack. This one binds an
/// ephemeral port and speaks HTTP to it, so `serve` itself is covered too.
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
