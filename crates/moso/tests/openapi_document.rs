//! The generated OpenAPI document, and the four routes the framework mounts to
//! serve it and its neighbours.
//!
//! A document that serialises is not a document that works. The two defects
//! that survive every other check are a `$ref` pointing at a schema nobody
//! registered and two operations claiming one `operationId`: the first breaks
//! the client generator, the second silently drops a method from it. Both are
//! checked here over a deliberately awkward application — generics, recursion,
//! enums, nested modules, shared types — because both are only reachable once
//! the document has more than one schema in it.

#![allow(dead_code)]

use std::collections::BTreeSet;

use moso::prelude::*;
use moso::response::{Created, NoContent, Page};

mod support;
use support::{dangling_refs, duplicate_operation_ids, get, operations, send};

// ---------------------------------------------------------------------------
// A model with something to trip over
// ---------------------------------------------------------------------------

/// A tag, reused by two different bodies so the same schema is referenced twice.
#[derive(Schema, Debug, Clone)]
pub struct Tag {
    /// The tag's slug.
    pub slug: Slug,
    /// How many things carry it.
    pub count: u32,
}

/// A node in a tree, which refers to itself.
#[derive(Schema, Debug, Clone)]
pub struct Node {
    /// This node's name.
    pub name: String,
    /// Its children. A schema that refers to itself is the classic way to make
    /// a generator loop forever or emit a dangling reference.
    pub children: Vec<Node>,
}

/// How a post may be filed.
#[derive(Schema, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    /// Anyone may read it.
    Public,
    /// Only the author.
    Private,
}

/// The body a client posts.
#[derive(Schema, Debug, Clone)]
pub struct NewPost {
    /// The headline.
    #[schema(len = 1..=140)]
    pub title: String,
    /// Where it sits.
    pub visibility: Visibility,
    /// Its tags.
    pub tags: Vec<Tag>,
    /// An outline, as a tree.
    pub outline: Option<Node>,
}

/// The representation the API returns.
#[derive(Schema, Debug, Clone)]
pub struct Post {
    /// Stable identifier.
    pub id: Id<Post>,
    /// The headline.
    pub title: String,
    /// Its tags — the same `Tag` the request body uses.
    pub tags: Vec<Tag>,
}

/// This application's configuration.
#[derive(Config, Debug, Clone, Default)]
pub struct Cfg {}

// ---------------------------------------------------------------------------
// Endpoints, some of them in modules, some of them sharing a name
// ---------------------------------------------------------------------------

/// The posts area.
pub mod posts {
    use super::*;

    /// List posts.
    #[endpoint]
    pub async fn list() -> Result<Page<Post>> {
        Ok(Page::new(Vec::new()))
    }

    /// Create a post.
    #[endpoint]
    pub async fn create(Json(body): Json<NewPost>) -> Result<Created<Post>> {
        Ok(Created::at(
            "/posts/1",
            Post {
                id: Id::new(),
                title: body.title,
                tags: body.tags,
            },
        ))
    }

    /// Show one post.
    #[endpoint]
    pub async fn show(Path(_id): Path<u64>) -> Result<Json<Post>> {
        Err(Error::not_found("post"))
    }

    /// Delete one post.
    #[endpoint]
    pub async fn destroy(Path(_id): Path<u64>) -> Result<NoContent> {
        Ok(NoContent)
    }

    /// This module's routes.
    pub fn router() -> Router {
        moso::routes! {
            GET    "/"     => list,
            POST   "/"     => create,
            GET    "/{id}" => show,
            DELETE "/{id}" => destroy,
        }
        .tag("posts")
    }
}

/// The tags area, whose `list` collides by name with `posts::list`.
pub mod tags {
    use super::*;

    /// List tags.
    #[endpoint]
    pub async fn list() -> Result<Json<Vec<Tag>>> {
        Ok(Json(Vec::new()))
    }

    /// This module's routes.
    pub fn router() -> Router {
        moso::routes! { GET "/" => list }.tag("tags")
    }
}

fn router() -> Router {
    Router::new()
        .nest("/posts", posts::router())
        .nest("/tags", tags::router())
}

fn built() -> moso::App {
    App::new(Cfg::default())
        .mount(router())
        .openapi(|document| {
            document.title("Journal");
            document.version("2.1.0");
        })
        .build()
        .expect("builds")
}

fn document() -> serde_json::Value {
    serde_json::to_value(built().openapi()).expect("the document serialises")
}

fn app() -> axum::Router<()> {
    built().into_service()
}

// ---------------------------------------------------------------------------
// The document is internally consistent
// ---------------------------------------------------------------------------

#[test]
fn every_reference_resolves() {
    let document = document();
    let dangling = dangling_refs(&document);
    assert!(
        dangling.is_empty(),
        "a `$ref` that does not resolve compiles into a client that does not \
         build:\n{}\n--- document ---\n{}",
        dangling.join("\n"),
        serde_json::to_string_pretty(&document).expect("pretty")
    );
}

#[test]
fn no_two_operations_claim_the_same_id() {
    let document = document();
    let duplicates = duplicate_operation_ids(&document);
    assert!(
        duplicates.is_empty(),
        "an operationId is a method name in every generated client:\n{duplicates:?}"
    );
}

#[test]
fn two_handlers_called_list_get_two_ids() {
    let document = document();
    let ids: BTreeSet<String> = operations(&document)
        .into_iter()
        .filter_map(|(_, _, operation)| {
            operation
                .get("operationId")
                .and_then(|id| id.as_str())
                .map(str::to_owned)
        })
        .collect();

    assert_eq!(
        ids.len(),
        operations(&document).len(),
        "every operation needs its own id: {ids:?}"
    );
}

#[test]
fn every_operation_is_documented_enough_to_generate_from() {
    let document = document();
    let ops = operations(&document);
    assert_eq!(ops.len(), 5, "five routes were registered");

    for (method, path, operation) in ops {
        assert!(
            operation.contains_key("operationId"),
            "{method} {path} has no operationId"
        );
        assert!(
            operation
                .get("summary")
                .and_then(|s| s.as_str())
                .is_some_and(|s| !s.is_empty()),
            "{method} {path} has no summary; the doc comment should have become one"
        );
        let responses = operation
            .get("responses")
            .and_then(|r| r.as_object())
            .unwrap_or_else(|| panic!("{method} {path} documents no responses"));
        assert!(
            !responses.is_empty(),
            "{method} {path} documents no responses"
        );
    }
}

#[test]
fn a_recursive_schema_is_registered_once_and_refers_to_itself() {
    let document = document();
    let node = &document["components"]["schemas"]["Node"];
    assert!(node.is_object(), "`Node` should be a named component");

    let child = &node["properties"]["children"]["items"];
    assert_eq!(
        child["$ref"], "#/components/schemas/Node",
        "recursion has to become a reference, or generation never terminates: {node}"
    );
}

#[test]
fn a_shared_type_is_registered_once_and_referenced_twice() {
    let document = document();
    assert!(document["components"]["schemas"]["Tag"].is_object());

    let uses = serde_json::to_string(&document)
        .expect("json")
        .matches("#/components/schemas/Tag")
        .count();
    assert!(
        uses >= 2,
        "`Tag` appears in both the request and the response: {uses}"
    );
}

#[test]
fn the_document_is_byte_stable_across_builds() {
    let first = serde_json::to_string(built().openapi()).expect("json");
    let second = serde_json::to_string(built().openapi()).expect("json");
    assert_eq!(
        first, second,
        "D15: the committed document must diff cleanly"
    );
}

#[test]
fn the_document_declares_what_it_is() {
    let document = document();
    assert!(
        document["openapi"]
            .as_str()
            .is_some_and(|version| version.starts_with("3.1")),
        "{document}"
    );
    assert_eq!(document["info"]["title"], "Journal");
    assert_eq!(document["info"]["version"], "2.1.0");
}

#[test]
fn nesting_is_reflected_in_the_paths() {
    let document = document();
    let paths = document["paths"].as_object().expect("paths");
    let mut keys: Vec<&String> = paths.keys().collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["/posts", "/posts/{id}", "/tags"],
        "the nested routers must be rewritten onto their prefixes"
    );
}

// ---------------------------------------------------------------------------
// The routes that serve it
// ---------------------------------------------------------------------------

#[tokio::test]
async fn healthz_is_up_and_uncached() {
    let reply = send(app(), get("/healthz")).await;

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["status"], "up");
    assert!(
        reply.header("cache-control").contains("no-store"),
        "a cached liveness probe is a liveness probe that lies: {:?}",
        reply.header("cache-control")
    );
}

#[tokio::test]
async fn readyz_reports_the_declared_version() {
    let reply = send(app(), get("/readyz")).await;

    assert!(
        reply.status == 200 || reply.status == 503,
        "{} {}",
        reply.status,
        reply.body
    );
    assert_eq!(
        reply.json()["version"],
        "2.1.0",
        "the running instance must be able to say which build it is: {}",
        reply.body
    );
}

#[tokio::test]
async fn readyz_turns_503_the_moment_shutdown_is_signalled() {
    let application = built();
    let signal = application.shutdown_signal();
    let service = application.into_service();

    assert_eq!(send(service.clone(), get("/readyz")).await.status, 200);

    signal.trigger();

    let reply = send(service, get("/readyz")).await;
    assert_eq!(
        reply.status, 503,
        "the load balancer has to be told before draining starts, not after: {}",
        reply.body
    );
    let report = reply.json();
    assert_eq!(report["status"], "down", "{}", reply.body);
    assert!(
        report["checks"]["process"]
            .as_str()
            .is_some_and(|reason| reason.contains("shutting down")),
        "the reason has to say it is a shutdown and not a failure: {}",
        reply.body
    );
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn openapi_json_serves_the_same_document_the_builder_produced() {
    let reply = send(app(), get("/openapi.json")).await;

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.header("content-type").contains("json"),
        "{:?}",
        reply.header("content-type")
    );

    let served: serde_json::Value = serde_json::from_str(&reply.body).expect("valid JSON");
    assert_eq!(
        served,
        document(),
        "the served document and the in-process one must not be able to drift"
    );
    assert!(dangling_refs(&served).is_empty());
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn openapi_json_is_cacheable_and_answers_a_conditional_request() {
    let reply = send(app(), get("/openapi.json")).await;
    let etag = reply.header("etag").to_owned();
    assert!(!etag.is_empty(), "a document this size needs an ETag");

    let conditional = axum::http::Request::builder()
        .uri("/openapi.json")
        .header("if-none-match", &etag)
        .body(axum::body::Body::empty())
        .expect("request");
    let second = send(app(), conditional).await;

    assert_eq!(second.status, 304, "{}", second.body);
    assert!(
        second.bytes.is_empty(),
        "a 304 carries no body: {} bytes",
        second.bytes.len()
    );
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn the_docs_ui_is_html_that_loads_the_spec_from_this_origin() {
    let reply = send(app(), get("/docs")).await;

    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.header("content-type").starts_with("text/html"),
        "{:?}",
        reply.header("content-type")
    );
    assert!(
        reply.body.contains("/openapi.json"),
        "the UI has to point at the document it documents"
    );
    assert!(
        !reply.body.contains("http://") && !reply.body.contains("https://cdn"),
        "the UI must not fetch anything from a third party: an air-gapped \
         deployment still needs its documentation"
    );
}

#[cfg(feature = "openapi")]
#[tokio::test]
async fn the_docs_routes_can_be_switched_off() {
    let service = App::new(Cfg::default())
        .mount(router())
        .http_config(moso::http_config::HttpConfig {
            expose_docs: false,
            ..moso::http_config::HttpConfig::default()
        })
        .build()
        .expect("builds")
        .into_service();

    assert_eq!(send(service.clone(), get("/docs")).await.status, 404);
    assert_eq!(send(service, get("/openapi.json")).await.status, 404);
}

#[tokio::test]
async fn the_probes_do_not_collide_with_application_routes() {
    // The probes are mounted on an outer router and the application is its
    // fallback, so an application route still answers.
    let reply = send(app(), get("/tags")).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
}
