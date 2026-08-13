//! The integration test the workspace did not have: a real application, built
//! out of the real macros, served through the real router.
//!
//! Every crate's unit tests pass in isolation. What none of them prove is that
//! `#[endpoint]`'s output resolves against `moso::__private`, that `routes!`
//! produces a `Router` the `App` accepts, or that a `#[derive(Schema)]` type
//! rejects a bad body with a 422 at the right JSON Pointer. That is what this
//! file is for.

#![allow(dead_code)]

use std::sync::atomic::{AtomicU32, Ordering};

use moso::prelude::*;
use moso::response::NoContent;
use tower::ServiceExt;

// ---------------------------------------------------------------------------
// The model
// ---------------------------------------------------------------------------

/// A user, as the API accepts one.
#[derive(Schema, Debug, Clone, PartialEq)]
pub struct CreateUser {
    /// Public handle.
    #[schema(len = 3..=32, pattern = r"^[a-z0-9_]+$")]
    pub username: String,
    /// Contact address.
    pub email: Email,
    /// Optional age, in years.
    #[schema(range = 13..=130)]
    pub age: Option<u8>,
}

/// A user, as the API returns one.
#[derive(Schema, Debug, Clone, PartialEq)]
pub struct UserOut {
    /// Stable identifier.
    pub id: u64,
    /// Public handle.
    pub username: String,
}

// ---------------------------------------------------------------------------
// The application's state and dependencies
// ---------------------------------------------------------------------------

/// An in-memory user store, provided once at boot.
#[derive(Debug, Default)]
pub struct Store {
    next_id: AtomicU32,
}

impl Store {
    fn allocate(&self) -> u64 {
        u64::from(self.next_id.fetch_add(1, Ordering::Relaxed)) + 1
    }
}

/// A per-request dependency, resolved from the provider map.
#[derive(Debug, Clone)]
pub struct Actor {
    /// Who the request is acting as.
    pub name: String,
}

impl moso::Dependency for Actor {
    const PROVIDER_REQ: &'static [moso::ProviderReq] = &[];

    async fn resolve(_ctx: &RequestCtx) -> Result<Self> {
        Ok(Actor {
            name: "anonymous".to_owned(),
        })
    }
}

// ---------------------------------------------------------------------------
// The endpoints
// ---------------------------------------------------------------------------

/// List every user.
#[endpoint]
async fn list(Inject(_store): Inject<Store>) -> Result<Json<Vec<UserOut>>> {
    Ok(Json(vec![UserOut {
        id: 1,
        username: "ada".to_owned(),
    }]))
}

/// Create a user.
///
/// The body is validated before this function runs.
#[endpoint]
async fn create(
    Inject(store): Inject<Store>,
    Depends(actor): Depends<Actor>,
    Json(body): Json<CreateUser>,
) -> Result<Created<UserOut>> {
    assert_eq!(actor.name, "anonymous");
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

fn router() -> Router {
    moso::routes! {
        GET    "/users"      => list,
        POST   "/users"      => create,
        GET    "/users/{id}" => show,
        DELETE "/users/{id}" => destroy,
    }
    .tag("users")
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// The application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct AppConfig {
    /// Greeting used by the smoke endpoint.
    #[config(default = "hello")]
    pub greeting: String,
}

fn app() -> axum::Router<()> {
    App::new(AppConfig::default())
        .provide(Store::default())
        .mount(router())
        .build()
        .expect("the application builds")
        .into_service()
}

async fn send(request: axum::http::Request<axum::body::Body>) -> (u16, String) {
    let response = app().oneshot(request).await.expect("infallible");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("body");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn get(path: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .uri(path)
        .body(axum::body::Body::empty())
        .unwrap()
}

fn post_json(path: &str, body: &str) -> axum::http::Request<axum::body::Body> {
    axum::http::Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body.to_owned()))
        .unwrap()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_get_route_reaches_its_handler() {
    let (status, body) = send(get("/users")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"ada\""), "{body}");
}

#[tokio::test]
async fn a_path_parameter_is_extracted() {
    let (status, body) = send(get("/users/7")).await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"id\":7"), "{body}");
}

#[tokio::test]
async fn a_valid_body_is_accepted_and_the_location_header_is_set() {
    let (status, body) = send(post_json(
        "/users",
        r#"{"username":"ada_l","email":"ada@example.com","age":36}"#,
    ))
    .await;
    assert_eq!(status, 201, "{body}");
    assert!(body.contains("\"username\":\"ada_l\""), "{body}");
}

#[tokio::test]
async fn an_invalid_body_is_a_422_with_a_json_pointer() {
    let (status, body) = send(post_json(
        "/users",
        r#"{"username":"A","email":"ada@example.com"}"#,
    ))
    .await;
    assert_eq!(status, 422, "{body}");
    assert!(body.contains("/username"), "expected a pointer: {body}");
}

#[tokio::test]
async fn a_malformed_body_is_a_400() {
    let (status, body) = send(post_json("/users", "{not json")).await;
    assert_eq!(status, 400, "{body}");
}

#[tokio::test]
async fn an_unknown_path_is_a_404_problem() {
    let (status, body) = send(get("/nope")).await;
    assert_eq!(status, 404, "{body}");
}

#[tokio::test]
async fn a_wrong_method_is_a_405() {
    let (status, body) = send(post_json("/users/1", "{}")).await;
    assert_eq!(status, 405, "{body}");
}

#[tokio::test]
async fn a_handler_error_becomes_a_problem_document() {
    let (status, body) = send(get("/users/0")).await;
    assert_eq!(status, 404, "{body}");
    assert!(body.contains("\"status\":404"), "{body}");
}

#[test]
fn the_endpoint_structs_carry_their_metadata() {
    use moso::__private::Endpoint;

    assert_eq!(<__moso_op_create as Endpoint>::NAME, "create");
    assert_eq!(<__moso_op_list as Endpoint>::NAME, "list");
}

#[test]
fn ep_resolves_a_handler_by_name() {
    let router = Router::new()
        .get("/users", moso::ep!(list))
        .post("/users", moso::ep!(create));
    assert_eq!(router.len(), 2);
}

#[test]
fn the_document_describes_every_route() {
    let app = App::new(AppConfig::default())
        .provide(Store::default())
        .mount(router())
        .build()
        .expect("builds");

    let json = serde_json::to_string(app.openapi()).expect("serialises");
    assert!(json.contains("/users"), "{json}");
    assert!(json.contains("/users/{id}"), "{json}");
    assert!(json.contains("CreateUser"), "{json}");
    assert!(json.contains("Create a user."), "{json}");
}

#[test]
fn a_missing_provider_is_a_boot_error_not_a_panic() {
    let result = App::new(AppConfig::default()).mount(router()).build();
    let error = result.expect_err("the `Store` provider is missing");
    let rendered = error.to_string();
    assert!(rendered.contains("Store"), "{rendered}");
}

#[test]
fn schema_validation_runs_off_the_wire() {
    use moso::schema::{Validate, ValidationCtx};

    let good = CreateUser {
        username: "ada_l".to_owned(),
        email: "ada@example.com".parse().expect("valid"),
        age: Some(36),
    };
    assert!(good.validate(&mut ValidationCtx::new()).is_ok());

    let bad = CreateUser {
        username: "A".to_owned(),
        email: "ada@example.com".parse().expect("valid"),
        age: Some(3),
    };
    let errors = bad
        .validate(&mut ValidationCtx::new())
        .expect_err("two fields are invalid");
    let pointers: Vec<_> = errors.iter().map(|e| e.pointer.to_string()).collect();
    assert!(pointers.iter().any(|p| p == "/username"), "{pointers:?}");
    assert!(pointers.iter().any(|p| p == "/age"), "{pointers:?}");
}

#[test]
fn the_generated_schema_carries_the_constraints() {
    let mut generator = moso::schema::SchemaGenerator::default();
    let node = CreateUser::json_schema(&mut generator);
    let json = serde_json::to_string(&node).expect("serialises");
    assert!(json.contains("minLength"), "{json}");
    assert!(json.contains("maxLength"), "{json}");
    assert!(json.contains("pattern"), "{json}");
}
