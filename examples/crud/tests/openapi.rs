//! The document is a build artefact, and it is committed.
//!
//! `openapi.json` in this directory was written by nobody: it is what
//! `App::build()` produced from the handlers. Committing it turns every change
//! to the API into a reviewable diff — a renamed field, a dropped parameter, a
//! response that quietly stopped being documented all show up in the pull
//! request rather than in a client's error log.
//!
//! Regenerate after an intentional change:
//!
//! ```text
//! UPDATE_OPENAPI=1 cargo test -p example-crud --test openapi
//! ```

mod support;

use std::path::PathBuf;

use serde_json::Value;
use support::TestApp;

/// The committed document.
fn committed_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("openapi.json")
}

/// The document this application generates, exactly as it would be committed.
async fn generated() -> String {
    let app = TestApp::new().await;
    let mut rendered = serde_json::to_string_pretty(app.document()).expect("the document renders");
    rendered.push('\n');
    rendered
}

#[tokio::test]
async fn the_committed_document_matches_the_application() {
    let generated = generated().await;

    if std::env::var_os("UPDATE_OPENAPI").is_some() {
        std::fs::write(committed_path(), &generated).expect("openapi.json is writable");
        return;
    }

    let committed = std::fs::read_to_string(committed_path()).expect(
        "openapi.json is missing; regenerate it with `UPDATE_OPENAPI=1 cargo test -p \
         example-crud --test openapi`",
    );

    assert_eq!(
        committed, generated,
        "the committed `openapi.json` is out of date. If the change was intentional, \
         regenerate it with `UPDATE_OPENAPI=1 cargo test -p example-crud --test openapi` and \
         commit the diff."
    );
}

#[tokio::test]
async fn the_document_is_openapi_3_1() {
    let app = TestApp::new().await;
    let version = app.document()["openapi"].as_str().unwrap_or_default();
    assert!(version.starts_with("3.1"), "{version}");
}

#[tokio::test]
async fn the_summary_of_each_operation_comes_from_its_doc_comment() {
    let app = TestApp::new().await;
    assert_eq!(
        app.document()["paths"]["/api/v1/posts"]["get"]["summary"],
        Value::from("List posts.")
    );
    assert_eq!(
        app.document()["paths"]["/api/v1/posts"]["post"]["summary"],
        Value::from("Create a post.")
    );
    assert!(
        app.document()["paths"]["/api/v1/posts"]["get"]["description"]
            .as_str()
            .unwrap_or_default()
            .contains("cursor-paginated"),
        "the rest of the doc comment becomes the description"
    );
}

#[tokio::test]
async fn every_query_parameter_is_documented_with_its_constraint() {
    let app = TestApp::new().await;
    let parameters = app.document()["paths"]["/api/v1/posts"]["get"]["parameters"]
        .as_array()
        .expect("the listing declares parameters")
        .clone();

    let named = |name: &str| -> Value {
        parameters
            .iter()
            .find(|parameter| parameter["name"] == *name)
            .cloned()
            .unwrap_or_else(|| panic!("no `{name}` parameter in {parameters:#?}"))
    };

    assert_eq!(named("limit")["schema"]["minimum"], Value::from(1));
    assert_eq!(named("limit")["schema"]["maximum"], Value::from(100));
    assert_eq!(named("search")["schema"]["maxLength"], Value::from(100));
    assert_eq!(named("cursor")["in"], Value::from("query"));
    assert_eq!(
        named("drafts")["schema"]["default"],
        Value::from(false),
        "`#[schema(default = false)]` reaches the document, not just the parser"
    );
    assert!(
        named("drafts")["required"].as_bool() != Some(true),
        "a parameter with a default is not required"
    );
}

#[tokio::test]
async fn the_request_schema_carries_the_constraints_the_handler_enforces() {
    let app = TestApp::new().await;
    let create = &app.document()["components"]["schemas"]["CreatePost"];

    assert_eq!(create["properties"]["title"]["minLength"], Value::from(3));
    assert_eq!(create["properties"]["title"]["maxLength"], Value::from(200));
    assert_eq!(
        create["required"],
        Value::from(vec!["title", "body"]),
        "`publish` has a default, so it is not required"
    );
}

#[tokio::test]
async fn a_guarded_operation_advertises_the_key_it_requires() {
    let app = TestApp::new().await;
    let document = app.document();

    assert_eq!(
        document["components"]["securitySchemes"]["api_key"]["name"],
        Value::from("x-api-key")
    );
    assert_eq!(
        document["paths"]["/api/v1/posts"]["post"]["security"],
        serde_json::json!([{ "api_key": [] }]),
        "the guard contributes the requirement to every route it protects"
    );
    assert!(
        document["paths"]["/api/v1/posts"]["post"]["responses"]["401"].is_object(),
        "…and the 401 it can return"
    );
}

#[tokio::test]
async fn an_unguarded_operation_requires_nothing() {
    let app = TestApp::new().await;
    assert!(
        app.document()["paths"]["/api/v1/posts"]["get"]["security"].is_null(),
        "reading is public"
    );
}

#[tokio::test]
async fn the_error_type_puts_its_statuses_in_the_document() {
    let app = TestApp::new().await;
    let responses = &app.document()["paths"]["/api/v1/posts/{id}"]["patch"]["responses"];

    // From `#[endpoint(errors = BlogError)]`: 404 and 409 are variants of the
    // application's own error enum, 422 comes from the validated body, and 429
    // from the router-level `.responds(…)`.
    for status in ["404", "409", "422", "429"] {
        assert!(
            responses[status].is_object(),
            "no {status} in {responses:#?}"
        );
    }
}

#[tokio::test]
async fn a_204_is_documented_without_a_body() {
    let app = TestApp::new().await;
    let response = &app.document()["paths"]["/api/v1/posts/{id}"]["delete"]["responses"]["204"];
    assert!(response.is_object(), "the delete documents its 204");
    assert!(
        response["content"].is_null(),
        "a 204 has no body: {response:#?}"
    );
}

#[tokio::test]
async fn the_created_response_documents_its_location_header() {
    let app = TestApp::new().await;
    let created = &app.document()["paths"]["/api/v1/posts"]["post"]["responses"]["201"];
    assert!(created["headers"]["location"].is_object(), "{created:#?}");
}

// ---------------------------------------------------------------------------
// The harness itself
// ---------------------------------------------------------------------------

/// A contract check that cannot fail is worse than no contract check, because
/// it reads like coverage. This proves the validator behind
/// `assert_matches_openapi()` actually rejects the two kinds of drift it exists
/// to catch.
#[tokio::test]
async fn the_contract_validator_rejects_the_drift_it_exists_to_catch() {
    let app = TestApp::new().await;
    let created = app.create_post("Hello", true).await;
    let schema = serde_json::json!({ "$ref": "#/components/schemas/PostOut" });

    assert!(
        support::validate(app.document(), &schema, &created).is_empty(),
        "a real response must satisfy its own published schema"
    );

    // A field the document does not mention.
    let mut extra = created.clone();
    extra["secret_internal_flag"] = Value::Bool(true);
    let violations = support::validate(app.document(), &schema, &extra);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("secret_internal_flag")),
        "{violations:?}"
    );

    // A field whose type changed under a client's feet.
    let mut retyped = created.clone();
    retyped["created_at"] = Value::from(1_700_000_000_u64);
    let violations = support::validate(app.document(), &schema, &retyped);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("/created_at")),
        "{violations:?}"
    );

    // A required member that went missing.
    let mut truncated = created;
    truncated.as_object_mut().expect("an object").remove("slug");
    let violations = support::validate(app.document(), &schema, &truncated);
    assert!(
        violations
            .iter()
            .any(|violation| violation.contains("slug")),
        "{violations:?}"
    );
}
