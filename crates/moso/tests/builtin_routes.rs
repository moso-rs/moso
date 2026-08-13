//! The routes `App::build` mounts on its own: health, readiness, the OpenAPI
//! document and the documentation UI.
//!
//! These are mounted on an *outer* router, so a bug here does not show up in
//! any test that drives the application's own routes.

#![allow(dead_code)]

use moso::prelude::*;
use moso::response::NoContent;
use tower::ServiceExt;

/// A trivial operation, so the document is not empty.
#[endpoint]
async fn ping() -> Result<NoContent> {
    Ok(NoContent)
}

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

fn app() -> axum::Router<()> {
    App::new(Cfg::default())
        .mount(moso::routes! { GET "/ping" => ping })
        .build()
        .expect("builds")
        .into_service()
}

async fn send(path: &str) -> (u16, axum::http::HeaderMap, String) {
    let request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.expect("infallible");
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 22)
        .await
        .expect("body");
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

#[tokio::test]
async fn healthz_answers() {
    let (status, _, body) = send("/healthz").await;
    assert_eq!(status, 200, "{body}");
}

#[tokio::test]
async fn readyz_answers() {
    let (status, _, body) = send("/readyz").await;
    assert!(
        status == 200 || status == 503,
        "unexpected {status}: {body}"
    );
    assert!(body.contains(moso::VERSION), "{body}");
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn the_openapi_document_is_served_and_is_valid_json() {
    let (status, headers, body) = send("/openapi.json").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        headers.contains_key("etag"),
        "the document should be cacheable"
    );

    let document: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
    assert_eq!(
        document["openapi"].as_str().unwrap_or_default()[..3],
        *"3.1"
    );
    assert!(document["paths"]["/ping"].is_object(), "{body}");
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn the_docs_ui_is_served() {
    let (status, headers, body) = send("/docs").await;
    assert_eq!(status, 200);
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    assert!(body.contains("/openapi.json"), "the UI must load the spec");
}

#[test]
fn the_document_is_deterministic() {
    let first = serde_json::to_string(
        App::new(Cfg::default())
            .mount(moso::routes! { GET "/ping" => ping })
            .build()
            .expect("builds")
            .openapi(),
    )
    .expect("json");
    let second = serde_json::to_string(
        App::new(Cfg::default())
            .mount(moso::routes! { GET "/ping" => ping })
            .build()
            .expect("builds")
            .openapi(),
    )
    .expect("json");
    assert_eq!(first, second, "the document must be byte-stable (D15)");
}
