#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = "Moso's authorization battery: typed permissions, policies and query-level filtering."]
//!
//! Authorization in Rust is roll-your-own. There is no batteries-included layer,
//! which makes it the clearest unclaimed gap in the ecosystem — and the reason
//! this crate exists rather than a wrapper around Casbin.
//!
//! Five goals shape everything below:
//!
//! 1. **Typed, not stringly.** A permission is a compile-time constant. A typo
//!    is a compile error, not a silent `false`.
//! 2. **Enumerable.** The whole set is knowable at boot, so the admin can render
//!    it, the OpenAPI can document it, and an audit can list it.
//! 3. **Two layers, cleanly separated.** Coarse capability checks
//!    ([`Requires`], from `#[requires]`) and fine resource checks
//!    ([`Policy`], through [`Authorized`]). Most systems fail because they model
//!    only one.
//! 4. **Deny by default, and provably so.** An endpoint with no declaration is
//!    reported by `moso check --authz`; `#[public]` is how you say you meant it.
//! 5. **Explainable.** Every decision can produce an [`Explanation`]. Debugging
//!    "why can't this user do X" without one is the recurring pain of every
//!    authorization system.
//!
//! ```text
//! // src/authz.rs
//! moso::permissions! {
//!     posts.read    = "View posts",
//!     posts.publish = "Publish posts",
//!     admin.access  = "Access the admin panel",
//! }
//!
//! moso::roles! {
//!     Viewer = [posts.read],
//!     Editor = Viewer + [posts.publish],
//! }
//!
//! // src/routes/posts.rs — the capability check
//! #[endpoint]
//! #[requires(Perm::PostsPublish)]
//! async fn publish_all(Inject(db): Inject<Db>) -> Result<NoContent> { … }
//!
//! // …and the resource check, which loads the row and yields it
//! #[endpoint]
//! async fn publish(post: Authorized<Publish, Post>) -> Result<PostOut> { … }
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@perm`] | [`Permission`], [`PermSet`], [`PermBits`], [`PermRef`], [`PermissionRegistry`] |
//! | [`mod@role`] | [`Role`], [`RoleSet`], [`RoleAssignment`], [`Scope`], [`RoleSource`] |
//! | [`mod@actor`] | [`Actor`], [`ActorId`], [`ActorKind`], [`ActorSource`], [`PermissionSource`] |
//! | [`mod@action`] | [`Action`], [`PathName`], and the two declaring macros |
//! | [`mod@policy`] | [`Policy`], [`ScopedPolicy`], [`Decision`], [`Obligation`], [`PolicyRegistry`] |
//! | [`mod@extract`] | [`Authorized`], [`Requires`], [`AuthzDeclaration`], [`mark_public`] |
//! | [`mod@middleware`] | [`actor_layer`] — who is asking, in the request extensions |
//! | [`mod@query`] | [`AuthorizedQuery`] — `authorized_for::<Read>` |
//! | [`mod@explain`] | [`Explanation`], [`TraceStep`], [`EXPLAIN_HEADER`] |
//! | [`mod@audit`] | [`AuditRecord`], [`AuditSink`], [`AuditConfig`], [`BatchingAuditSink`] |
//! | [`mod@table`] | [`AuditEntry`], [`TableAuditSink`] — the `moso_authz_audit` table |
//! | [`mod@redact`] | [`Redacted`] — the response that honours obligations |
//! | [`mod@testing`] | [`assert_policies_agree`](testing::assert_policies_agree) — a `Policy` and its `ScopedPolicy` cannot drift |
//! | [`mod@error`] | [`Error`], and what each variant becomes over HTTP |
//!
//! # Four decisions worth knowing before reading the code
//!
//! **`Perm` is generated in *your* crate; the trait here is [`Permission`].**
//! `moso::permissions!` produces a `#[repr(u16)] enum Perm` in the module you
//! invoke it in, because `Perm::PostsPublish` is what a call site should read.
//! A trait of the same name would collide in every prelude import, so nothing in
//! this crate is called `Perm`. The same reasoning applies to `Role`, which *is*
//! the trait's name — but it is kept out of [`prelude`] for exactly that reason.
//!
//! **A [`PermSet`] is a bitset with a hard cap.** [`MAX_PERMISSIONS`] is 256:
//! four words, `Copy`, and a check is four `AND`s. Permission checks happen many
//! times per request, and an `Arc<HashSet<String>>` lookup in a hot loop is how
//! authorization becomes the thing a profiler points at.
//!
//! **This crate does not depend on `moso-auth`.** Authorization is useful
//! without a login form: a service authorised by an API key, a job running as a
//! service principal, an operator on the command line. The seam is
//! [`ActorSource`] — one provider that turns a request into an [`Actor`]. The
//! declared edge is `authz -> [moso-orm]` and nothing else
//! (`xtask/allow/dep-edges.toml`).
//!
//! **Query-level filtering is the point.** [`AuthorizedQuery::authorized_for`]
//! contributes a `WHERE` clause from a [`ScopedPolicy`] instead of filtering
//! rows after loading. That is what makes list endpoints correct *and* fast, and
//! what makes pagination totals true.

pub mod action;
pub mod actor;
pub mod audit;
pub mod error;
pub mod explain;
pub mod extract;
pub mod middleware;
pub mod perm;
pub mod policy;
pub mod query;
pub mod redact;
pub mod role;
pub mod table;
pub mod testing;

#[cfg(test)]
mod fixture;

pub use crate::action::{Action, HasRole, PathName};
pub use crate::actor::{
    AUTH_SCHEME, Actor, ActorId, ActorIdentity, ActorKind, ActorPermissions, ActorSource,
    PermissionSource, detached_ctx, detached_ctx_for,
};
pub use crate::audit::{
    AuditConfig, AuditGuard, AuditOutcome, AuditRecord, AuditSink, BatchingAuditSink,
    MemoryAuditSink, TracingAuditSink, audit_dropped, count_dropped, flush_audit,
};
pub use crate::error::{BoxError, Error, GENERIC_DENIAL, Result};
pub use crate::explain::{EXPLAIN_HEADER, Explanation, explain_requested};
pub use crate::extract::{
    AUTHZ_EXTENSION, Authorized, AuthzDeclaration, FromPath, FromPathId, Masked, Public,
    RequireMode, Required, Requirement, Requires, ResourceSource, SOURCE_EXTENSION, boot_problems,
    declarations_of, document_problems, mark_public, mark_source, read_declaration,
    read_declarations, source_at, source_of, undeclared_operations,
};
pub use crate::middleware::actor_layer;
pub use crate::perm::{
    MAX_PERMISSIONS, PermBits, PermRef, PermSet, Permission, PermissionRegistry,
};
pub use crate::policy::{
    Decision, Obligation, Policy, PolicyCtx, PolicyRef, PolicyRegistry, ScopedPolicy, TraceStep,
};
pub use crate::query::AuthorizedQuery;
pub use crate::redact::Redacted;
pub use crate::role::{
    MAX_ROLES, MemoryRoleSource, Role, RoleAssignment, RoleSet, RoleSource, Scope, ScopeId,
};
pub use crate::table::{AUDIT_TABLE, AuditEntry, PurgeTask, TableAuditSink};

/// The version of this crate, for `moso doctor` and the boot log.
///
/// ```
/// assert!(!moso_authz::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything an application that authorises imports.
///
/// [`Role`] is deliberately absent: `moso::roles!` generates an enum of that
/// name in the application's crate, and a glob that shadowed it would be a
/// confusing error at every call site. Import it explicitly —
/// `use moso_authz::Role as RoleTrait;` — on the rare occasion you need the
/// trait by name.
///
/// ```no_run
/// use moso_authz::prelude::*;
///
/// fn allowed(decision: &Decision) -> bool {
///     decision.allowed()
/// }
/// ```
pub mod prelude {
    pub use crate::{
        Action, Actor, ActorId, ActorKind, Authorized, AuthorizedQuery, Decision, Error, HasRole,
        Obligation, PermRef, PermSet, Permission, Policy, PolicyCtx, Redacted, Requires, Result,
        Scope, ScopeId, ScopedPolicy, TraceStep,
    };
}

#[cfg(test)]
mod tests {
    /// The public surface resolves from the crate root, so an application
    /// writes `moso_authz::PermSet` and not `moso_authz::perm::PermSet`.
    #[test]
    fn the_frozen_surface_resolves_from_the_root() {
        fn exists<T>() {}

        exists::<crate::ActorId>();
        exists::<crate::ActorIdentity>();
        exists::<crate::ActorKind>();
        exists::<crate::AuditConfig>();
        exists::<crate::AuditOutcome>();
        exists::<crate::AuditRecord>();
        exists::<crate::AuthzDeclaration>();
        exists::<crate::Decision>();
        exists::<crate::Error>();
        exists::<crate::Explanation>();
        exists::<crate::FromPathId>();
        exists::<crate::Obligation>();
        exists::<crate::PermBits>();
        exists::<crate::PermRef>();
        exists::<crate::PermissionRegistry>();
        exists::<crate::PolicyCtx>();
        exists::<crate::PolicyRef>();
        exists::<crate::PolicyRegistry>();
        exists::<crate::RequireMode>();
        exists::<crate::Scope>();
        exists::<crate::ScopeId>();
        exists::<crate::TraceStep>();

        fn dyn_compatible(_: &dyn crate::AuditSink, _: &dyn crate::PermissionSource) {}
        let _ = dyn_compatible;
    }

    /// The fingerprint distinguishes registries that differ in *any* way — a
    /// name, an order, a count. That is what makes [`crate::PermBits`] safe to
    /// send across a boundary, so it is checked rather than assumed.
    #[test]
    fn the_fingerprint_separates_registries() {
        use crate::perm::fingerprint_of;

        let base = fingerprint_of(&["posts.read", "posts.write"]);
        assert_ne!(base, fingerprint_of(&["posts.write", "posts.read"]));
        assert_ne!(base, fingerprint_of(&["posts.read"]));
        assert_ne!(base, fingerprint_of(&["posts.read", "posts.writ"]));
        assert_eq!(base, fingerprint_of(&["posts.read", "posts.write"]));
    }

    /// Two name lists that concatenate to the same byte string must not
    /// collide: `["ab", "c"]` and `["a", "bc"]` are different registries.
    #[test]
    fn the_fingerprint_separates_on_boundaries() {
        use crate::perm::fingerprint_of;

        assert_ne!(fingerprint_of(&["ab", "c"]), fingerprint_of(&["a", "bc"]));
    }

    /// The bitset caps are the ones the documentation and the macros promise.
    #[test]
    fn the_caps_are_what_the_macros_check_against() {
        assert_eq!(crate::MAX_PERMISSIONS, 256);
        assert_eq!(crate::perm::PERM_WORDS, 4);
        assert_eq!(crate::MAX_ROLES, 64);
    }
}
