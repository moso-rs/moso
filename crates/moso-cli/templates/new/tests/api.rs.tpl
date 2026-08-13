//! The HTTP contract, exercised end to end.
//!
//! `App::into_service()` hands back the composed tower service — the real
//! router, the real middleware stack, the real dependency graph — so these
//! tests drive exactly what production serves without binding a port.

use moso::deps::axum::body::{Body, to_bytes};
use moso::deps::http::{Request, StatusCode};
use moso::deps::tower::ServiceExt;

/// Boot the application the way `main` does, then answer one request.
async fn send(request: Request<Body>) -> (StatusCode, String) {
    let service = @@LIB_NAME@@::build()
        .expect("the application builds")
        .into_service();
    let response = service.oneshot(request).await.expect("infallible");
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.expect("body");
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn get(path: &str) -> Request<Body> {
    Request::builder()
        .uri(path)
        .body(Body::empty())
        .expect("a valid request")
}

fn post_json(path: &str, body: &'static str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .expect("a valid request")
}

#[tokio::test]
async fn the_root_greets_the_world() {
    let (status, body) = send(get("/")).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("hello, world"), "{body}");
}

#[tokio::test]
async fn a_greeting_is_created() {
    let (status, body) = send(post_json("/greetings", r#"{"name":"ada"}"#)).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert!(body.contains("hello, ada"), "{body}");
}

#[tokio::test]
async fn an_invalid_body_is_rejected_with_a_pointer_to_the_field() {
    let (status, body) = send(post_json("/greetings", r#"{"name":""}"#)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    // RFC 9457 problem+json, with an RFC 6901 pointer at the offending field.
    assert!(body.contains("/name"), "{body}");
}

#[tokio::test]
async fn an_unknown_path_is_a_problem_document() {
    let (status, body) = send(get("/nope")).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[test]
fn every_route_is_documented() {
    let app = @@LIB_NAME@@::build().expect("the application builds");
    assert!(!app.router_info().is_empty(), "no routes registered");
    for route in app.router_info() {
        assert!(
            route.documented,
            "{} {} has no #[endpoint] description",
            route.method, route.path
        );
    }
}
