//! The layer that puts *who is asking* into the request, so a capability
//! denial can be attributed to somebody.
//!
//! # The gap this closes
//!
//! `#[requires(Perm::PostsPublish)]` names the permission enum and never the
//! role enum, so the audit entry it writes cannot resolve an `Actor<R>` — it
//! reads an identity out of the request extensions instead. Nothing put one
//! there, so every capability denial was recorded against an anonymous actor in
//! the global scope, which is an audit trail that records that *somebody* was
//! refused.
//!
//! ```text
//! request
//!   │
//!   ├─ actor_layer::<Role>()          ← here: resolve once, publish three
//!   │    extensions: ActorIdentity, Actor<Role>, IpAddr
//!   │
//!   ├─ RouteHandler                   ← builds the RequestCtx, snapshotting them
//!   │    guards            → `Requires` reads ActorIdentity for its audit entry
//!   │    extractors        → `Depends<Actor<Role>>` reads Actor<Role> back
//!   └─ handler
//! ```
//!
//! # Why the layer resolves the actor itself
//!
//! [`RequestCtx`] snapshots the extensions when it is
//! built, and it is built *inside* the route service — after this layer has
//! run. An identity inserted any later would not be in the snapshot the audit
//! path reads. So the layer asks the [`ActorSource`] directly, before any
//! context exists, and publishes the whole [`Actor`](crate::Actor) as well as
//! the erased identity. `Actor`'s [`Dependency`](moso_core::Dependency) impl
//! reads that extension first, so a request that is both attributed *and*
//! authorised still costs one actor lookup — the per-request dependency cache
//! cannot help here, because it did not exist yet.
//!
//! The one cost of that arrangement is that the context the source is handed is
//! not the context the handler will get: it is built for this call and dropped
//! after it, so a `Depends<T>` resolved *inside* an [`ActorSource`] is resolved
//! again later. Reading headers, cookies and the database — what a source
//! actually does — is unaffected. The correlation id is pinned into the
//! extensions so the two contexts at least agree about which request they are.
//!
//! # What it deliberately does not do
//!
//! It never changes a status code. An [`ActorSource`] that returns a 401 or a
//! 503 is logged at `debug` and otherwise ignored, because failing here would
//! turn every `#[public]` endpoint behind the layer into a 401 — and the
//! request goes on to resolve the actor the ordinary way, where the error
//! reaches the caller from the extractor that actually needed it.

use moso_core::ctx::RequestCtx;
use moso_core::extract::{ClientIp, Extract};
use moso_core::middleware::{FromFn, Next, from_fn};
use moso_core::{BoxFuture, Request, Response};

use crate::{ActorIdentity, ActorSource, Role};

/// The function [`actor_layer`] wraps, named so the layer's type can be.
///
/// A `fn` pointer rather than a closure: [`from_fn`] needs a
/// `Clone + Send + Sync + 'static` callable, and a pointer is all three without
/// a nameless opaque type leaking into [`actor_layer`]'s signature.
type ActorFn = fn(Request, Next) -> BoxFuture<'static, moso_core::Result<Response>>;

/// Attribute every request to the actor the configured [`ActorSource`] resolves.
///
/// The one line an application writes. It goes **after** the routes it covers,
/// because [`Router::layer`](moso_core::Router::layer) applies to the routes
/// registered so far:
///
/// ```text
/// let router = Router::new()
///     .get("/posts", moso::ep!(list))
///     .post("/posts/{id}/publish", moso::ep!(publish))
///     .layer(moso_authz::actor_layer::<Role>());   // ← every route above
/// ```
///
/// With it in place, a `#[requires]` denial is recorded against the caller
/// rather than against `anonymous`, and the entry's `ip` field is filled in
/// from [`ClientIp`] — which honours
/// `http.trusted_proxies`, so the address is the peer's unless a proxy you
/// trust said otherwise.
///
/// It is safe to install on routes that need no authorization: an absent
/// credential resolves to [`Actor::anonymous`](crate::Actor::anonymous), which is what
/// those entries record anyway.
///
/// ```
/// use moso_authz::actor_layer;
/// # use moso_authz::{PermSet, Permission, Role, perm::fingerprint_of};
/// # #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum Perm { Read }
/// # impl Permission for Perm {
/// #     const ALL: &'static [Self] = &[Perm::Read];
/// #     const FINGERPRINT: u64 = fingerprint_of(&["posts.read"]);
/// #     fn index(self) -> u16 { 0 }
/// #     fn from_index(i: u16) -> Option<Self> { (i == 0).then_some(Perm::Read) }
/// #     fn as_str(self) -> &'static str { "posts.read" }
/// #     fn description(self) -> &'static str { "View posts" }
/// #     fn group(self) -> &'static str { "posts" }
/// #     fn parse(n: &str) -> Option<Self> { (n == "posts.read").then_some(Perm::Read) }
/// # }
/// # #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum AppRole { Viewer }
/// # impl Role for AppRole {
/// #     type Perm = Perm;
/// #     const ALL: &'static [Self] = &[AppRole::Viewer];
/// #     fn index(self) -> u8 { 0 }
/// #     fn from_index(i: u8) -> Option<Self> { (i == 0).then_some(AppRole::Viewer) }
/// #     fn as_str(self) -> &'static str { "viewer" }
/// #     fn description(self) -> &'static str { "Read-only" }
/// #     fn permissions(self) -> PermSet<Perm> { PermSet::of([Perm::Read]) }
/// #     fn parse(n: &str) -> Option<Self> { (n == "viewer").then_some(AppRole::Viewer) }
/// # }
/// // `Clone`, because a router hands one copy to every route it covers.
/// let layer = actor_layer::<AppRole>();
/// let _second = layer;
/// ```
#[must_use]
pub fn actor_layer<R: Role>() -> FromFn<ActorFn> {
    from_fn(attribute::<R> as ActorFn)
}

/// The layer's body: attribute the head, then run the rest of the stack.
fn attribute<R: Role>(
    request: Request,
    next: Next,
) -> BoxFuture<'static, moso_core::Result<Response>> {
    Box::pin(async move {
        let (mut parts, body) = request.into_parts();
        attribute_parts::<R>(&mut parts).await;
        Ok(next.run(Request::from_parts(parts, body)).await)
    })
}

/// Publish the address, the identity and the actor into the extensions.
///
/// Every failure is a no-op rather than a refusal — see the module header — so
/// this returns nothing and logs what it could not do.
async fn attribute_parts<R: Role>(parts: &mut http::request::Parts) {
    let Ok(ctx) = moso_core::middleware::middleware_ctx(parts) else {
        // No application state: this router was never mounted by an `App`, so
        // there is no provider map to resolve a source from. The route itself
        // reports that far more clearly than a layer can.
        return;
    };

    // Pin the correlation id this context settled on, so the one the router's
    // context generates a moment later is the same one. Normally the
    // `request_id` middleware has already put it here and this replaces it with
    // itself; when that slot is disabled, this is what keeps a line the actor
    // source logged joinable to the audit entry the decision writes.
    parts.extensions.insert(*ctx.request_id());

    publish_address(parts, &ctx).await;

    let Some(source) = ctx.try_provider::<dyn ActorSource<R>>() else {
        tracing::debug!(
            target: "moso::authz",
            "`actor_layer` found no `ActorSource` to attribute this request with, so audit \
             entries from `#[requires]` will record an anonymous actor\n  help: register one \
             with `.provide_dyn::<dyn ActorSource<Role>>(..)`"
        );
        return;
    };
    publish_actor(parts, &*source, &ctx).await;
}

/// Publish the caller's address, if the server recorded one.
///
/// Through [`ClientIp`] rather than by reading a header, so the trusted-proxy
/// policy decides whether `X-Forwarded-For` counts. A server started without
/// connection info has no peer address at all, and an audit entry with no `ip`
/// is better than one carrying a number somebody made up.
async fn publish_address(parts: &mut http::request::Parts, ctx: &RequestCtx) {
    if let Ok(address) = ClientIp::extract(parts, ctx).await {
        parts.extensions.insert(address.into_inner());
    }
}

/// Resolve the actor and publish it, or say why it could not be.
async fn publish_actor<R: Role>(
    parts: &mut http::request::Parts,
    source: &dyn ActorSource<R>,
    ctx: &RequestCtx,
) {
    match source.actor(ctx).await {
        Ok(actor) => {
            // Three extensions, one lookup: the erased identity for the audit
            // path, the bare id for anything written against the older
            // documentation, and the whole actor for `Depends<Actor<R>>`.
            parts.extensions.insert(ActorIdentity::from(&actor));
            parts.extensions.insert(actor.id().clone());
            parts.extensions.insert(actor);
        }
        Err(error) => tracing::debug!(
            target: "moso::authz",
            %error,
            "`actor_layer` could not resolve an actor; the request continues unattributed and \
             the extractor that needs one reports this to the caller"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use moso_core::AppState;

    use super::*;
    use crate::fixture::{Role, actor};
    use crate::{Actor, ActorId, ActorKind};

    /// An actor source that answers from a header, the way the documentation's
    /// `SessionActor` does.
    struct HeaderActor;

    impl ActorSource<Role> for HeaderActor {
        fn actor<'a>(
            &'a self,
            ctx: &'a RequestCtx,
        ) -> BoxFuture<'a, moso_core::Result<Actor<Role>>> {
            Box::pin(async move {
                match ctx
                    .headers()
                    .get("x-actor")
                    .and_then(|value| value.to_str().ok())
                {
                    Some(id) => Ok(actor(id, [Role::Editor])),
                    None => Ok(Actor::anonymous()),
                }
            })
        }
    }

    /// A source that is having a bad day, to prove the layer swallows it.
    struct Unavailable;

    impl ActorSource<Role> for Unavailable {
        fn actor<'a>(
            &'a self,
            _ctx: &'a RequestCtx,
        ) -> BoxFuture<'a, moso_core::Result<Actor<Role>>> {
            Box::pin(async { Err(moso_core::Error::unavailable("the role store is down")) })
        }
    }

    /// A request head, and a context over the empty application state — which
    /// is all `publish_actor` needs, since the source is handed to it.
    fn head(header: Option<&str>) -> (http::request::Parts, RequestCtx) {
        let mut request = http::Request::builder().uri("/posts");
        if let Some(header) = header {
            request = request.header("x-actor", header);
        }
        let (mut parts, ()) = request.body(()).expect("a request head").into_parts();

        let state = Arc::new(AppState::for_tests());
        parts.extensions.insert(Arc::clone(&state));
        let ctx = RequestCtx::new(state, &parts);
        (parts, ctx)
    }

    #[tokio::test]
    async fn the_layer_publishes_the_identity_the_audit_trail_reads() {
        let (mut parts, ctx) = head(Some("usr_1"));
        publish_actor(&mut parts, &HeaderActor, &ctx).await;

        let identity = parts
            .extensions
            .get::<ActorIdentity>()
            .expect("the erased identity");
        assert_eq!(identity.id().as_str(), "usr_1");
        assert_eq!(identity.kind(), ActorKind::User);
        assert_eq!(
            parts.extensions.get::<ActorId>().map(ActorId::as_str),
            Some("usr_1"),
        );
    }

    /// The whole actor is published too, which is what keeps a request that is
    /// both attributed and authorised to one actor lookup.
    #[tokio::test]
    async fn the_resolved_actor_is_left_for_the_dependency_to_reuse() {
        let (mut parts, ctx) = head(Some("usr_1"));
        publish_actor(&mut parts, &HeaderActor, &ctx).await;

        let actor = parts
            .extensions
            .get::<Actor<Role>>()
            .expect("the resolved actor");
        assert!(actor.is(Role::Editor));
    }

    #[tokio::test]
    async fn an_absent_credential_is_attributed_to_nobody_rather_than_refused() {
        let (mut parts, ctx) = head(None);
        publish_actor(&mut parts, &HeaderActor, &ctx).await;

        let identity = parts.extensions.get::<ActorIdentity>().expect("identity");
        assert!(identity.id().is_anonymous());
        assert_eq!(identity.kind(), ActorKind::Anonymous);
    }

    /// A source that fails must not become a status code here: the request goes
    /// on, and the extractor that actually needs an actor reports it.
    #[tokio::test]
    async fn a_failing_source_leaves_the_request_unattributed_and_nothing_else() {
        let (mut parts, ctx) = head(Some("usr_1"));
        publish_actor(&mut parts, &Unavailable, &ctx).await;

        assert!(parts.extensions.get::<ActorIdentity>().is_none());
        assert!(parts.extensions.get::<ActorId>().is_none());
    }

    /// No source in the provider map is a logged no-op, not a refusal.
    #[tokio::test]
    async fn no_registered_source_is_a_no_op() {
        let (mut parts, _ctx) = head(Some("usr_1"));
        attribute_parts::<Role>(&mut parts).await;

        assert!(parts.extensions.get::<ActorIdentity>().is_none());
    }

    /// A head with no application state at all — a router used outside an
    /// `App` — must not panic.
    #[tokio::test]
    async fn a_router_with_no_application_behind_it_is_a_no_op() {
        let (mut parts, ()) = http::Request::builder()
            .uri("/posts")
            .body(())
            .expect("a request head")
            .into_parts();

        attribute_parts::<Role>(&mut parts).await;
        assert!(parts.extensions.get::<ActorIdentity>().is_none());
    }

    /// A server started without connection info records no address, rather
    /// than one taken from a header the caller controls.
    #[tokio::test]
    async fn no_peer_address_records_no_address() {
        let (mut parts, ctx) = head(Some("usr_1"));
        parts
            .headers
            .insert("x-forwarded-for", http::HeaderValue::from_static("1.2.3.4"));

        publish_address(&mut parts, &ctx).await;

        assert!(parts.extensions.get::<std::net::IpAddr>().is_none());
    }

    /// The context the source is handed and the context the router builds a
    /// moment later must agree about which request they are, or a line the
    /// source logged joins to nothing.
    #[tokio::test]
    async fn the_correlation_id_survives_into_the_router_s_own_context() {
        /// The id a context over this head settles on, as text — so the test
        /// does not have to name `ulid::Ulid`, which is not this crate's.
        fn correlation_id(parts: &http::request::Parts) -> String {
            RequestCtx::new(Arc::new(AppState::for_tests()), parts)
                .request_id()
                .to_string()
        }

        let (mut parts, _ctx) = head(Some("usr_1"));
        assert_ne!(
            correlation_id(&parts),
            correlation_id(&parts),
            "with nothing pinned, every context over this head invents its own id",
        );

        attribute_parts::<Role>(&mut parts).await;

        assert_eq!(
            correlation_id(&parts),
            correlation_id(&parts),
            "the layer pinned one, so the router adopts it rather than inventing another",
        );
    }

    #[test]
    fn the_layer_is_copied_because_every_route_gets_one() {
        let layer = actor_layer::<Role>();
        let copy = layer;

        assert_eq!(
            core::mem::size_of_val(&layer),
            core::mem::size_of_val(&copy),
        );
    }
}
