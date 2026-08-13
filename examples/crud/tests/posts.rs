//! The posts API, driven end to end through the real application.
//!
//! Every test here boots the whole thing — configuration, provider map,
//! middleware stack, guards, router, document — and speaks HTTP to it. Nothing
//! is stubbed, so a test that passes here is evidence about the program that
//! ships.

mod support;

use serde_json::{Value, json};
use support::{API, TestApp};

// ---------------------------------------------------------------------------
// Creating
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_post_answers_201_with_a_location_and_a_slug() {
    let app = TestApp::new().await;

    let response = app
        .post(&format!("{API}/posts"))
        .author("ada")
        .json(&json!({
            "title": "Hello from Moso",
            "body": "The body.",
            "publish": true,
        }))
        .send()
        .await
        .assert_status(201)
        .assert_matches_openapi();

    assert_eq!(response.at("/title"), Value::from("Hello from Moso"));
    assert_eq!(response.at("/slug"), Value::from("hello-from-moso"));
    assert_eq!(response.at("/author"), Value::from("ada"));
    assert!(
        response.at("/published_at").is_string(),
        "`publish: true` sets the timestamp: {}",
        response.body
    );

    let id = response.at("/id");
    let location = response
        .headers
        .get("location")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    assert_eq!(
        location,
        format!("{API}/posts/{}", id.as_str().unwrap_or_default()),
        "the `Location` header points at the new resource"
    );
}

#[tokio::test]
async fn a_post_without_publish_is_a_draft() {
    let app = TestApp::new().await;
    let created = app.create_post("A draft", false).await;
    assert_eq!(created["published_at"], Value::Null);
}

#[tokio::test]
async fn two_posts_with_the_same_title_get_distinct_slugs() {
    let app = TestApp::new().await;
    let first = app.create_post("Hello", true).await;
    let second = app.create_post("Hello", true).await;

    assert_eq!(first["slug"], Value::from("hello"));
    assert_ne!(first["slug"], second["slug"], "a slug is unique");
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_short_title_is_a_422_at_the_exact_pointer_with_the_exact_code() {
    let app = TestApp::new().await;

    let response = app
        .post(&format!("{API}/posts"))
        .json(&json!({ "title": "ab", "body": "The body." }))
        .send()
        .await
        .assert_status(422)
        .assert_json_at("/errors/0/pointer", Value::from("/title"))
        .assert_json_at("/errors/0/code", Value::from("len"));

    assert_eq!(
        response.at("/status"),
        Value::from(422),
        "the body is an RFC 9457 problem document"
    );
    assert_eq!(
        response
            .headers
            .get("content-type")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .split(';')
            .next()
            .unwrap_or_default(),
        "application/problem+json"
    );
}

#[tokio::test]
async fn every_invalid_field_is_reported_at_once() {
    let app = TestApp::new().await;

    let response = app
        .post(&format!("{API}/posts"))
        .json(&json!({ "title": "ab", "body": "" }))
        .send()
        .await
        .assert_status(422);

    let pointers: Vec<String> = response
        .at("/errors")
        .as_array()
        .expect("a list of field errors")
        .iter()
        .map(|error| error["pointer"].as_str().unwrap_or_default().to_owned())
        .collect();

    assert!(
        pointers.contains(&"/title".to_owned()),
        "the too-short title is reported: {pointers:?}"
    );
    assert!(
        pointers.contains(&"/body".to_owned()),
        "and the empty body, in the same response: {pointers:?}"
    );
}

#[tokio::test]
async fn a_missing_required_field_is_a_400() {
    let app = TestApp::new().await;

    // 400 rather than 422, and deliberately so: a body missing a required
    // member never becomes a `CreatePost`, so this is a *deserialisation*
    // failure, not a constraint failure. 422 is reserved for a body that parsed
    // and then failed a rule — which is the distinction that lets a client tell
    // "my serialiser is wrong" from "my data is wrong".
    app.post(&format!("{API}/posts"))
        .json(&json!({ "title": "A perfectly good title" }))
        .send()
        .await
        .assert_status(400)
        .assert_json_at("/errors/0/code", Value::from("required"));
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_400_not_a_422() {
    let app = TestApp::new().await;

    app.post(&format!("{API}/posts"))
        .raw("{not json")
        .send()
        .await
        .assert_status(400);
}

#[tokio::test]
async fn a_query_parameter_outside_its_range_is_a_422() {
    let app = TestApp::new().await;

    app.get(&format!("{API}/posts?limit=1000"))
        .send()
        .await
        .assert_status(422)
        .assert_json_at("/errors/0/pointer", Value::from("/limit"))
        .assert_json_at("/errors/0/code", Value::from("range"));
}

#[tokio::test]
async fn a_cursor_this_api_did_not_issue_is_a_422() {
    let app = TestApp::new().await;

    app.get(&format!("{API}/posts?cursor=bm9uc2Vuc2U"))
        .send()
        .await
        .assert_status(422)
        .assert_json_at("/errors/0/pointer", Value::from("/cursor"));
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[tokio::test]
async fn fetching_a_post_that_does_not_exist_is_a_404() {
    let app = TestApp::new().await;

    let response = app
        .get(&format!("{API}/posts/00000000-0000-0000-0000-000000000000"))
        .send()
        .await
        .assert_status(404)
        .assert_matches_openapi();

    assert_eq!(response.at("/status"), Value::from(404));
    assert!(
        response
            .at("/detail")
            .as_str()
            .unwrap_or_default()
            .contains("00000000"),
        "the detail names what was asked for: {}",
        response.body
    );
}

#[tokio::test]
async fn an_identifier_that_is_not_a_uuid_is_a_422() {
    let app = TestApp::new().await;

    app.get(&format!("{API}/posts/not-a-uuid"))
        .send()
        .await
        .assert_status(422);
}

#[tokio::test]
async fn a_draft_is_invisible_to_everybody_but_its_author() {
    let app = TestApp::new().await;
    let draft = app.create_post("A draft", false).await;
    let id = draft["id"].as_str().expect("an id");

    app.get(&format!("{API}/posts/{id}"))
        .author("ada")
        .send()
        .await
        .assert_status(200);

    app.get(&format!("{API}/posts/{id}"))
        .author("grace")
        .send()
        .await
        .assert_status(404);

    app.get(&format!("{API}/posts/{id}"))
        .author("grace")
        .editor()
        .send()
        .await
        .assert_status(200);
}

// ---------------------------------------------------------------------------
// Listing and pagination
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_walks_two_pages_with_a_cursor_and_repeats_nothing() {
    let app = TestApp::new().await;
    for index in 0..5 {
        app.create_post(&format!("Post {index}"), true).await;
    }

    let first = app
        .get(&format!("{API}/posts?limit=2"))
        .send()
        .await
        .assert_status(200)
        .assert_matches_openapi();

    assert_eq!(
        first.at("/items").as_array().map(Vec::len),
        Some(2),
        "the page holds exactly the limit"
    );
    assert_eq!(
        first.at("/total"),
        Value::from(5),
        "the total counts every match, not the page"
    );
    assert_eq!(
        first.at("/items/0/title"),
        Value::from("Post 4"),
        "newest first: {}",
        first.body
    );

    let cursor = first
        .at("/next_cursor")
        .as_str()
        .expect("there is a next page")
        .to_owned();

    let second = app
        .get(&format!("{API}/posts?limit=2&cursor={cursor}"))
        .send()
        .await
        .assert_status(200)
        .assert_matches_openapi();

    assert_eq!(second.at("/items/0/title"), Value::from("Post 2"));
    assert_eq!(second.at("/items/1/title"), Value::from("Post 1"));

    let ids = |page: &support::TestResponse| -> Vec<String> {
        page.at("/items")
            .as_array()
            .expect("items")
            .iter()
            .map(|item| item["id"].as_str().unwrap_or_default().to_owned())
            .collect()
    };
    let (first_ids, second_ids) = (ids(&first), ids(&second));
    assert!(
        first_ids.iter().all(|id| !second_ids.contains(id)),
        "a cursor must not repeat a row: {first_ids:?} then {second_ids:?}"
    );

    // The last page has no cursor, which is how a client knows to stop.
    let last = app
        .get(&format!("{API}/posts?limit=10"))
        .send()
        .await
        .assert_status(200);
    assert_eq!(last.at("/next_cursor"), Value::Null);
}

#[tokio::test]
async fn the_page_size_falls_back_to_the_configured_default() {
    let app = TestApp::new().await;
    for index in 0..3 {
        app.create_post(&format!("Post {index}"), true).await;
    }

    // `posts.page_size` defaults to 20, so an unasked-for limit returns all
    // three rather than truncating.
    let response = app
        .get(&format!("{API}/posts"))
        .send()
        .await
        .assert_status(200);
    assert_eq!(response.at("/items").as_array().map(Vec::len), Some(3));
}

#[tokio::test]
async fn a_listing_is_filtered_by_a_case_insensitive_search() {
    let app = TestApp::new().await;
    app.create_post("Rust in anger", true).await;
    app.create_post("Something else", true).await;

    // `?search=RUST` is a case-insensitive substring of the title, compiled to a
    // SQL `lower(title) like lower('%RUST%')` by the ORM's `icontains`.
    let by_title = app
        .get(&format!("{API}/posts?search=RUST"))
        .send()
        .await
        .assert_status(200);
    assert_eq!(by_title.at("/items").as_array().map(Vec::len), Some(1));
    assert_eq!(by_title.at("/items/0/title"), Value::from("Rust in anger"));
}

#[tokio::test]
async fn drafts_stay_out_of_the_public_listing() {
    let app = TestApp::new().await;
    app.create_post("A draft", false).await;

    let anonymous = app
        .get(&format!("{API}/posts"))
        .send()
        .await
        .assert_status(200);
    assert_eq!(anonymous.at("/items").as_array().map(Vec::len), Some(0));

    let author = app
        .get(&format!("{API}/posts"))
        .author("ada")
        .send()
        .await
        .assert_status(200);
    assert_eq!(
        author.at("/items").as_array().map(Vec::len),
        Some(1),
        "an author sees their own drafts"
    );

    let editor = app
        .get(&format!("{API}/posts?drafts=true"))
        .author("grace")
        .editor()
        .send()
        .await
        .assert_status(200);
    assert_eq!(
        editor.at("/items").as_array().map(Vec::len),
        Some(1),
        "an editor may ask for every draft"
    );
}

// ---------------------------------------------------------------------------
// Updating, publishing and deleting
// ---------------------------------------------------------------------------

#[tokio::test]
async fn patching_changes_only_the_fields_that_were_sent() {
    let app = TestApp::new().await;
    let created = app.create_post("Original", true).await;
    let id = created["id"].as_str().expect("an id");

    let patched = app
        .patch(&format!("{API}/posts/{id}"))
        .author("ada")
        .json(&json!({ "title": "Rewritten" }))
        .send()
        .await
        .assert_status(200)
        .assert_matches_openapi();

    assert_eq!(patched.at("/title"), Value::from("Rewritten"));
    assert_eq!(
        patched.at("/body"),
        created["body"],
        "the body is untouched"
    );
    assert_eq!(
        patched.at("/author"),
        created["author"],
        "and so is every other field"
    );
}

#[tokio::test]
async fn an_empty_patch_is_the_applications_own_422() {
    let app = TestApp::new().await;
    let created = app.create_post("Original", true).await;
    let id = created["id"].as_str().expect("an id");

    let response = app
        .patch(&format!("{API}/posts/{id}"))
        .author("ada")
        .json(&json!({}))
        .send()
        .await
        .assert_status(422);

    assert!(
        response
            .at("/detail")
            .as_str()
            .unwrap_or_default()
            .contains("at least one field"),
        "the detail comes from `BlogError::NothingToUpdate`: {}",
        response.body
    );
}

#[tokio::test]
async fn somebody_elses_post_may_not_be_edited() {
    let app = TestApp::new().await;
    let created = app.create_post("Ada's post", true).await;
    let id = created["id"].as_str().expect("an id");

    app.patch(&format!("{API}/posts/{id}"))
        .author("grace")
        .json(&json!({ "title": "Mine now" }))
        .send()
        .await
        .assert_status(403);

    app.patch(&format!("{API}/posts/{id}"))
        .author("grace")
        .editor()
        .json(&json!({ "title": "Editorial change" }))
        .send()
        .await
        .assert_status(200);
}

#[tokio::test]
async fn publishing_requires_the_editor_dependency() {
    let app = TestApp::new().await;
    let created = app.create_post("A draft", false).await;
    let id = created["id"].as_str().expect("an id");

    let refused = app
        .post(&format!("{API}/posts/{id}/publish"))
        .author("ada")
        .send()
        .await
        .assert_status(403);
    assert!(
        refused
            .at("/detail")
            .as_str()
            .unwrap_or_default()
            .contains("only an editor"),
        "the message comes from `#[depends(error = …)]`: {}",
        refused.body
    );

    let published = app
        .post(&format!("{API}/posts/{id}/publish"))
        .author("grace")
        .editor()
        .send()
        .await
        .assert_status(200)
        .assert_matches_openapi();
    assert!(published.at("/published_at").is_string());
}

#[tokio::test]
async fn deleting_answers_204_and_then_404() {
    let app = TestApp::new().await;
    let created = app.create_post("Doomed", true).await;
    let id = created["id"].as_str().expect("an id");

    let response = app
        .delete(&format!("{API}/posts/{id}"))
        .author("ada")
        .send()
        .await
        .assert_status(204)
        .assert_matches_openapi();
    assert!(response.body.is_empty(), "a 204 carries no body");

    app.delete(&format!("{API}/posts/{id}"))
        .author("ada")
        .send()
        .await
        .assert_status(404);
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_write_without_the_api_key_is_a_401_and_never_reaches_the_handler() {
    let app = TestApp::new().await;

    app.request("POST", &format!("{API}/posts"))
        .json(&json!({ "title": "ab", "body": "" }))
        .send()
        .await
        .assert_status(401);

    // The body was invalid too. A 401 rather than a 422 proves the guard ran
    // before extraction, which is the point of a guard.
    let listing = app.get(&format!("{API}/posts")).send().await;
    assert_eq!(listing.at("/total"), Value::from(0));
}

#[tokio::test]
async fn a_wrong_api_key_is_a_401() {
    let app = TestApp::new().await;

    app.request("POST", &format!("{API}/posts"))
        .bad_key()
        .json(&json!({ "title": "A good title", "body": "…" }))
        .send()
        .await
        .assert_status(401);
}

#[tokio::test]
async fn reading_needs_no_key() {
    let app = TestApp::new().await;
    app.get(&format!("{API}/posts"))
        .send()
        .await
        .assert_status(200);
}

// ---------------------------------------------------------------------------
// The middleware
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_middleware_stamps_every_response_and_counts_every_request() {
    let app = TestApp::new().await;

    app.get(&format!("{API}/posts"))
        .send()
        .await
        .assert_status(200)
        .assert_header("x-app", "moso-crud");

    // Even a failure is stamped: the layer wraps the error path too.
    app.get(&format!("{API}/posts/not-a-uuid"))
        .send()
        .await
        .assert_status(422)
        .assert_header("x-app", "moso-crud");

    let status = app.get("/status").send().await.assert_status(200);
    assert_eq!(
        status.at("/requests"),
        Value::from(3),
        "two requests, plus the one asking: {}",
        status.body
    );
}

// ---------------------------------------------------------------------------
// The routes the framework mounts on its own
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_status_endpoint_reports_the_configured_name() {
    let app = TestApp::new().await;
    app.create_post("Hello", true).await;

    app.get("/status")
        .send()
        .await
        .assert_status(200)
        .assert_matches_openapi()
        .assert_json_at("/name", Value::from("moso blog"))
        .assert_json_at("/posts", Value::from(1));
}

#[tokio::test]
async fn health_and_readiness_answer() {
    let app = TestApp::new().await;

    app.get("/healthz").send().await.assert_status(200);

    let ready = app.get("/readyz").send().await;
    assert!(
        ready.status.as_u16() == 200 || ready.status.as_u16() == 503,
        "unexpected {}: {}",
        ready.status,
        ready.body
    );
}

#[tokio::test]
async fn the_document_and_the_docs_ui_are_served() {
    let app = TestApp::new().await;

    let document = app.get("/openapi.json").send().await.assert_status(200);
    assert_eq!(
        document.at("/info/title"),
        Value::from("Moso blog API"),
        "the served document is the one this application generated"
    );

    let ui = app.get("/docs").send().await.assert_status(200);
    assert!(ui.body.contains("/openapi.json"), "the UI loads the spec");
}

#[tokio::test]
async fn an_unknown_path_is_a_404_problem_and_a_wrong_method_is_a_405() {
    let app = TestApp::new().await;

    app.get("/nope").send().await.assert_status(404);
    app.request("PUT", &format!("{API}/posts"))
        .key()
        .json(&json!({}))
        .send()
        .await
        .assert_status(405);
}
