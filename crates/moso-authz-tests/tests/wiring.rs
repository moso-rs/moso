//! The wiring an application writes, driven through one real request.
//!
//! Everything here is about the seams between `moso-authz` and `moso-core`,
//! which unit tests on either side cannot reach:
//!
//! | Claim | Why a unit test cannot prove it |
//! | --- | --- |
//! | `actor_layer` attributes a `#[requires]` denial | the identity has to survive into the `RequestCtx` the *router* builds |
//! | the actor is resolved once | the layer and `Depends<Actor<Role>>` are on opposite sides of that context |
//! | `Requires::new(PermSet::empty())` refuses | a guard only runs inside a mounted router |
//! | the audit sink is flushed at shutdown | `on_shutdown` is an `AppBuilder` hook |
//!
//! Each one is a request through the composed service, which is the same stack
//! `App::serve` runs.

#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moso::deps::http;
use moso::prelude::*;
use moso::response::NoContent;
use moso_authz::{
    Actor, ActorId, ActorKind, ActorPermissions, ActorSource, AuditSink, MemoryAuditSink, PermSet,
    PermissionSource, Requires, RoleSet, Scope,
};
use moso_core::BoxFuture;
use moso_core::ctx::RequestCtx;
use tower::ServiceExt as _;

// ---------------------------------------------------------------------------
// The application
// ---------------------------------------------------------------------------

moso::permissions! {
    /// Posts
    posts.read      = "View posts",
    posts.publish   = "Publish posts",

    /// Administration
    admin.access    = "Access the admin panel",
}

moso::roles! {
    /// Read-only access.
    Viewer = [posts.read],
    /// Runs the organisation.
    Admin  = Viewer + [posts.publish, admin.access],
}

/// Everything this application reads from its environment.
#[derive(Config, Clone, Debug, Default)]
pub struct AppConfig {}

/// Turns `x-actor` into who is asking, and counts how often it was asked.
///
/// The counter is the point: a request that both attributes and authorises must
/// resolve the actor once, and the only way to see that is to count.
struct HeaderActor {
    resolutions: Arc<AtomicUsize>,
}

impl ActorSource<Role> for HeaderActor {
    fn actor<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, moso::Result<Actor<Role>>> {
        Box::pin(async move {
            self.resolutions.fetch_add(1, Ordering::Relaxed);
            let Some(id) = ctx.headers().get("x-actor").and_then(|v| v.to_str().ok()) else {
                return Ok(Actor::anonymous());
            };
            let roles = if id == "root" {
                RoleSet::of([Role::Admin])
            } else {
                RoleSet::of([Role::Viewer])
            };
            Ok(Actor::new(
                ActorId::new(id),
                ActorKind::User,
                Scope::Global,
                roles,
            ))
        })
    }
}

/// Publish a post. Audited on the allow path as well as the deny path.
#[moso::requires(Perm::PostsPublish, audit)]
#[moso::endpoint]
pub async fn publish(Path(id): Path<i64>) -> moso::Result<NoContent> {
    let _ = id;
    Ok(NoContent)
}

/// Read a post, which every actor here may do — so a handler that *also* takes
/// the actor exercises the "resolved once" claim.
#[moso::requires(Perm::PostsRead)]
#[moso::endpoint]
pub async fn show(
    Path(id): Path<i64>,
    Depends(actor): Depends<Actor<Role>>,
) -> moso::Result<NoContent> {
    let _ = (id, actor.id());
    Ok(NoContent)
}

/// The scheme every authorization-aware operation names. Defining it is the
/// application's job, because this crate does not know how you authenticate.
fn with_scheme(builder: moso::AppBuilder) -> moso::AppBuilder {
    builder.openapi(|document| {
        document
            .title("authz wiring")
            .version("0.0.0")
            .security_scheme(moso_authz::AUTH_SCHEME, SecurityScheme::http_bearer("JWT"));
    })
}

/// What every test here builds, with the layer attached or not.
fn app(sink: &Arc<MemoryAuditSink>, resolutions: &Arc<AtomicUsize>, attributed: bool) -> App {
    let source = Arc::new(HeaderActor {
        resolutions: Arc::clone(resolutions),
    });

    let mut router = Router::new()
        .post("/posts/{id}/publish", moso::ep!(publish))
        .get("/posts/{id}", moso::ep!(show));
    if attributed {
        // The one line this whole file exists to check.
        router = router.layer(moso_authz::actor_layer::<Role>());
    }

    with_scheme(App::new(AppConfig::default()))
        .provide_dyn::<dyn ActorSource<Role>>(source)
        .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()))
        .provide_dyn::<dyn AuditSink>(Arc::clone(sink) as Arc<dyn AuditSink>)
        .mount(router)
        .build()
        .expect("the application builds")
}

/// One request through the composed service.
async fn call(app: App, method: &str, path: &str, actor: Option<&str>) -> http::StatusCode {
    let request = http::Request::builder().method(method).uri(path);
    send(app, request, actor).await.0
}

/// The same, keeping the body — for the tests that read a problem document.
async fn send(
    app: App,
    mut request: http::request::Builder,
    actor: Option<&str>,
) -> (http::StatusCode, String) {
    if let Some(actor) = actor {
        request = request.header("x-actor", actor);
    }
    let request = request
        .body(moso::deps::axum::body::Body::empty())
        .expect("a request");

    let response = app
        .into_service()
        .oneshot(request)
        .await
        .expect("the service answers");
    let status = response.status();
    let body = moso::deps::axum::body::to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("the body reads");
    (status, String::from_utf8_lossy(&body).into_owned())
}

// ---------------------------------------------------------------------------
// Attribution
// ---------------------------------------------------------------------------

/// The gap this closes: without the layer, the audit entry for a capability
/// denial records `anonymous`, because nothing put an identity in the request.
#[tokio::test]
async fn without_the_layer_a_capability_denial_is_recorded_against_nobody() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let app = app(&sink, &resolutions, false);

    let status = call(app, "POST", "/posts/7/publish", Some("usr_1")).await;

    assert_eq!(status, http::StatusCode::FORBIDDEN);
    let entries = sink.denials();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0].actor.is_anonymous(),
        "this is the behaviour `actor_layer` exists to fix",
    );
}

#[tokio::test]
async fn the_layer_attributes_a_capability_denial_to_the_caller() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let app = app(&sink, &resolutions, true);

    let status = call(app, "POST", "/posts/7/publish", Some("usr_1")).await;

    assert_eq!(
        status,
        http::StatusCode::FORBIDDEN,
        "a viewer cannot publish"
    );
    let entries = sink.denials();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].actor.as_str(), "usr_1");
    assert_eq!(entries[0].actor_kind, ActorKind::User);
    assert_eq!(entries[0].action, "posts.publish");
    assert_eq!(
        entries[0].route.as_deref(),
        Some("/posts/{id}/publish"),
        "the matched pattern, never the raw path",
    );
}

/// An allow forced by `#[requires(.., audit)]` is attributed too, which is the
/// half a compliance review asks about.
#[tokio::test]
async fn an_audited_allow_names_the_actor_that_was_permitted() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let app = app(&sink, &resolutions, true);

    let status = call(app, "POST", "/posts/7/publish", Some("root")).await;

    assert_eq!(status, http::StatusCode::NO_CONTENT);
    let entries = sink.entries();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].actor.as_str(), "root");
    assert!(!entries[0].outcome.is_deny());
}

/// An absent credential is attributed to nobody rather than refused by the
/// layer, so a route the layer covers but that needs no permission still works.
#[tokio::test]
async fn an_unauthenticated_request_is_recorded_as_anonymous_and_not_rejected_early() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let app = app(&sink, &resolutions, true);

    let status = call(app, "POST", "/posts/7/publish", None).await;

    assert_eq!(
        status,
        http::StatusCode::FORBIDDEN,
        "the guard refuses, not the layer"
    );
    assert!(sink.denials()[0].actor.is_anonymous());
}

/// The layer resolves the actor before the request context exists, and
/// publishes it so `Depends<Actor<Role>>` reads it back rather than asking the
/// source a second time.
#[tokio::test]
async fn an_attributed_and_authorised_request_resolves_its_actor_once() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let app = app(&sink, &resolutions, true);

    let status = call(app, "GET", "/posts/7", Some("usr_1")).await;

    assert_eq!(status, http::StatusCode::NO_CONTENT);
    assert_eq!(
        resolutions.load(Ordering::Relaxed),
        1,
        "the layer, the `#[requires]` check and `Depends<Actor<Role>>` share one resolution",
    );
}

// ---------------------------------------------------------------------------
// Deny by default
// ---------------------------------------------------------------------------

/// `Requires::new(PermSet::empty())` used to allow everybody, because `has_all`
/// of nothing is vacuously true. A permission set built from a filter that came
/// back empty must close the route it was meant to close, not open it.
#[tokio::test]
async fn a_guard_with_an_empty_permission_set_refuses_everybody() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(HeaderActor {
        resolutions: Arc::clone(&resolutions),
    });

    // The mistake: a set that a filter emptied.
    let empty: PermSet<Perm> = PermSet::all()
        .iter()
        .filter(|_| false)
        .fold(PermSet::empty(), PermSet::with);

    let app = with_scheme(App::new(AppConfig::default()))
        .provide_dyn::<dyn ActorSource<Role>>(source)
        .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()))
        .provide_dyn::<dyn AuditSink>(Arc::clone(&sink) as Arc<dyn AuditSink>)
        .mount(
            Router::new()
                .get("/posts/{id}", moso::ep!(show))
                .guard(Requires::new(empty)),
        )
        .build()
        .expect("the application builds");

    // `root` holds every permission there is, and is still refused.
    let status = call(app, "GET", "/posts/7", Some("root")).await;

    assert_eq!(
        status,
        http::StatusCode::FORBIDDEN,
        "an empty requirement refuses, including the actor who holds everything",
    );
}

/// …and the same guard with a permission in it still admits the actor holding
/// it, so the change refuses nothing it should not.
#[tokio::test]
async fn a_guard_with_a_permission_in_it_still_admits_the_actor_holding_it() {
    let sink = Arc::new(MemoryAuditSink::new());
    let resolutions = Arc::new(AtomicUsize::new(0));
    let source = Arc::new(HeaderActor {
        resolutions: Arc::clone(&resolutions),
    });

    let app = with_scheme(App::new(AppConfig::default()))
        .provide_dyn::<dyn ActorSource<Role>>(source)
        .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()))
        .provide_dyn::<dyn AuditSink>(Arc::clone(&sink) as Arc<dyn AuditSink>)
        .mount(
            Router::new()
                .get("/posts/{id}", moso::ep!(show))
                .guard(Requires::new(PermSet::of([Perm::PostsRead]))),
        )
        .build()
        .expect("the application builds");

    assert_eq!(
        call(app, "GET", "/posts/7", Some("usr_1")).await,
        http::StatusCode::NO_CONTENT,
    );
}

// ---------------------------------------------------------------------------
// The policy registry, consulted at runtime
// ---------------------------------------------------------------------------

moso_authz::actions! {
    for Role;
    /// Making a draft public.
    Publish = "publish",
}

/// A post, held in memory so this test needs no database.
#[derive(Clone, Debug)]
pub struct Post {
    pub id: i64,
    pub author_id: String,
}

/// The row every request to `/drafts/{id}` is asked about.
///
/// A hand-written [`ResourceSource`] rather than `FromPathId`, because what is
/// being checked is the *explanation*, and loading the row through the ORM
/// would drag a database into a test about a `policy` row.
pub struct FixedDraft;

impl moso_authz::ResourceSource<Post> for FixedDraft {
    const RESOURCE: &'static str = "Post";

    fn describe(op: &mut moso_openapi::OperationBuilder) {
        op.parameter(
            moso_openapi::builder::Param::path("id")
                .required(true)
                .schema_of::<String>()
                .description("The identifier of the Post."),
        );
    }

    fn load<'a>(
        _parts: &'a mut moso::deps::http::request::Parts,
        _ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso::Result<Option<Post>>> {
        Box::pin(async {
            Ok(Some(Post {
                id: 456,
                author_id: "usr_999".to_owned(),
            }))
        })
    }

    fn locate(_parts: &moso::deps::http::request::Parts, ctx: &RequestCtx) -> Option<String> {
        ctx.path_params()?.get("id").map(ToOwned::to_owned)
    }
}

impl moso_authz::Policy<Publish, Post> for Actor<Role> {
    async fn allows(
        &self,
        _: Publish,
        post: &Post,
        _ctx: &moso_authz::PolicyCtx,
    ) -> moso_authz::Decision {
        if post.author_id == self.id().as_str() {
            return moso_authz::Decision::allow("author");
        }
        moso_authz::Decision::deny("not the author and not an admin")
    }
}

/// Publish a draft, through the policy.
#[moso::endpoint]
pub async fn publish_draft(
    draft: moso_authz::Authorized<Publish, Post, FixedDraft>,
) -> moso::Result<NoContent> {
    let _ = draft.id;
    Ok(NoContent)
}

/// An application with the registry registered, or without it.
fn drafts_app(with_registry: bool) -> App {
    let resolutions = Arc::new(AtomicUsize::new(0));
    let builder = with_scheme(App::new(AppConfig::default()))
        .provide_dyn::<dyn ActorSource<Role>>(Arc::new(HeaderActor { resolutions }))
        .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()));

    let builder = if with_registry {
        // The registration path an application writes: build the registry with
        // `policy!`, which captures the file and line, and provide it.
        let registry = moso_authz::policy!(moso_authz::PolicyRegistry::new(), Publish, "Post");
        builder.provide(registry)
    } else {
        builder
    };

    builder
        .mount(Router::new().post("/drafts/{id}/publish", moso::ep!(publish_draft)))
        .build()
        .expect("the application builds")
}

/// The gap this closes: the live 403's explain block had no `policy` row,
/// because nothing consulted the registry.
#[tokio::test]
async fn a_registered_policy_names_itself_in_a_live_explain_block() {
    let request = http::Request::builder()
        .method("POST")
        .uri("/drafts/456/publish")
        .header(moso_authz::EXPLAIN_HEADER, "1");

    let (status, body) = send(drafts_app(true), request, Some("usr_1")).await;

    assert_eq!(status, http::StatusCode::FORBIDDEN);
    assert!(
        body.contains("Policy<Publish, Post> for Actor"),
        "the block names the impl as it is written: {body}",
    );
    assert!(
        body.contains("tests/wiring.rs:"),
        "…and where it is written: {body}",
    );
}

/// An application that registered nothing still gets a block, without that row
/// — a location this crate invented would be worse than an admitted gap.
#[tokio::test]
async fn an_unregistered_policy_leaves_the_row_out_rather_than_inventing_one() {
    let request = http::Request::builder()
        .method("POST")
        .uri("/drafts/456/publish")
        .header(moso_authz::EXPLAIN_HEADER, "1");

    let (status, body) = send(drafts_app(false), request, Some("usr_1")).await;

    assert_eq!(status, http::StatusCode::FORBIDDEN);
    assert!(body.contains("DENY"), "the block is still rendered: {body}");
    assert!(!body.contains("Policy<Publish, Post>"), "{body}");
}

// ---------------------------------------------------------------------------
// The shutdown flush
// ---------------------------------------------------------------------------

/// `AuditSink::flush` documents the shutdown drain as its call site, and
/// `moso-core` knows nothing about this crate. `flush_audit` is the one line
/// that connects the two, and this is it running as an `on_shutdown` hook.
#[tokio::test]
async fn the_shutdown_hook_flushes_a_batching_sink_before_the_process_leaves() {
    use moso_authz::audit::{BatchingAuditSink, flush_audit};
    use moso_authz::{ActorId as Id, AuditRecord};

    let written = Arc::new(MemoryAuditSink::new());
    // A batch size no test run will fill, so only the flush can write it.
    let sink = Arc::new(BatchingAuditSink::new(
        Arc::clone(&written) as Arc<dyn AuditSink>,
        1_000,
    ));

    let app = with_scheme(App::new(AppConfig::default()))
        .provide_dyn::<dyn AuditSink>(Arc::clone(&sink) as Arc<dyn AuditSink>)
        .on_shutdown(|resolver| async move { flush_audit(&resolver).await })
        .mount(Router::new())
        .build()
        .expect("the application builds");

    sink.record(AuditRecord::deny(
        Id::new("usr_1"),
        ActorKind::User,
        Scope::Global,
        "posts.publish",
        "not the author",
    ))
    .await;
    assert!(written.is_empty(), "one entry is not a batch of a thousand");

    // What `App::serve` does after the drain, without binding a listener.
    flush_audit(&app.resolver()).await;

    assert_eq!(written.len(), 1);
}
