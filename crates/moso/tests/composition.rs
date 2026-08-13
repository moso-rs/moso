//! Router composition: module-path handlers, nesting, merging, guards and
//! fallbacks — the parts of `routes!` and `Router` that only misbehave once
//! more than one module is involved.

#![allow(dead_code)]

use moso::prelude::*;
use moso::response::{Either, NoContent};
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// Handlers in a module, so `routes!` has a path to rewrite
// ---------------------------------------------------------------------------

/// The users area of the API.
pub mod users {
    use super::*;

    /// List users.
    #[endpoint]
    pub async fn list() -> Result<Json<Vec<String>>> {
        Ok(Json(vec!["ada".to_owned()]))
    }

    /// Show one user.
    #[endpoint]
    pub async fn show(Path(id): Path<u64>) -> Result<Json<u64>> {
        Ok(Json(id))
    }

    /// Show one of a user's posts, reached with a *tuple* `Path`.
    #[endpoint]
    pub async fn post(Path((id, slug)): Path<(u64, String)>) -> Result<Json<String>> {
        Ok(Json(format!("{id}:{slug}")))
    }

    /// This module's routes.
    pub fn router() -> Router {
        // Module-qualified idents: `routes!` must rewrite `users::list` to
        // `users::__moso_op_list`, preserving the path (contract D3).
        moso::routes! {
            GET "/"                  => list,
            GET "/{id}"              => show,
            GET "/{id}/posts/{slug}" => post,
        }
        .tag("users")
    }
}

/// The posts area of the API.
pub mod posts {
    use super::*;

    /// List posts.
    #[endpoint]
    pub async fn list() -> Result<Json<Vec<String>>> {
        Ok(Json(vec!["hello".to_owned()]))
    }
}

/// Routes registered from the crate root, naming handlers through their module.
fn root_router() -> Router {
    moso::routes! {
        GET "/posts" => posts::list,
    }
}

/// Either arm of a two-shaped response.
#[endpoint]
async fn maybe(Query(q): Query<Which>) -> Result<Either<Json<String>, NoContent>> {
    if q.empty.unwrap_or(false) {
        Ok(Either::B(NoContent))
    } else {
        Ok(Either::A(Json("body".to_owned())))
    }
}

/// Which arm to take.
#[derive(Schema, Debug, Clone, Default)]
pub struct Which {
    /// Return 204 instead of a body.
    pub empty: Option<bool>,
}

/// Report the pattern this request matched, and its request id.
///
/// Both come out of the request extensions, which is the failure mode that
/// degrades tracing and metrics without failing a single assertion elsewhere.
#[endpoint]
async fn whoami(
    matched: moso::extract::MatchedPath,
    id: moso::extract::RequestId,
) -> Result<Json<Vec<String>>> {
    Ok(Json(vec![matched.as_str().to_owned(), id.to_string()]))
}

/// The route a request that matches nothing lands on.
#[endpoint]
async fn not_found() -> Result<NoContent> {
    Err(Error::not_found("route"))
}

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

fn app() -> axum::Router<()> {
    let router = root_router()
        .nest("/users", users::router())
        .merge(moso::routes! {
            GET "/maybe" => maybe,
            GET "/whoami" => whoami,
        })
        .fallback(moso::ep!(not_found));

    App::new(Cfg::default())
        .mount(router)
        .build()
        .expect("builds")
        .into_service()
}

async fn send(path: &str) -> (u16, String) {
    let request = axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap();
    let response = app().oneshot(request).await.expect("infallible");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_module_qualified_handler_is_registered() {
    let (status, body) = send("/posts").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("hello"), "{body}");
}

#[tokio::test]
async fn a_nested_router_serves_under_its_prefix() {
    let (status, body) = send("/users").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("ada"), "{body}");
}

#[tokio::test]
async fn a_nested_path_parameter_still_resolves() {
    let (status, body) = send("/users/42").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "42", "{body}");
}

#[tokio::test]
async fn a_tuple_path_extractor_gets_both_captures() {
    let (status, body) = send("/users/7/posts/hello").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body, "\"7:hello\"", "{body}");
}

#[tokio::test]
async fn a_merged_router_is_reachable() {
    let (status, body) = send("/maybe").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("body"), "{body}");
}

#[tokio::test]
async fn either_takes_the_second_arm() {
    let (status, body) = send("/maybe?empty=true").await;
    assert_eq!(status, 204, "{body}");
}

#[tokio::test]
async fn the_fallback_handles_an_unmatched_path() {
    let (status, body) = send("/nothing-here").await;
    assert_eq!(status, 404, "{body}");
}

#[test]
fn nesting_rewrites_the_documented_paths() {
    let app = App::new(Cfg::default())
        .mount(root_router().nest("/users", users::router()))
        .build()
        .expect("builds");

    let json = serde_json::to_string(app.openapi()).expect("json");
    assert!(json.contains("/users/{id}"), "{json}");
    assert!(json.contains("/posts"), "{json}");
}

#[test]
fn two_handlers_named_list_do_not_collide() {
    // `users::list` and `posts::list` generate `__moso_op_list` in *different*
    // modules; a flat registry would have made this a duplicate-operationId
    // boot error.
    let app = App::new(Cfg::default())
        .mount(root_router().nest("/users", users::router()))
        .build()
        .expect("distinct modules must not collide");
    assert_eq!(app.router_info().len(), 4);
}

#[tokio::test]
async fn the_matched_pattern_and_request_id_reach_the_handler() {
    let (status, body) = send("/whoami").await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("/whoami"),
        "the matched pattern, not the concrete path: {body}"
    );
    // A ULID, generated by the request-id middleware.
    assert!(body.len() > 30, "{body}");
}
