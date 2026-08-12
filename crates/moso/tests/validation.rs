//! Validation, from the wire to the problem document.
//!
//! Two claims are under test here and they are the ones the framework is sold
//! on:
//!
//! 1. **You cannot obtain a `T: Schema` from a request that did not validate.**
//!    There is no code path that produces one — the extractor validates, so the
//!    handler body never needs to.
//! 2. **A malformed body and an invalid body are different failures.** 400 says
//!    "I could not read this"; 422 says "I read it and it is wrong", and every
//!    wrong field is named with an RFC 6901 pointer and a stable code, all in
//!    one response.

#![allow(dead_code)]

use moso::prelude::*;
use moso::response::NoContent;

mod support;
use support::{field_errors, get, post_json, send};

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// A postal address, nested inside the sign-up body.
#[derive(Schema, Debug, Clone)]
pub struct Address {
    /// Two-letter country code.
    #[schema(len = 2..=2, pattern = r"^[A-Z]{2}$")]
    pub country: String,
    /// Postcode, in whatever the country uses.
    #[schema(len = 1..=12)]
    pub postcode: String,
}

/// Everything the sign-up endpoint accepts.
#[derive(Schema, Debug, Clone)]
pub struct SignUp {
    /// Public handle.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    /// Contact address.
    pub email: Email,
    /// Age in years.
    #[schema(range = 13..=130)]
    pub age: u8,
    /// Interests, each a non-empty tag, with no repeats.
    #[schema(unique, each(len = 1..=16))]
    pub tags: Vec<String>,
    /// Where the account holder lives.
    #[schema(nested)]
    pub address: Address,
    /// How many seats to buy, in whole dozens.
    #[schema(multiple_of = 12)]
    pub seats: Option<u32>,
}

/// A query with something to get wrong.
#[derive(Schema, Debug, Clone, Default)]
pub struct Paging {
    /// How many rows.
    #[schema(range = 1..=100)]
    pub limit: Option<u32>,
}

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

// ---------------------------------------------------------------------------
// The endpoints
// ---------------------------------------------------------------------------

/// Accept a sign-up. Reached only when the body already validated.
#[endpoint]
async fn sign_up(Json(body): Json<SignUp>) -> Result<Json<String>> {
    // Proof of claim 1: no `validate()` call here, and it cannot be reached
    // with a body that would have failed one.
    Ok(Json(body.username))
}

/// Page through something.
#[endpoint]
async fn page(Query(paging): Query<Paging>) -> Result<Json<u32>> {
    Ok(Json(paging.limit.unwrap_or(10)))
}

/// Take a path parameter that has to parse.
#[endpoint]
async fn show(Path(id): Path<u64>) -> Result<Json<u64>> {
    Ok(Json(id))
}

/// Accept nothing at all.
#[endpoint]
async fn noop() -> Result<NoContent> {
    Ok(NoContent)
}

fn app() -> axum::Router<()> {
    App::new(Cfg::default())
        .mount(moso::routes! {
            POST "/sign-up"    => sign_up,
            GET  "/page"       => page,
            GET  "/items/{id}" => show,
            POST "/noop"       => noop,
        })
        .build()
        .expect("builds")
        .into_service()
}

/// A body that is valid in every respect, as a baseline.
const GOOD: &str = r#"{
    "username": "ada_lovelace",
    "email": "ada@example.com",
    "age": 36,
    "tags": ["maths", "engines"],
    "address": { "country": "GB", "postcode": "SW1A1AA" },
    "seats": 24
}"#;

// ---------------------------------------------------------------------------
// The happy path, so the failures below mean something
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_valid_body_reaches_the_handler() {
    let reply = send(app(), post_json("/sign-up", GOOD)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.body, "\"ada_lovelace\"");
}

// ---------------------------------------------------------------------------
// 422: read, and wrong
// ---------------------------------------------------------------------------

#[tokio::test]
async fn three_broken_constraints_are_all_reported_in_one_response() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "A",
                "email": "ada@example.com",
                "age": 9,
                "tags": ["maths", "maths"],
                "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    assert!(
        reply
            .header("content-type")
            .starts_with("application/problem+json"),
        "{:?}",
        reply.header("content-type")
    );

    let problem = reply.json();
    assert_eq!(problem["status"], 422);
    assert_eq!(problem["title"], "Validation Failed");

    // Every broken rule, not the first: a client that has to fix one field per
    // round-trip is a client that shows one error per round-trip to a human.
    // Three fields are wrong and one of them breaks two rules, so four errors —
    // and `unique` points at the *duplicate's* index, not at the array.
    assert_eq!(
        field_errors(&problem),
        vec![
            ("/age".to_owned(), "range".to_owned()),
            ("/tags/1".to_owned(), "unique".to_owned()),
            ("/username".to_owned(), "len".to_owned()),
            ("/username".to_owned(), "pattern".to_owned()),
        ],
        "{}",
        reply.body
    );
    assert_eq!(problem["detail"], "4 fields failed validation");
}

#[tokio::test]
async fn a_field_error_carries_the_parameters_a_client_needs_to_render_it() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "ada_lovelace", "email": "ada@example.com", "age": 9,
                "tags": [], "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    let problem = reply.json();
    let age = problem["errors"]
        .as_array()
        .expect("errors")
        .iter()
        .find(|error| error["pointer"] == "/age")
        .expect("an error on /age")
        .clone();

    assert_eq!(age["code"], "range");
    assert_eq!(age["params"]["min"], 13, "{age}");
    assert_eq!(age["params"]["max"], 130, "{age}");
    assert!(
        age["message"].as_str().is_some_and(|m| !m.is_empty()),
        "a human-readable message is part of the contract: {age}"
    );
}

#[tokio::test]
async fn a_nested_error_points_at_the_nested_field() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "ada_lovelace", "email": "ada@example.com", "age": 36,
                "tags": [], "address": { "country": "gb", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(
        field_errors(&reply.json()),
        vec![("/address/country".to_owned(), "pattern".to_owned())],
        "the pointer has to reach into the nested object: {}",
        reply.body
    );
}

#[tokio::test]
async fn an_element_error_points_at_its_index() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "ada_lovelace", "email": "ada@example.com", "age": 36,
                "tags": ["maths", ""],
                "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(
        field_errors(&reply.json()),
        vec![("/tags/1".to_owned(), "len".to_owned())],
        "{}",
        reply.body
    );
}

#[tokio::test]
async fn a_pattern_and_a_length_on_one_field_both_report() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "AB", "email": "ada@example.com", "age": 36,
                "tags": [], "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    assert_eq!(
        field_errors(&reply.json()),
        vec![
            ("/username".to_owned(), "len".to_owned()),
            ("/username".to_owned(), "pattern".to_owned()),
        ],
        "both constraints on one field, not the first one only: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_multiple_of_violation_reports_its_divisor() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "ada_lovelace", "email": "ada@example.com", "age": 36,
                "tags": [], "address": { "country": "GB", "postcode": "SW1A1AA" },
                "seats": 25
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    let problem = reply.json();
    assert_eq!(
        field_errors(&problem),
        vec![("/seats".to_owned(), "multiple_of".to_owned())]
    );
    assert_eq!(problem["errors"][0]["params"]["multiple_of"], 12);
}

#[tokio::test]
async fn a_bad_format_is_a_422_at_the_field() {
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "ada_lovelace", "email": "not-an-email", "age": 36,
                "tags": [], "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    let errors = field_errors(&reply.json());
    assert_eq!(errors.len(), 1, "{}", reply.body);
    assert_eq!(errors[0].0, "/email");
}

#[tokio::test]
async fn a_missing_required_field_is_a_400_that_names_the_field() {
    // 400 and not 422: `SignUp` cannot be constructed at all, so this is a
    // *read* failure — the same class as `{not json`, caught by serde rather
    // than by a constraint. What it must still do is say which member, at a
    // pointer, which is the part a client can act on.
    let reply = send(
        app(),
        post_json("/sign-up", r#"{"username": "ada_lovelace"}"#),
    )
    .await;

    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(
        field_errors(&reply.json()),
        vec![("/email".to_owned(), "required".to_owned())],
        "{}",
        reply.body
    );
}

// ---------------------------------------------------------------------------
// 400: not read at all
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_syntactically_broken_body_is_a_400() {
    let reply = send(app(), post_json("/sign-up", "{not json at all")).await;

    assert_eq!(reply.status, 400, "{}", reply.body);
    let problem = reply.json();
    assert_eq!(problem["status"], 400);
    assert!(
        problem["detail"].as_str().is_some_and(|d| !d.is_empty()),
        "a 400 has to say what it could not read: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_wrongly_typed_member_is_a_400_with_a_pointer() {
    // The JSON is well-formed and the *shape* is wrong: serde cannot build the
    // struct at all, so this is a read failure and not a rule failure — but it
    // still knows exactly where it gave up.
    let reply = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "ada_lovelace", "email": "ada@example.com",
                "age": "thirty-six",
                "tags": [], "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(reply.status, 400, "{}", reply.body);
    assert!(
        reply.body.contains("age"),
        "a 400 from a typed read must still point at the member: {}",
        reply.body
    );
}

#[tokio::test]
async fn the_two_failures_are_told_apart_by_status_alone() {
    let malformed = send(app(), post_json("/sign-up", "{")).await;
    let invalid = send(
        app(),
        post_json(
            "/sign-up",
            r#"{
                "username": "A", "email": "ada@example.com", "age": 36,
                "tags": [], "address": { "country": "GB", "postcode": "SW1A1AA" }
            }"#,
        ),
    )
    .await;

    assert_eq!(malformed.status, 400);
    assert_eq!(invalid.status, 422);
    assert_ne!(
        malformed.json()["type"],
        invalid.json()["type"],
        "the two must also be distinguishable by `type`, for a client that \
         matches on the URI rather than the number"
    );
}

#[tokio::test]
async fn a_body_with_no_content_type_is_accepted() {
    // Documented, and deliberate: too many clients omit the header, and
    // refusing them buys nothing that the parse does not already buy.
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/sign-up")
        .body(axum::body::Body::from(GOOD))
        .expect("request");
    let reply = send(app(), request).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}

#[tokio::test]
async fn a_body_with_the_wrong_content_type_is_a_415() {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/sign-up")
        .header("content-type", "text/plain")
        .body(axum::body::Body::from(GOOD))
        .expect("request");
    let reply = send(app(), request).await;

    assert_eq!(reply.status, 415, "{}", reply.body);
    assert!(
        reply.body.contains("text/plain"),
        "the 415 must name what was sent: {}",
        reply.body
    );
}

#[tokio::test]
async fn a_json_suffix_content_type_is_accepted() {
    let request = axum::http::Request::builder()
        .method("POST")
        .uri("/sign-up")
        .header("content-type", "application/merge-patch+json")
        .body(axum::body::Body::from(GOOD))
        .expect("request");
    assert_eq!(send(app(), request).await.status, 200);
}

// ---------------------------------------------------------------------------
// The other extractors report the same way
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_invalid_query_parameter_points_into_the_query() {
    let reply = send(app(), get("/page?limit=500")).await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    let errors = field_errors(&reply.json());
    assert_eq!(errors.len(), 1, "{}", reply.body);
    assert!(
        errors[0].0.contains("limit"),
        "the pointer must name the parameter: {errors:?}"
    );
    assert_eq!(errors[0].1, "range");
}

#[tokio::test]
async fn an_unparseable_path_parameter_is_a_422_coded_type() {
    // The same rule as `Query`: a value of the wrong type is a rule failure
    // with a machine-readable code, not an opaque "bad request".
    let reply = send(app(), get("/items/not-a-number")).await;

    assert_eq!(reply.status, 422, "{}", reply.body);
    let errors = field_errors(&reply.json());
    assert_eq!(errors.len(), 1, "{}", reply.body);
    assert_eq!(errors[0].1, "type");
    assert!(
        errors[0].0.starts_with("/path"),
        "path pointers are rooted at `/path`: {errors:?}"
    );
}

// ---------------------------------------------------------------------------
// The document says all of this
// ---------------------------------------------------------------------------

#[test]
fn the_constraints_reach_the_generated_schema() {
    let app = App::new(Cfg::default())
        .mount(moso::routes! { POST "/sign-up" => sign_up })
        .build()
        .expect("builds");
    let document = serde_json::to_value(app.openapi()).expect("json");
    let schema = &document["components"]["schemas"]["SignUp"];

    assert_eq!(schema["properties"]["username"]["minLength"], 3);
    assert_eq!(schema["properties"]["username"]["maxLength"], 32);
    assert_eq!(schema["properties"]["username"]["pattern"], "^[a-z0-9_]+$");
    assert_eq!(schema["properties"]["age"]["minimum"], 13);
    assert_eq!(schema["properties"]["age"]["maximum"], 130);
    assert_eq!(schema["properties"]["tags"]["uniqueItems"], true);
    assert_eq!(schema["properties"]["seats"]["multipleOf"], 12);

    // D9: one attribute, two outputs. A schema that documents a constraint the
    // runtime does not enforce is a lie, and vice versa.
    assert!(
        schema["required"]
            .as_array()
            .expect("required")
            .iter()
            .any(|name| name == "email"),
        "{schema}"
    );
}

#[test]
fn the_operation_documents_both_failure_statuses() {
    let app = App::new(Cfg::default())
        .mount(moso::routes! { POST "/sign-up" => sign_up })
        .build()
        .expect("builds");
    let document = serde_json::to_value(app.openapi()).expect("json");
    let responses = &document["paths"]["/sign-up"]["post"]["responses"];

    assert!(
        responses["400"].is_object(),
        "the malformed-body failure is part of the contract: {responses}"
    );
    assert!(
        responses["422"].is_object(),
        "the invalid-body failure is part of the contract: {responses}"
    );
}
