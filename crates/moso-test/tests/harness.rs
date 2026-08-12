//! The harness, tested against a real application.
//!
//! Everything in `src/` is unit-tested in isolation, and none of that proves the
//! thing that matters: that [`TestApp`] boots a real `App` built out of the real
//! macros, that [`TestClient`] reaches its handlers over both transports, and
//! that the assertions fire — and *do not* fire — on the right responses.
//!
//! So this file builds an application with `#[endpoint]`, `routes!`,
//! `#[derive(Schema)]`, `#[derive(Config)]` and dependency injection, and drives
//! it the way a user's test suite would.

#![allow(dead_code)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use moso::prelude::*;
use moso::response::NoContent;
use moso_test::prelude::*;
use moso_test::{TestApp, contract};
use serde_json::json;

// ---------------------------------------------------------------------------
// The application under test
// ---------------------------------------------------------------------------

/// A user, as the API accepts one.
#[derive(Schema, Debug, Clone, PartialEq)]
pub struct CreateUser {
    /// Public handle.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    /// Contact address.
    pub email: Email,
}

/// A user, as the API returns one.
#[derive(Schema, Debug, Clone, PartialEq)]
pub struct UserOut {
    /// Stable identifier.
    pub id: u64,
    /// Public handle.
    pub username: String,
}

/// An in-memory store, provided once at boot.
#[derive(Debug, Default)]
pub struct Store {
    next_id: AtomicU32,
}

impl Store {
    fn allocate(&self) -> u64 {
        u64::from(self.next_id.fetch_add(1, Ordering::Relaxed)) + 1
    }
}

/// Something a test overrides, to prove overriding works.
#[derive(Debug)]
pub struct Greeting(pub String);

/// The application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct AppConfig {
    /// Unused here; present so the application has a real configuration type.
    #[config(default = "moso-test")]
    pub name: String,
}

/// List every user.
#[endpoint]
async fn list(Inject(_store): Inject<Store>) -> Result<Json<Vec<UserOut>>> {
    Ok(Json(vec![UserOut {
        id: 1,
        username: "ada".to_owned(),
    }]))
}

/// Create a user.
#[endpoint]
async fn create(
    Inject(store): Inject<Store>,
    Json(body): Json<CreateUser>,
) -> Result<Created<UserOut>> {
    let id = store.allocate();
    Ok(Created::at(
        format!("/users/{id}"),
        UserOut {
            id,
            username: body.username,
        },
    ))
}

/// Show one user.
#[endpoint]
async fn show(Path(id): Path<u64>) -> Result<Json<UserOut>> {
    if id == 0 {
        return Err(Error::not_found("user"));
    }
    Ok(Json(UserOut {
        id,
        username: format!("user{id}"),
    }))
}

/// Delete a user.
#[endpoint]
async fn destroy(Path(_id): Path<u64>) -> Result<NoContent> {
    Ok(NoContent)
}

/// Greet, using whatever `Greeting` is provided.
#[endpoint]
async fn greet(Inject(greeting): Inject<Greeting>) -> Result<Json<UserOut>> {
    Ok(Json(UserOut {
        id: 0,
        username: greeting.0.clone(),
    }))
}

/// Log at every level, so the capture layer has something to catch.
#[endpoint]
async fn noisy() -> Result<NoContent> {
    tracing::warn!(target: "app::noisy", limit = 10, "rate limit exceeded");
    tracing::error!(target: "app::noisy", "something went wrong");
    Ok(NoContent)
}

/// Echo the request's correlation id back, to prove the header survives.
///
/// Reads the header rather than `RequestId`, which parses a ULID: the harness
/// issues human-readable ids so that a failure report can be searched for.
#[endpoint]
async fn echo_id(headers: moso::deps::http::HeaderMap) -> Result<Json<UserOut>> {
    let id = headers
        .get(moso::REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    Ok(Json(UserOut {
        id: 0,
        username: id,
    }))
}

/// Take longer than any test will wait, so a deadline can be observed.
#[endpoint]
async fn slow() -> Result<NoContent> {
    tokio::time::sleep(Duration::from_secs(30)).await;
    Ok(NoContent)
}

fn router() -> Router {
    moso::routes! {
        GET    "/slow"        => slow,
        GET    "/users"       => list,
        POST   "/users"       => create,
        GET    "/users/{id}"  => show,
        DELETE "/users/{id}"  => destroy,
        GET    "/greet"       => greet,
        GET    "/noisy"       => noisy,
        GET    "/echo-id"     => echo_id,
    }
    .tag("users")
}

/// The composition root, exactly as an application would expose it.
fn app() -> moso::AppBuilder {
    App::new(AppConfig::default())
        .provide(Store::default())
        .provide(Greeting("production".to_owned()))
        .mount(router())
}

async fn spawn() -> TestApp {
    TestApp::spawn(app()).await.expect("the application boots")
}

// ---------------------------------------------------------------------------
// Booting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_test_app_boots_the_real_application() {
    let app = spawn().await;
    assert_eq!(app.base_url().as_str(), "http://localhost/");
    assert!(app.local_addr().is_none(), "in-process mode binds nothing");
    assert!(app.service().is_some());
    assert!(app.openapi().paths.contains_key("/users"));
    assert_eq!(app.state().profile(), moso::config::Profile::Test);
}

#[tokio::test]
async fn a_boot_failure_is_the_applications_own_report() {
    // The `Store` provider is missing, so the DI graph does not validate.
    let error = TestApp::builder()
        .app(App::new(AppConfig::default()).mount(router()))
        .spawn()
        .await
        .expect_err("the graph is incomplete");
    let rendered = error.to_string();
    assert!(rendered.contains("Store"), "{rendered}");
}

#[tokio::test]
async fn the_test_app_macro_boots_from_a_composition_root() {
    let app = moso_test::test_app!(app()).await.expect("boots");
    app.client().get("/users").send().await.assert_status(200);
}

#[tokio::test]
async fn the_zero_argument_macro_calls_app_in_scope() {
    let app = moso_test::test_app!().await.expect("boots");
    app.client().get("/users").send().await.assert_status(200);
}

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

    one.client()
        .get("/greet")
        .send()
        .await
        .assert_json_path("/username", "one");
    two.client()
        .get("/greet")
        .send()
        .await
        .assert_json_path("/username", "two");

    // Logs stay separate: only the app that served the request holds its lines.
    one.client().get("/noisy").send().await.assert_status(204);
    assert!(
        one.logs()
            .records()
            .iter()
            .any(|record| record.contains("rate limit"))
    );
    assert!(
        !two.logs()
            .records()
            .iter()
            .any(|record| record.contains("rate limit"))
    );
}

// ---------------------------------------------------------------------------
// The client
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_get_reaches_its_handler() {
    let app = spawn().await;
    app.client()
        .get("/users")
        .send()
        .await
        .assert_status(200)
        .assert_header_contains("content-type", "application/json")
        .assert_json_path("/0/username", "ada");
}

#[tokio::test]
async fn a_path_parameter_is_extracted() {
    let app = spawn().await;
    app.client()
        .get("/users/7")
        .send()
        .await
        .assert_status(200)
        .assert_json_path("/id", 7);
}

#[tokio::test]
async fn a_json_body_round_trips_and_the_location_header_is_set() {
    let app = spawn().await;
    let response = app
        .client()
        .post("/users")
        .json(&json!({ "username": "ada_l", "email": "ada@example.com" }))
        .send()
        .await;

    response
        .assert_status(201)
        .assert_header_present("location")
        .assert_json_path("/username", "ada_l");

    let user: UserOut = response.json();
    assert_eq!(user.username, "ada_l");
}

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

#[tokio::test]
async fn a_handler_error_is_a_problem_document() {
    let app = spawn().await;
    app.client()
        .get("/users/0")
        .send()
        .await
        .assert_status(404)
        .assert_problem("not-found")
        .assert_problem("https://moso.rs/errors/not-found");
}

#[tokio::test]
async fn a_no_content_response_has_an_empty_body() {
    let app = spawn().await;
    app.client()
        .delete("/users/3")
        .send()
        .await
        .assert_status(204)
        .assert_empty_body();
}

#[tokio::test]
async fn an_unknown_route_is_a_404_and_a_wrong_method_a_405() {
    let app = spawn().await;
    app.client().get("/nope").send().await.assert_status(404);
    app.client()
        .post("/users/1")
        .send()
        .await
        .assert_status(405);
}

#[tokio::test]
async fn query_parameters_are_encoded_onto_the_url() {
    let app = spawn().await;
    let request = app
        .client()
        .get("/users")
        .query_pair("limit", "10")
        .query(&[("cursor", "a b")]);
    let url = request.url();
    assert_eq!(url.path(), "/users");
    let query = url.query().expect("a query string");
    assert!(query.contains("limit=10"), "{query}");
    assert!(query.contains("cursor=a+b"), "{query}");
    request.send().await.assert_status(200);
}

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

#[tokio::test]
async fn an_anonymous_client_drops_the_credentials() {
    let app = spawn().await;
    let authenticated = app.as_bearer("token-1").with_cookie("session", "abc");
    let response = authenticated.anonymous().get("/users").send().await;
    assert!(
        !response
            .request()
            .headers
            .iter()
            .any(|(name, _)| name == "authorization" || name == "cookie")
    );
    // The app's own client was never mutated.
    assert!(app.client().headers().is_empty());
}

#[tokio::test]
async fn a_form_body_is_encoded_as_urlencoded() {
    let app = spawn().await;
    let response = app
        .client()
        .post("/users")
        .form(&[("username", "ada_l"), ("email", "ada@example.com")])
        .send()
        .await;
    assert_eq!(
        response.request().content_type(),
        Some("application/x-www-form-urlencoded")
    );
    // The endpoint only accepts JSON, so this is a 415 — which is itself the
    // proof that the form body reached the extractor.
    response.assert_status(415);
}

#[tokio::test]
async fn a_multipart_body_is_sent_with_its_boundary() {
    let app = spawn().await;
    let form = Multipart::new().text("title", "hello").file(
        "avatar",
        "a.png",
        "image/png",
        &b"\x89PNG"[..],
    );
    let response = app.client().post("/users").multipart(form).send().await;
    let content_type = response.request().content_type().unwrap_or_default();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    assert_eq!(response.status().as_u16(), 415);
}

#[tokio::test]
async fn the_generated_request_id_is_the_one_the_server_sees() {
    let app = spawn().await;
    let response = app.client().get("/echo-id").send().await;
    let sent = response.request_id().to_owned();

    // The harness issues ULID-shaped ids precisely so the request-id middleware
    // adopts them rather than replacing them. If this ever regresses, every
    // failure report silently loses its "server logs" section.
    response
        .assert_status(200)
        .assert_json_path("/username", sent.as_str())
        .assert_header("x-request-id", &sent);
}

#[tokio::test]
async fn an_id_the_middleware_refuses_still_files_its_logs_correctly() {
    let app = spawn().await;
    // Not a ULID, so `moso_core::middleware::request_id` generates its own.
    let response = app
        .client()
        .get("/noisy")
        .request_id("not-a-ulid")
        .send()
        .await;
    response.assert_status(204);
    assert_ne!(response.header("x-request-id"), Some("not-a-ulid"));

    // Attribution still works, because the in-process capture layer names the
    // request itself rather than trusting the id the stack settled on.
    assert!(
        response
            .logs()
            .iter()
            .any(|record| record.contains("rate limit")),
        "captured: {}",
        app.logs().dump()
    );
}

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
    assert!(report.contains("could not be sent"), "{report}");
}

// ---------------------------------------------------------------------------
// The bound-port transport
// ---------------------------------------------------------------------------

#[cfg(feature = "server")]
#[tokio::test]
async fn the_socket_transport_serves_on_a_real_port() {
    let app = TestApp::builder()
        .app(app())
        .bind()
        .spawn()
        .await
        .expect("binds and serves");

    let addr = app.local_addr().expect("a bound address");
    assert_eq!(addr.ip().to_string(), "127.0.0.1");
    assert_ne!(addr.port(), 0);
    assert_eq!(app.base_url().as_str(), format!("http://{addr}/"));
    assert!(app.service().is_none(), "serve_on consumed the App");

    app.client()
        .get("/users")
        .send()
        .await
        .assert_status(200)
        .assert_json_path("/0/username", "ada");

    app.client()
        .post("/users")
        .json(&json!({ "username": "ada_l", "email": "ada@example.com" }))
        .send()
        .await
        .assert_status(201);

    app.shutdown().await;
}

#[cfg(feature = "server")]
#[tokio::test]
async fn logs_are_captured_over_the_socket_transport_too() {
    let app = TestApp::builder()
        .app(app())
        .bind()
        .spawn()
        .await
        .expect("binds and serves");

    let response = app.client().get("/noisy").send().await;
    response.assert_status(204);

    // Attribution over a socket goes through the `x-request-id` header, which
    // Moso's trace layer records on its span.
    let lines = response.logs();
    assert!(
        lines.iter().any(|record| record.contains("rate limit")),
        "captured: {:?}",
        app.logs().dump()
    );
    app.shutdown().await;
}

#[cfg(not(feature = "server"))]
#[tokio::test]
async fn binding_without_the_server_feature_is_a_sentence_not_a_panic() {
    let error = TestApp::builder()
        .app(app())
        .bind()
        .spawn()
        .await
        .expect_err("there is no client to talk to the port with");
    assert!(error.to_string().contains("`server` feature"), "{error}");
}

// ---------------------------------------------------------------------------
// Log assertions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn log_assertions_see_the_handlers_own_lines() {
    let app = spawn().await;
    app.client().get("/noisy").send().await.assert_status(204);

    assert!(app.logs().is_capturing());
    app.logs()
        .assert_contains(Level::WARN, "rate limit")
        .assert_contains(Level::ERROR, "something went wrong")
        .assert_contains_at_least(Level::WARN, "rate limit")
        .assert_none_containing(Level::WARN, "no such line");
}

#[tokio::test]
async fn log_lines_are_filed_under_the_request_that_produced_them() {
    let app = spawn().await;
    let quiet = app.client().get("/users").send().await;
    let loud = app.client().get("/noisy").send().await;

    assert!(
        loud.logs()
            .iter()
            .any(|record| record.contains("rate limit"))
    );
    assert!(
        !quiet
            .logs()
            .iter()
            .any(|record| record.contains("rate limit"))
    );
    assert!(
        loud.logs()
            .iter()
            .all(|record| record.request_id.as_deref() == Some(loud.request_id()))
    );
}

#[tokio::test]
async fn assert_no_errors_passes_when_nothing_logged_an_error() {
    let app = spawn().await;
    app.client().get("/users").send().await.assert_status(200);
    app.logs().assert_no_errors();
}

#[tokio::test]
async fn clearing_the_buffer_forgets_the_arrange_phase() {
    let app = spawn().await;
    app.client().get("/noisy").send().await;
    assert!(!app.logs().is_empty());
    app.logs().clear();
    assert!(app.logs().is_empty());
    app.logs().assert_no_errors();
}

#[tokio::test]
#[should_panic(expected = "expected no ERROR log lines")]
async fn assert_no_errors_fails_and_prints_the_buffer() {
    let app = spawn().await;
    app.client().get("/noisy").send().await;
    app.logs().assert_no_errors();
}

// ---------------------------------------------------------------------------
// Failure output — the actual product
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_failed_status_assertion_prints_request_response_and_logs() {
    let app = spawn().await;
    let response = app
        .client()
        .post("/users")
        .json(&json!({ "username": "A", "email": "ada@example.com" }))
        .send()
        .await;

    let report = response.report(
        "expected status 201 Created, got 422 Unprocessable Entity",
        &[],
    );

    // The request.
    assert!(report.contains("POST http://localhost/users"), "{report}");
    assert!(report.contains("request body:"), "{report}");
    assert!(report.contains("\"username\": \"A\""), "{report}");
    // The response.
    assert!(report.contains("422 Unprocessable Entity"), "{report}");
    assert!(report.contains("response headers:"), "{report}");
    assert!(report.contains("\"pointer\": \"/username\""), "{report}");
    // The server's own view.
    assert!(
        report.contains(&format!(
            "server logs for request_id {}",
            response.request_id()
        )),
        "{report}"
    );
    assert!(report.contains("in-process"), "{report}");
}

#[tokio::test]
#[should_panic(expected = "expected status 201 Created, got 200 OK")]
async fn a_failed_status_assertion_panics_with_the_report() {
    let app = spawn().await;
    app.client().get("/users").send().await.assert_status(201);
}

#[tokio::test]
#[should_panic(expected = "json diff")]
async fn a_failed_json_path_assertion_prints_a_diff() {
    let app = spawn().await;
    app.client()
        .get("/users/7")
        .send()
        .await
        .assert_json_path("/id", 8);
}

#[tokio::test]
#[should_panic(expected = "the body has nothing at `/missing`")]
async fn a_missing_json_path_says_so() {
    let app = spawn().await;
    app.client()
        .get("/users/7")
        .send()
        .await
        .assert_json_path("/missing", 1);
}

#[tokio::test]
#[should_panic(expected = "expected a field error at `/email` with code `format`")]
async fn a_missing_field_error_lists_the_ones_present() {
    let app = spawn().await;
    app.client()
        .post("/users")
        .json(&json!({ "username": "A", "email": "ada@example.com" }))
        .send()
        .await
        .assert_field_error("/email", "format");
}

#[tokio::test]
#[should_panic(expected = "expected header `location`")]
async fn a_missing_header_is_named() {
    let app = spawn().await;
    app.client()
        .get("/users")
        .send()
        .await
        .assert_header("location", "/users/1");
}

// ---------------------------------------------------------------------------
// JSON assertions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn json_matching_is_a_subset_and_json_eq_is_not() {
    let app = spawn().await;
    let response = app.client().get("/users/7").send().await;
    response
        .assert_json_matches(json!({ "id": 7 }))
        .assert_json_eq(json!({ "id": 7, "username": "user7" }))
        .assert_no_json_path("/email");
}

#[tokio::test]
#[should_panic(expected = "the body is not the expected document")]
async fn json_eq_rejects_an_extra_member() {
    let app = spawn().await;
    app.client()
        .get("/users/7")
        .send()
        .await
        .assert_json_eq(json!({ "id": 7 }));
}

// ---------------------------------------------------------------------------
// Contract assertions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_documented_response_matches_its_schema() {
    let app = spawn().await;
    app.client()
        .get("/users/7")
        .send()
        .await
        .assert_status(200)
        .assert_matches_openapi();

    app.client()
        .get("/users")
        .send()
        .await
        .assert_matches_openapi();

    app.client()
        .post("/users")
        .json(&json!({ "username": "ada_l", "email": "ada@example.com" }))
        .send()
        .await
        .assert_status(201)
        .assert_matches_openapi();
}

#[tokio::test]
async fn every_response_can_be_checked_automatically() {
    let app = TestApp::builder()
        .app(app())
        .assert_openapi(contract::Options::strict())
        .spawn()
        .await
        .expect("boots");
    // No explicit `assert_matches_openapi`: the client does it on every send.
    app.client().get("/users/7").send().await.assert_status(200);
}

#[tokio::test]
#[should_panic(expected = "no path matching `/nope`")]
async fn an_undocumented_path_is_drift() {
    let app = spawn().await;
    app.client()
        .get("/nope")
        .send()
        .await
        .assert_matches_openapi();
}

/// Acceptance criterion 7 of `43-testing.md`, end to end: a handler returning a
/// field the document does not describe must fail the contract assertion.
#[tokio::test]
#[should_panic(expected = "the response carries a property the document does not describe")]
async fn an_undocumented_field_fails_the_contract_assertion() {
    /// Documented as returning a `UserOut`…
    #[endpoint]
    async fn drifted() -> Result<Json<UserOut>> {
        Ok(Json(UserOut {
            id: 1,
            username: "ada".to_owned(),
        }))
    }

    // …but actually serving a body with an extra member. Mounting the honest
    // endpoint for the document and an Axum route for the bytes is the only way
    // to build drift in a framework that generates the document from the type.
    let extra = moso::deps::axum::Router::new().route(
        "/drifted",
        moso::deps::axum::routing::get(|| async {
            moso::deps::axum::Json(json!({ "id": 1, "username": "ada", "secret": "oops" }))
        }),
    );

    let app = TestApp::builder()
        .app(app())
        .mount(moso::routes! { GET "/drifted" => drifted })
        .customise(move |builder| builder.mount_axum("/shadow", extra))
        .spawn()
        .await
        .expect("boots");

    // Reach the drifting bytes, but judge them against `/drifted`'s schema by
    // asking the harness to validate the shadow path's body against it.
    let response = app.client().get("/shadow/drifted").send().await;
    let document = app.openapi();
    let schema = document
        .operation(moso::openapi::HttpMethod::Get, "/drifted")
        .and_then(|operation| operation.responses.get("200"))
        .and_then(|spec| spec.content.get("application/json"))
        .and_then(|media| media.schema.as_ref())
        .expect("the endpoint documents a 200 body");

    let violations = contract::validate(
        document,
        schema,
        &response.json_value(),
        contract::Options::strict(),
    );
    assert_eq!(violations.len(), 1, "{violations:?}");
    panic!("{}", violations[0].message);
}

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn advancing_time_moves_the_provided_clock() {
    let app = spawn().await;
    let clock: Arc<TestClock> = app.resolver().get().expect("the harness provides one");
    let before = clock.now();

    app.advance_time(Duration::from_secs(3600)).await;

    assert_eq!(clock.now(), before + Duration::from_secs(3600));
    assert_eq!(app.clock().now(), clock.now());
}

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

// ---------------------------------------------------------------------------
// Overrides
// ---------------------------------------------------------------------------

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

#[tokio::test]
async fn an_extra_router_can_be_mounted_for_the_test() {
    /// A route that only exists in tests.
    #[endpoint]
    async fn probe() -> Result<NoContent> {
        Ok(NoContent)
    }

    let app = TestApp::builder()
        .app(app())
        .mount_at("/testing", moso::routes! { GET "/probe" => probe })
        .spawn()
        .await
        .expect("boots");
    app.client()
        .get("/testing/probe")
        .send()
        .await
        .assert_status(204);
    assert!(app.openapi().paths.contains_key("/testing/probe"));
}

#[tokio::test]
async fn the_http_config_can_be_replaced_for_one_test() {
    let app = TestApp::builder()
        .app(app())
        .http_config(moso::http_config::HttpConfig {
            body_max: 8,
            ..moso::http_config::HttpConfig::default()
        })
        .spawn()
        .await
        .expect("boots");

    assert_eq!(app.state().http().body_max, 8);
    app.client()
        .post("/users")
        .json(&json!({ "username": "ada_l", "email": "ada@example.com" }))
        .send()
        .await
        .assert_status(413);
}

#[tokio::test]
async fn http_config_edits_compose_instead_of_clobbering() {
    let app = TestApp::builder()
        .app(app())
        .http_config(moso::http_config::HttpConfig {
            body_max: 4096,
            ..moso::http_config::HttpConfig::default()
        })
        .http_config_with(|http| http.uri_max = 1024)
        .expose_internal_errors()
        .spawn()
        .await
        .expect("boots");

    let http = app.state().http();
    assert_eq!(http.body_max, 4096, "the base survived the edits");
    assert_eq!(http.uri_max, 1024, "the first edit survived the second");
    assert!(http.expose_internal_errors, "the second edit applied");
}

#[tokio::test]
async fn a_default_header_is_sent_on_every_request() {
    let app = TestApp::builder()
        .app(app())
        .default_header("x-tenant", "acme")
        .spawn()
        .await
        .expect("boots");
    let response = app.client().get("/users").send().await;
    assert!(
        response
            .request()
            .headers
            .iter()
            .any(|(name, value)| name == "x-tenant" && value == "acme")
    );
}
