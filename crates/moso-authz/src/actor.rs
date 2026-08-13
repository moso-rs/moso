//! [`Actor`] — who is asking, with their roles and permissions already resolved.
//!
//! # Why the actor is built by the application, not by this crate
//!
//! Resolving an actor means knowing who is authenticated, which is
//! `moso-auth`'s job. This crate deliberately does **not** depend on
//! `moso-auth` (`xtask/allow/dep-edges.toml`: `authz -> [moso-orm]`), because
//! authorization is useful without it — a service authorised by an API key, a
//! job running as a service principal, a CLI acting as an operator. The seam is
//! [`ActorSource`]: the application registers one provider that turns a request
//! into an [`Actor`], and everything in this crate works against that.

use core::marker::PhantomData;

use moso_core::BoxFuture;
use moso_core::ctx::RequestCtx;
use moso_core::di::{Dependency, ProviderReq};
use moso_openapi::OperationBuilder;
use serde::{Deserialize, Serialize};

use crate::{Action, Decision, PermSet, Policy, PolicyCtx, Role, RoleSet, Scope};

/// What kind of thing is acting.
///
/// Recorded on every audit entry, because "alice deleted it" and "alice's
/// deploy key deleted it" are different incidents.
///
/// ```
/// use moso_authz::ActorKind;
///
/// assert_eq!(ActorKind::default(), ActorKind::Anonymous);
/// assert!(!ActorKind::Anonymous.is_authenticated());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ActorKind {
    /// Nobody is authenticated. Holds no roles and no permissions.
    #[default]
    Anonymous,
    /// A human, through a session or a bearer token.
    User,
    /// A machine, through an API key. Its permissions are the *intersection* of
    /// the key's scopes and the owning user's.
    ApiKey,
    /// Another service, through mutual TLS or a service token.
    Service,
    /// A background job, running as whoever enqueued it.
    Job,
}

impl ActorKind {
    /// Whether anything is authenticated at all.
    ///
    /// ```
    /// use moso_authz::ActorKind;
    ///
    /// assert!(ActorKind::User.is_authenticated());
    /// ```
    #[must_use]
    pub const fn is_authenticated(self) -> bool {
        !matches!(self, Self::Anonymous)
    }

    /// The name used in audit records and explain traces.
    ///
    /// ```
    /// use moso_authz::ActorKind;
    ///
    /// assert_eq!(ActorKind::ApiKey.as_str(), "api_key");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Anonymous => "anonymous",
            Self::User => "user",
            Self::ApiKey => "api_key",
            Self::Service => "service",
            Self::Job => "job",
        }
    }
}

/// Who an actor is, as a string.
///
/// A string for the same reason [`ScopeId`](crate::ScopeId) is: the subject is
/// a UUID here, a bigint there and an email somewhere else, and an audit trail
/// has to record all three. `From` impls cover the common shapes.
///
/// ```
/// use moso_authz::ActorId;
///
/// let anonymous = ActorId::anonymous();
/// assert!(anonymous.is_anonymous());
/// assert_eq!(ActorId::new("usr_123").as_str(), "usr_123");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ActorId(String);

impl ActorId {
    /// Wrap a subject identifier.
    ///
    /// ```
    /// use moso_authz::ActorId;
    ///
    /// assert_eq!(ActorId::new("usr_1").as_str(), "usr_1");
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier used for an unauthenticated request.
    ///
    /// ```
    /// use moso_authz::ActorId;
    ///
    /// assert!(ActorId::anonymous().is_anonymous());
    /// ```
    #[must_use]
    pub fn anonymous() -> Self {
        Self("anonymous".to_owned())
    }

    /// The identifier.
    ///
    /// ```
    /// use moso_authz::ActorId;
    ///
    /// assert_eq!(ActorId::new("x").as_str(), "x");
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this is [`ActorId::anonymous`].
    ///
    /// ```
    /// use moso_authz::ActorId;
    ///
    /// assert!(!ActorId::new("usr_1").is_anonymous());
    /// ```
    #[must_use]
    pub fn is_anonymous(&self) -> bool {
        self.0 == "anonymous"
    }
}

impl<E: moso_schema::IdMarker> From<moso_schema::Id<E>> for ActorId {
    fn from(value: moso_schema::Id<E>) -> Self {
        Self(value.to_string())
    }
}

impl core::fmt::Display for ActorId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who is asking, with roles and permissions already resolved.
///
/// One resolution per request, memoised by the dependency cache. Every check
/// after that is a bitwise `AND` — target overhead is under a microsecond, and
/// the way to hit it is to do no work in `has`.
///
/// ```no_run
/// use moso_authz::{Actor, Role};
///
/// fn may_publish<R: Role>(actor: &Actor<R>, publish: R::Perm) -> bool {
///     actor.has(publish)
/// }
/// ```
///
/// # Writing a policy for it
///
/// [`Policy`] is this crate's trait and `Actor<R>` is this crate's type, so an
/// `impl Policy<Edit, Post> for Actor<Role>` looks like it should fall foul of
/// the orphan rule. It does not: `Edit` and `Post` are the application's types,
/// which is enough. This is why the action markers are types and not strings.
pub struct Actor<R: Role> {
    /// Who.
    id: ActorId,
    /// What kind of thing.
    kind: ActorKind,
    /// The scope this actor was resolved for.
    scope: Scope,
    /// The static roles held in that scope.
    roles: RoleSet<R>,
    /// Every permission, static roles and dynamic grants unioned, then
    /// intersected with an API key's scopes when there is one.
    permissions: PermSet<R::Perm>,
    /// The credential's own ceiling, when the actor is an API key or a token
    /// with a reduced scope. Kept for the explain trace.
    ceiling: Option<PermSet<R::Perm>>,
}

impl<R: Role> Actor<R> {
    /// An actor with a fixed identity, roles and permissions.
    ///
    /// What an [`ActorSource`] builds. Applications rarely call it directly;
    /// tests call it constantly.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, ActorId, ActorKind, Role, RoleSet, Scope};
    /// # fn f<R: Role>(roles: RoleSet<R>) {
    /// let _: Actor<R> = Actor::new(ActorId::new("usr_1"), ActorKind::User, Scope::Global, roles);
    /// # }
    /// ```
    #[must_use]
    pub fn new(id: ActorId, kind: ActorKind, scope: Scope, roles: RoleSet<R>) -> Self {
        Self {
            id,
            kind,
            scope,
            roles,
            permissions: roles.permissions(),
            ceiling: None,
        }
    }

    /// The anonymous actor: no roles, no permissions, global scope.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, Role};
    /// # fn f<R: Role>() { let _: Actor<R> = Actor::anonymous(); }
    /// ```
    #[must_use]
    pub fn anonymous() -> Self {
        Self {
            id: ActorId::anonymous(),
            kind: ActorKind::Anonymous,
            scope: Scope::Global,
            roles: RoleSet::empty(),
            permissions: PermSet::empty(),
            ceiling: None,
        }
    }

    /// Add permissions granted outside any static role.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, PermSet, Role};
    /// # fn f<R: Role>(a: Actor<R>, extra: PermSet<R::Perm>) { let _ = a.with_permissions(extra); }
    /// ```
    #[must_use]
    pub fn with_permissions(mut self, extra: PermSet<R::Perm>) -> Self {
        self.permissions = self.permissions.union(extra);
        // Adding after a ceiling was applied must not lift it: an API key's
        // scopes are a maximum, not a starting point.
        if let Some(ceiling) = self.ceiling {
            self.permissions = self.permissions.intersection(ceiling);
        }
        self
    }

    /// Cap the actor's permissions at a credential's own scopes.
    ///
    /// An API key can never grant more than its owner has, so this
    /// *intersects*. Applying it twice is idempotent.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, PermSet, Role};
    /// # fn f<R: Role>(a: Actor<R>, scopes: PermSet<R::Perm>) { let _ = a.capped_at(scopes); }
    /// ```
    #[must_use]
    pub fn capped_at(mut self, ceiling: PermSet<R::Perm>) -> Self {
        self.permissions = self.permissions.intersection(ceiling);
        self.ceiling = Some(match self.ceiling {
            // Two credentials in a chain each narrow: the tighter one wins.
            Some(existing) => existing.intersection(ceiling),
            None => ceiling,
        });
        self
    }

    /// The credential's own ceiling, when there is one.
    ///
    /// Kept so an explain trace can say "the key does not carry it" rather than
    /// "you do not have it", which are different problems with different fixes.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, PermSet, Role};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: Option<PermSet<R::Perm>> = a.ceiling(); }
    /// ```
    #[must_use]
    pub fn ceiling(&self) -> Option<PermSet<R::Perm>> {
        self.ceiling
    }

    /// The same actor, resolved for a different scope.
    ///
    /// Used by an [`ActorSource`] that resolves roles per request: the tenant
    /// is known from the path or the host, and the roles are looked up for it.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, Role, RoleSet, Scope};
    /// # fn f<R: Role>(a: Actor<R>, roles: RoleSet<R>) { let _ = a.in_scope(Scope::Global, roles); }
    /// ```
    #[must_use]
    pub fn in_scope(mut self, scope: Scope, roles: RoleSet<R>) -> Self {
        self.scope = scope;
        self.roles = roles;
        self.permissions = match self.ceiling {
            Some(ceiling) => roles.permissions().intersection(ceiling),
            None => roles.permissions(),
        };
        self
    }

    /// Who.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, ActorId, Role};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: &ActorId = a.id(); }
    /// ```
    #[must_use]
    pub fn id(&self) -> &ActorId {
        &self.id
    }

    /// What kind of thing is acting.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, ActorKind, Role};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: ActorKind = a.kind(); }
    /// ```
    #[must_use]
    pub fn kind(&self) -> ActorKind {
        self.kind
    }

    /// The scope this actor was resolved for.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, Role, Scope};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: &Scope = a.scope(); }
    /// ```
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The static roles held.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, Role, RoleSet};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: RoleSet<R> = a.roles(); }
    /// ```
    #[must_use]
    pub fn roles(&self) -> RoleSet<R> {
        self.roles
    }

    /// Every permission held.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, PermSet, Role};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: PermSet<R::Perm> = a.permissions(); }
    /// ```
    #[must_use]
    pub fn permissions(&self) -> PermSet<R::Perm> {
        self.permissions
    }

    /// Whether the actor holds `permission`. One `AND`.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, Role};
    /// # fn f<R: Role>(a: &Actor<R>, p: R::Perm) { let _: bool = a.has(p); }
    /// ```
    #[must_use]
    pub fn has(&self, permission: R::Perm) -> bool {
        self.permissions.has(permission)
    }

    /// Whether the actor holds every permission in `required`.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, PermSet, Role};
    /// # fn f<R: Role>(a: &Actor<R>, r: PermSet<R::Perm>) { let _: bool = a.has_all(r); }
    /// ```
    #[must_use]
    pub fn has_all(&self, required: PermSet<R::Perm>) -> bool {
        self.permissions.has_all(required)
    }

    /// Whether the actor holds any permission in `required`.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, PermSet, Role};
    /// # fn f<R: Role>(a: &Actor<R>, r: PermSet<R::Perm>) { let _: bool = a.has_any(r); }
    /// ```
    #[must_use]
    pub fn has_any(&self, required: PermSet<R::Perm>) -> bool {
        self.permissions.has_any(required)
    }

    /// Whether the actor holds `role`.
    ///
    /// Prefer [`has`](Actor::has): checking a permission survives a role being
    /// renamed or split, and checking a role does not.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, Role};
    /// # fn f<R: Role>(a: &Actor<R>, r: R) { let _: bool = a.is(r); }
    /// ```
    #[must_use]
    pub fn is(&self, role: R) -> bool {
        self.roles.has(role)
    }

    /// Run a policy imperatively.
    ///
    /// The escape hatch for a check that does not fit
    /// [`Authorized`](crate::Authorized) — one inside a loop, one on a resource
    /// that came from somewhere other than the path.
    ///
    /// ```no_run
    /// use moso_authz::{Actor, Decision, Policy, Role};
    ///
    /// async fn check<R, A, Res>(actor: &Actor<R>, action: A, resource: &Res) -> Decision
    /// where
    ///     R: Role,
    ///     A: moso_authz::Action,
    ///     Res: Sync,
    ///     Actor<R>: Policy<A, Res>,
    /// {
    ///     actor.can(action, resource).await
    /// }
    /// ```
    pub async fn can<A, Res>(&self, action: A, resource: &Res) -> Decision
    where
        A: Action,
        Res: Sync,
        Self: Policy<A, Res>,
    {
        self.can_with(
            action,
            resource,
            &PolicyCtx::new(self.id.clone(), self.scope.clone()),
        )
        .await
    }

    /// Run a policy with a context the caller built.
    ///
    /// [`can`](Actor::can) is this with a fresh detached context. Use this one
    /// inside a request, where the correlation id and the explain flag are
    /// already known — an audit record with no request id joins to nothing.
    ///
    /// ```no_run
    /// use moso_authz::{Actor, Decision, Policy, PolicyCtx, Role};
    ///
    /// async fn check<R, A, Res>(actor: &Actor<R>, action: A, r: &Res, ctx: &PolicyCtx) -> Decision
    /// where
    ///     R: Role,
    ///     A: moso_authz::Action,
    ///     Res: Sync,
    ///     Actor<R>: Policy<A, Res>,
    /// {
    ///     actor.can_with(action, r, ctx).await
    /// }
    /// ```
    pub async fn can_with<A, Res>(&self, action: A, resource: &Res, ctx: &PolicyCtx) -> Decision
    where
        A: Action,
        Res: Sync,
        Self: Policy<A, Res>,
    {
        use tracing::Instrument as _;

        // Instrumented rather than entered: a span guard held across an `.await`
        // attaches the span to whatever the executor runs next.
        let span = tracing::debug_span!(
            target: "moso::authz",
            "authz.policy",
            action = A::NAME,
            actor = %self.id,
        );
        let decision = self.allows(action, resource, ctx).instrument(span).await;
        tracing::debug!(
            target: "moso::authz",
            action = A::NAME,
            allowed = decision.allowed(),
            reason = decision.reason(),
            "policy decided"
        );
        decision
    }
}

impl<R: Role> Clone for Actor<R> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            kind: self.kind,
            scope: self.scope.clone(),
            roles: self.roles,
            permissions: self.permissions,
            ceiling: self.ceiling,
        }
    }
}

impl<R: Role> core::fmt::Debug for Actor<R> {
    /// Names and permission names, never bit patterns — a log line has to be
    /// readable by the person debugging "why can't this user do X".
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Actor")
            .field("id", &self.id)
            .field("kind", &self.kind.as_str())
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

/// Turns a request into an [`Actor`].
///
/// The one thing an application must provide to use this crate. A typical
/// implementation reads `moso-auth`'s session or API key, loads the subject's
/// role assignments, and caches the result in a KV namespace keyed partly on
/// the user's `auth_hash` — so a role change invalidates the cache and a
/// password reset does too.
///
/// Dyn-compatible, because it is fetched from the provider map.
///
/// ```no_run
/// use moso_authz::{Actor, ActorSource, Role};
/// use moso_core::BoxFuture;
/// use moso_core::ctx::RequestCtx;
///
/// /// Everybody is anonymous. The starting point, and what a test uses.
/// pub struct AlwaysAnonymous;
///
/// impl<R: Role> ActorSource<R> for AlwaysAnonymous {
///     fn actor<'a>(&'a self, _ctx: &'a RequestCtx) -> BoxFuture<'a, moso_core::Result<Actor<R>>> {
///         Box::pin(async { Ok(Actor::anonymous()) })
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot produce an `Actor`",
    label = "not an actor source",
    note = "an actor source implements `actor(&self, ctx)`, turning a request into who is asking",
    note = "help: register one at boot — `.provide_dyn::<dyn ActorSource<Role>>(source)` — \
            because `Actor<Role>` is resolved through it and `Depends<Actor<Role>>` fails at \
            boot without it",
    note = "help: with `moso-auth`, the source reads the session or API key and loads the \
            subject's role assignments"
)]
pub trait ActorSource<R: Role>: Send + Sync + 'static {
    /// Resolve the actor for this request.
    ///
    /// # Errors
    ///
    /// A [`moso_core::Error`]: a 401 when a credential was presented and is
    /// invalid, a 503 when the role store is unreachable. An *absent*
    /// credential is not an error — it is [`Actor::anonymous`], and the
    /// decision to refuse belongs to the policy.
    fn actor<'a>(&'a self, ctx: &'a RequestCtx) -> BoxFuture<'a, moso_core::Result<Actor<R>>>;
}

impl<R: Role> Dependency for Actor<R> {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<dyn ActorSource<R>>()];

    fn describe(op: &mut OperationBuilder) {
        describe_authenticated(op);
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        // `actor_layer` resolves the actor *before* the request context exists,
        // because the audit trail needs the identity in the extensions the
        // context snapshots. Reading its result back is what keeps a request
        // that is both attributed and authorised to one actor lookup: the
        // dependency cache cannot help, since it did not exist yet.
        if let Some(actor) = ctx.extension::<Self>() {
            return Ok(actor);
        }
        let source = ctx.provider::<dyn ActorSource<R>>()?;
        source.actor(ctx).await
    }
}

/// Who is acting, without naming the role type.
///
/// `#[requires]` names the *permission* enum and never the role enum, so the
/// audit entry for a capability denial cannot ask for an `Actor<R>`. This is the
/// erased form that both halves can name: [`actor_layer`](crate::actor_layer)
/// puts one in the request extensions, and the audit path reads it back.
///
/// Insert one from your own middleware if you resolve the actor yourself:
///
/// ```
/// use moso_authz::{ActorId, ActorIdentity, ActorKind, Scope};
///
/// let identity = ActorIdentity::new(ActorId::new("usr_1"), ActorKind::User, Scope::Global);
/// assert_eq!(identity.id().as_str(), "usr_1");
///
/// // …and the starting point, which is what an unattributed request records.
/// assert!(ActorIdentity::anonymous().id().is_anonymous());
/// ```
///
/// # Carrying it across a process boundary
///
/// [`to_wire`](ActorIdentity::to_wire) renders it to an opaque, non-secret
/// string and [`from_wire`](ActorIdentity::from_wire) reads it back. That is the
/// contract with `moso-jobs`, which carries the string on a queued row without
/// depending on this crate: an application enqueues under
/// `moso_jobs::actor::scope(identity.to_wire(), …)` and a job body recovers the
/// enqueuer with `ActorIdentity::from_wire(ctx.actor_identity()?)`. Only the
/// identity travels — id, kind, scope — never a credential.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorIdentity {
    /// Who.
    id: ActorId,
    /// What kind of thing.
    kind: ActorKind,
    /// Where they were acting.
    scope: Scope,
}

impl ActorIdentity {
    /// Name an actor.
    ///
    /// ```
    /// use moso_authz::{ActorId, ActorIdentity, ActorKind, Scope};
    ///
    /// let _ = ActorIdentity::new(ActorId::new("svc_1"), ActorKind::Service, Scope::Global);
    /// ```
    #[must_use]
    pub fn new(id: ActorId, kind: ActorKind, scope: Scope) -> Self {
        Self { id, kind, scope }
    }

    /// The identity of an unattributed request.
    ///
    /// ```
    /// use moso_authz::{ActorIdentity, ActorKind};
    ///
    /// assert_eq!(ActorIdentity::anonymous().kind(), ActorKind::Anonymous);
    /// ```
    #[must_use]
    pub fn anonymous() -> Self {
        Self::new(ActorId::anonymous(), ActorKind::Anonymous, Scope::Global)
    }

    /// Who.
    ///
    /// ```
    /// use moso_authz::ActorIdentity;
    ///
    /// assert_eq!(ActorIdentity::anonymous().id().as_str(), "anonymous");
    /// ```
    #[must_use]
    pub fn id(&self) -> &ActorId {
        &self.id
    }

    /// What kind of thing was acting.
    ///
    /// ```
    /// use moso_authz::{ActorIdentity, ActorKind};
    ///
    /// assert!(!ActorIdentity::anonymous().kind().is_authenticated());
    /// ```
    #[must_use]
    pub fn kind(&self) -> ActorKind {
        self.kind
    }

    /// Where they were acting.
    ///
    /// ```
    /// use moso_authz::ActorIdentity;
    ///
    /// assert!(ActorIdentity::anonymous().scope().is_global());
    /// ```
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The three parts, for an [`AuditRecord`](crate::AuditRecord) constructor.
    ///
    /// ```
    /// use moso_authz::{ActorId, ActorIdentity, ActorKind, Scope};
    ///
    /// let (id, kind, scope) = ActorIdentity::anonymous().into_parts();
    /// assert!(id.is_anonymous());
    /// assert_eq!(kind, ActorKind::Anonymous);
    /// assert!(scope.is_global());
    /// ```
    #[must_use]
    pub fn into_parts(self) -> (ActorId, ActorKind, Scope) {
        (self.id, self.kind, self.scope)
    }

    /// Render the identity to an opaque, non-secret wire string.
    ///
    /// For carrying an actor across a process boundary — onto a background job's
    /// row, into an audit record. It encodes **only** who was acting: the
    /// subject id, the actor kind, the scope. It never contains a credential, a
    /// session token or a permission set, so a job runs with the enqueuer's
    /// identity *for audit* and a worker that needs their live authority
    /// re-resolves it. `moso-jobs` treats the string as opaque; only this crate
    /// gives it meaning, through [`from_wire`](ActorIdentity::from_wire).
    ///
    /// ```
    /// use moso_authz::{ActorId, ActorIdentity, ActorKind, Scope};
    ///
    /// let identity = ActorIdentity::new(ActorId::new("usr_1"), ActorKind::User, Scope::Global);
    /// let wire = identity.to_wire();
    /// assert_eq!(ActorIdentity::from_wire(&wire), Some(identity));
    /// ```
    #[must_use]
    pub fn to_wire(&self) -> String {
        // A struct of a string, a fieldless enum and a scope cannot fail to
        // serialise, so the empty-string fallback is unreachable in practice;
        // it is chosen anyway because it decodes back to `None` — an
        // unattributed job — rather than to a wrong actor.
        serde_json::to_string(self).unwrap_or_default()
    }

    /// Recover an identity written by [`to_wire`](ActorIdentity::to_wire).
    ///
    /// `None` when the string is not one this version wrote — a corrupt or
    /// forward-incompatible value attributes the work to nobody rather than to
    /// the wrong somebody, which is the safe direction for an audit trail.
    ///
    /// ```
    /// use moso_authz::{ActorId, ActorIdentity, ActorKind, Scope};
    ///
    /// let identity = ActorIdentity::new(ActorId::new("svc_1"), ActorKind::Service, Scope::Global);
    /// assert_eq!(ActorIdentity::from_wire(&identity.to_wire()), Some(identity));
    /// assert!(ActorIdentity::from_wire("not a wire identity").is_none());
    /// ```
    #[must_use]
    pub fn from_wire(wire: &str) -> Option<Self> {
        serde_json::from_str(wire).ok()
    }
}

impl<R: Role> From<&Actor<R>> for ActorIdentity {
    fn from(actor: &Actor<R>) -> Self {
        actor.identity()
    }
}

/// The 401 and the security requirement that resolving an actor implies.
///
/// Shared by [`Actor`]'s `Dependency` impl and by
/// [`Authorized`](crate::Authorized)'s `Extract` impl, so the two cannot drift.
/// The requirement names the scheme rather than defining it: which scheme
/// authenticates is `moso-auth`'s business, and this crate deliberately does not
/// depend on it.
pub(crate) fn describe_authenticated(op: &mut OperationBuilder) {
    op.security(moso_openapi::security::SecurityRequirement::scheme(
        AUTH_SCHEME,
    ));
    op.response(
        401,
        moso_openapi::ResponseSpec::problem(
            "no credentials were presented, or the ones presented do not identify anyone.",
        ),
    );
    op.response(
        503,
        moso_openapi::ResponseSpec::problem(
            "the store the caller's roles come from could not be reached. `retryable` is `true`: \
             an unreachable role store is never degraded to \"no permissions\", because that turns \
             a cache outage into a site-wide lockout.",
        ),
    );
}

/// The name every authorization-aware operation lists as its security scheme.
///
/// One name, so an application that registers a bearer scheme and one that
/// registers a cookie scheme both produce a document whose operations point at
/// *their* scheme. Defining it is the application's job — through
/// `moso-auth`, or by hand — because this crate does not know how the caller
/// authenticated.
///
/// ```
/// assert_eq!(moso_authz::AUTH_SCHEME, "moso_auth");
/// ```
pub const AUTH_SCHEME: &str = "moso_auth";

/// Where a `#[requires]` guard reads the caller's permissions from.
///
/// Type-erased on purpose. The guard is generated from
/// `#[requires(Perm::PostsCreate)]`, which names the permission type but not the
/// role type, so it cannot ask for `Actor<R>`. It asks for [`PermBits`](crate::PermBits) instead,
/// and the fingerprint check catches the one way that could go wrong: bits from
/// a different registry, where every bit would mean something else.
///
/// ```no_run
/// use moso_authz::{PermBits, PermissionSource};
/// use moso_core::BoxFuture;
/// use moso_core::ctx::RequestCtx;
///
/// async fn read(source: &dyn PermissionSource, ctx: &RequestCtx)
///     -> moso_core::Result<PermBits>
/// {
///     source.permissions(ctx).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot supply permissions to `#[requires]`",
    label = "not a permission source",
    note = "a permission source implements `permissions(&self, ctx)`, returning the caller's \
            `PermBits`",
    note = "help: `Actor<R>` has one — register it with \
            `.provide_dyn::<dyn PermissionSource>(ActorPermissions::<Role>::new())`",
    note = "help: `#[requires]` cannot name your role type, which is why this is erased; the \
            fingerprint on `PermBits` is what makes that safe"
)]
pub trait PermissionSource: Send + Sync + 'static {
    /// The caller's permissions for this request.
    ///
    /// # Errors
    ///
    /// A [`moso_core::Error`], typically the 503 an unreachable role store
    /// produces.
    fn permissions<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<crate::PermBits>>;

    /// The registry these bits belong to.
    ///
    /// Compared against the `#[requires]` set's fingerprint at boot, so a
    /// mismatch is a boot error and not a wrong answer at runtime.
    fn fingerprint(&self) -> u64;
}

/// Adapts an [`ActorSource`] into a [`PermissionSource`].
///
/// The one line an application writes to make `#[requires]` work once it has an
/// actor source:
///
/// ```text
/// App::new()
///     .provide_dyn::<dyn ActorSource<Role>>(Arc::new(MyActorSource::new(db)))
///     .provide_dyn::<dyn PermissionSource>(Arc::new(ActorPermissions::<Role>::new()))
/// ```
///
/// # Why it takes no arguments
///
/// It resolves the actor through the *request cache*
/// ([`RequestCtx::depends`]), not through a source it holds: a handler with
/// `#[requires]` **and** an `Authorized<..>` parameter must resolve its actor
/// once, and the cache is what guarantees that. The source it ends up calling
/// is the `dyn ActorSource<R>` in the provider map, which the application has
/// to register anyway — `Actor<R>`'s `PROVIDER_REQ` makes a missing one a boot
/// error.
///
/// ```no_run
/// use moso_authz::{ActorPermissions, Role};
///
/// fn adapt<R: Role>() -> ActorPermissions<R> {
///     ActorPermissions::new()
/// }
/// ```
pub struct ActorPermissions<R: Role> {
    /// The role type, which holds no data.
    marker: PhantomData<fn() -> R>,
}

impl<R: Role> ActorPermissions<R> {
    /// An adapter over whatever [`ActorSource`] the provider map holds.
    ///
    /// ```no_run
    /// # use moso_authz::{ActorPermissions, Role};
    /// # fn f<R: Role>() { let _ = ActorPermissions::<R>::new(); }
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            marker: PhantomData,
        }
    }
}

impl<R: Role> Default for ActorPermissions<R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R: Role> core::fmt::Debug for ActorPermissions<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("ActorPermissions")
    }
}

impl<R: Role> PermissionSource for ActorPermissions<R> {
    fn permissions<'a>(
        &'a self,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<crate::PermBits>> {
        Box::pin(async move {
            // Through the request cache, not through `ActorSource` directly: a
            // handler with `#[requires]` *and* an `Authorized<..>` parameter
            // must resolve its actor once, not twice.
            let actor = ctx.depends::<Actor<R>>().await?;
            Ok(actor.permissions().to_bits())
        })
    }

    fn fingerprint(&self) -> u64 {
        <R::Perm as crate::Permission>::FINGERPRINT
    }
}

impl<R: Role> Actor<R> {
    /// The [`Explanation`](crate::Explanation) for a decision this actor made.
    ///
    /// Fills in the parts only the actor knows — the roles held, the scope each
    /// is held in, and every permission with its description — so
    /// `moso authz explain` and the `X-Moso-Authz-Explain` header produce the
    /// *same* block from the same decision. That is acceptance criterion 6, and
    /// having one function is how it stays true.
    ///
    /// The `(from Editor)` annotation is attached only when exactly one role is
    /// held, because with two it would be a guess.
    ///
    /// ```no_run
    /// use moso_authz::{Actor, Decision, Explanation, Role};
    ///
    /// fn explain<R: Role>(actor: &Actor<R>, decision: &Decision) -> Explanation {
    ///     actor.explain(decision)
    /// }
    /// ```
    #[must_use]
    pub fn explain(&self, decision: &crate::Decision) -> crate::Explanation {
        let roles: Vec<(String, Scope)> = self
            .roles
            .names()
            .into_iter()
            .map(|name| (name.to_owned(), self.scope.clone()))
            .collect();
        let permissions = self.permissions.iter().map(crate::PermRef::of).collect();

        let explanation =
            crate::Explanation::of(decision, &self.id, &self.scope).with_grants(roles, permissions);
        match self.roles.names().as_slice() {
            [only] => explanation.granted_by(*only),
            _ => explanation,
        }
    }

    /// Who this actor is, for an audit record and an explain trace.
    ///
    /// Erased so the audit path does not have to name `R` — which it cannot,
    /// because `#[requires]` names the permission enum and not the role enum.
    ///
    /// ```no_run
    /// # use moso_authz::{Actor, ActorIdentity, Role};
    /// # fn f<R: Role>(a: &Actor<R>) { let _: ActorIdentity = a.identity(); }
    /// ```
    #[must_use]
    pub fn identity(&self) -> ActorIdentity {
        ActorIdentity::new(self.id.clone(), self.kind, self.scope.clone())
    }
}

/// The [`PolicyCtx`] an imperative [`Actor::can`] builds when there is no
/// request in scope.
///
/// Exposed so a job or a test can run a policy the same way a handler does.
///
/// ```
/// use moso_authz::{detached_ctx, PolicyCtx};
///
/// let ctx: PolicyCtx = detached_ctx();
/// assert!(!ctx.explain());
/// assert!(ctx.actor().is_anonymous());
/// assert_eq!(ctx.request_id(), None);
/// ```
#[must_use]
pub fn detached_ctx() -> PolicyCtx {
    PolicyCtx::new(ActorId::anonymous(), Scope::Global)
}

/// A detached [`PolicyCtx`] that reflects a specific enqueuer.
///
/// [`detached_ctx`] attributes an out-of-request check to nobody; this
/// attributes it to the actor a background job was enqueued by, whose
/// [`ActorIdentity`] travelled on the job row and was recovered with
/// [`ActorIdentity::from_wire`]. The job runs *as* that subject for audit — the
/// context carries their id and scope — while a worker that needs their live
/// authority still re-resolves it, so a permission revoked since the enqueue is
/// already gone.
///
/// ```
/// use moso_authz::{detached_ctx_for, ActorId, ActorIdentity, ActorKind, Scope};
///
/// let enqueuer = ActorIdentity::new(ActorId::new("usr_1"), ActorKind::User, Scope::Global);
/// let ctx = detached_ctx_for(&enqueuer);
/// assert_eq!(ctx.actor().as_str(), "usr_1");
/// assert!(!ctx.explain());
/// ```
#[must_use]
pub fn detached_ctx_for(identity: &ActorIdentity) -> PolicyCtx {
    PolicyCtx::new(identity.id().clone(), identity.scope().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Perm, Post, Publish, Role, actor};

    #[test]
    fn an_actor_resolves_its_roles_into_permissions_once() {
        let alice = actor("usr_1", [Role::Editor]);

        assert_eq!(alice.id().as_str(), "usr_1");
        assert_eq!(alice.kind(), ActorKind::User);
        assert!(alice.scope().is_global());
        assert!(alice.is(Role::Editor));
        assert!(!alice.is(Role::Admin));
        assert!(alice.has(Perm::PostsRead));
        assert!(!alice.has(Perm::AdminAccess));
        assert_eq!(alice.permissions(), Role::Editor.permissions());
    }

    /// Deny by default: the starting point holds nothing at all.
    #[test]
    fn the_anonymous_actor_holds_nothing() {
        let nobody: Actor<Role> = Actor::anonymous();

        assert!(nobody.id().is_anonymous());
        assert!(!nobody.kind().is_authenticated());
        assert!(nobody.roles().is_empty());
        assert!(nobody.permissions().is_empty());
        for permission in Perm::ALL.iter().copied() {
            assert!(!nobody.has(permission));
        }
    }

    #[test]
    fn direct_grants_are_added_to_the_role_permissions() {
        let alice =
            actor("usr_1", [Role::Viewer]).with_permissions(PermSet::of([Perm::AdminAccess]));

        assert!(alice.has(Perm::PostsRead), "the role's permissions survive");
        assert!(alice.has(Perm::AdminAccess), "the direct grant applies");
    }

    /// An API key can never grant more than its owner has.
    #[test]
    fn a_ceiling_intersects_and_is_idempotent() {
        let key_scopes = PermSet::of([Perm::PostsRead, Perm::AdminSettings]);
        let alice = actor("usr_1", [Role::Editor]).capped_at(key_scopes);

        assert!(alice.has(Perm::PostsRead));
        assert!(
            !alice.has(Perm::AdminSettings),
            "the key listed a scope the user does not hold",
        );
        assert!(!alice.has(Perm::PostsUpdate), "the key does not carry it");
        assert_eq!(alice.ceiling(), Some(key_scopes));

        let twice = alice.clone().capped_at(key_scopes);
        assert_eq!(twice.permissions(), alice.permissions());
    }

    /// Adding permissions after a ceiling was applied must not lift it — that
    /// is how a scoped credential quietly becomes an unscoped one.
    #[test]
    fn a_later_grant_cannot_escape_the_ceiling() {
        let alice = actor("usr_1", [Role::Owner])
            .capped_at(PermSet::of([Perm::PostsRead]))
            .with_permissions(PermSet::of([Perm::AdminSettings]));

        assert!(alice.has(Perm::PostsRead));
        assert!(!alice.has(Perm::AdminSettings));
    }

    #[test]
    fn two_credentials_in_a_chain_take_the_tighter_ceiling() {
        let alice = actor("usr_1", [Role::Owner])
            .capped_at(PermSet::of([Perm::PostsRead, Perm::PostsUpdate]))
            .capped_at(PermSet::of([Perm::PostsRead, Perm::AdminAccess]));

        assert_eq!(alice.permissions(), PermSet::of([Perm::PostsRead]));
    }

    #[test]
    fn resolving_for_another_scope_replaces_the_roles_and_keeps_the_ceiling() {
        let acme = Scope::Org(crate::ScopeId::new("acme"));
        let alice = actor("usr_1", [Role::Viewer])
            .capped_at(PermSet::of([Perm::PostsRead, Perm::PostsPublish]))
            .in_scope(acme.clone(), RoleSet::of([Role::Admin]));

        assert_eq!(alice.scope(), &acme);
        assert!(alice.is(Role::Admin));
        assert!(alice.has(Perm::PostsPublish));
        assert!(!alice.has(Perm::AdminAccess), "still capped by the key");
    }

    #[test]
    fn has_all_and_has_any_delegate_to_the_bitset() {
        let alice = actor("usr_1", [Role::Editor]);

        assert!(alice.has_all(PermSet::of([Perm::PostsRead, Perm::PostsUpdate])));
        assert!(!alice.has_all(PermSet::of([Perm::PostsRead, Perm::AdminAccess])));
        assert!(alice.has_any(PermSet::of([Perm::PostsRead, Perm::AdminAccess])));
    }

    /// A log line has to be readable by the person debugging "why can't this
    /// user do X", and must not dump a bit pattern or a permission list that
    /// grows without bound.
    #[test]
    fn debug_names_the_actor_without_dumping_the_bitset() {
        let rendered = format!("{:?}", actor("usr_1", [Role::Editor]));

        assert!(rendered.contains("usr_1"), "{rendered}");
        assert!(rendered.contains("user"), "{rendered}");
        assert!(!rendered.contains("words"), "{rendered}");
    }

    #[test]
    fn the_identity_bundle_is_what_an_audit_record_needs() {
        let identity = actor("usr_1", [Role::Viewer]).identity();

        assert_eq!(identity.id().as_str(), "usr_1");
        assert_eq!(identity.kind(), ActorKind::User);
        assert!(identity.scope().is_global());

        let (id, kind, scope) = identity.into_parts();
        assert_eq!(id.as_str(), "usr_1");
        assert_eq!(kind, ActorKind::User);
        assert!(scope.is_global());
    }

    /// The erased identity is what `#[requires]` records, and it must say the
    /// same thing the actor does — including for the anonymous starting point,
    /// which is what an unattributed request looks like.
    #[test]
    fn the_erased_identity_agrees_with_the_actor_it_came_from() {
        let alice = actor("usr_1", [Role::Editor]);

        assert_eq!(crate::ActorIdentity::from(&alice), alice.identity());
        assert_ne!(crate::ActorIdentity::anonymous(), alice.identity());
        assert_eq!(
            crate::ActorIdentity::anonymous(),
            Actor::<Role>::anonymous().identity(),
        );
    }

    /// The wire form is what `moso-jobs` carries on a queued row, so it has to
    /// round-trip every part of the identity — the subject, the kind and the
    /// scope — through a string and back, including a non-global scope.
    #[test]
    fn the_identity_round_trips_through_its_wire_form() {
        let acme = Scope::Org(crate::ScopeId::new("acme"));
        let identity = ActorIdentity::new(ActorId::new("usr_1"), ActorKind::User, acme.clone());

        let wire = identity.to_wire();
        let recovered = ActorIdentity::from_wire(&wire).expect("a value this crate wrote");

        assert_eq!(recovered, identity);
        assert_eq!(recovered.id().as_str(), "usr_1");
        assert_eq!(recovered.kind(), ActorKind::User);
        assert_eq!(recovered.scope(), &acme);
    }

    /// A string this version did not write attributes the work to nobody rather
    /// than to the wrong somebody — the safe direction for an audit trail.
    #[test]
    fn a_corrupt_wire_form_decodes_to_nothing_rather_than_a_wrong_actor() {
        assert!(ActorIdentity::from_wire("").is_none());
        assert!(ActorIdentity::from_wire("not a wire identity").is_none());
        assert!(ActorIdentity::from_wire("{}").is_none());
    }

    /// Only the identity crosses the boundary: the wire form of an authenticated
    /// user carries their id, kind and scope and nothing that could be replayed
    /// as a credential.
    #[test]
    fn the_wire_form_carries_identity_and_not_authority() {
        let identity = actor("usr_1", [Role::Admin]).identity();
        let wire = identity.to_wire();

        assert!(wire.contains("usr_1"), "{wire}");
        assert!(wire.contains("user"), "{wire}");
        // The identity type holds no roles, no permissions and no token, so the
        // wire form structurally cannot carry live authority.
        assert!(!wire.contains("admin"), "{wire}");
        assert!(!wire.contains("permission"), "{wire}");
    }

    /// A detached context built for an enqueuer reflects that subject, which is
    /// what lets a background job run as whoever scheduled it rather than as
    /// nobody.
    #[test]
    fn a_detached_context_for_an_enqueuer_reflects_that_subject() {
        let acme = Scope::Org(crate::ScopeId::new("acme"));
        let enqueuer = ActorIdentity::new(ActorId::new("usr_9"), ActorKind::User, acme.clone());

        let ctx = detached_ctx_for(&enqueuer);
        assert_eq!(ctx.actor().as_str(), "usr_9");
        assert_eq!(ctx.scope(), &acme);
        assert!(!ctx.explain());
        assert_eq!(ctx.request_id(), None);

        // …and the no-argument form still attributes to nobody.
        assert!(detached_ctx().actor().is_anonymous());
    }

    // ── the imperative form ───────────────────────────────────────────────

    fn draft(author: &str) -> Post {
        Post {
            id: 1,
            author_id: author.to_owned(),
            published: false,
            title: "Hello".to_owned(),
        }
    }

    #[tokio::test]
    async fn the_author_may_publish_their_own_post() {
        let alice = actor("usr_1", [Role::Admin]);
        let decision = alice.can(Publish, &draft("usr_1")).await;

        assert!(decision.allowed());
        assert_eq!(decision.reason(), "author");
    }

    #[tokio::test]
    async fn a_stranger_may_not_publish_and_the_denial_says_why() {
        let bob = actor("usr_2", [Role::Editor]);
        let decision = bob.can(Publish, &draft("usr_1")).await;

        assert!(!decision.allowed());
        assert_eq!(decision.reason(), "not the author and not an admin");
        assert_eq!(decision.trace().len(), 3);
        assert!(decision.trace().iter().all(|step| !step.passed()));
    }

    #[tokio::test]
    async fn an_admin_overrides_the_ownership_check() {
        let root = actor("usr_9", [Role::Owner]);
        let decision = root.can(Publish, &draft("usr_1")).await;

        assert!(decision.allowed());
        assert_eq!(decision.reason(), "admin override");
    }

    #[tokio::test]
    async fn a_denial_converts_into_a_result_carrying_the_reason() {
        let bob = actor("usr_2", [Role::Viewer]);
        let error = bob
            .can(Publish, &draft("usr_1"))
            .await
            .into_result("publish", "Post#1")
            .expect_err("denied");

        assert!(error.is_denied());
        assert_eq!(error.reason(), Some("not the author and not an admin"));
    }

    #[tokio::test]
    async fn a_detached_context_carries_no_request_and_no_explain() {
        let ctx = detached_ctx();
        assert!(!ctx.explain());
        assert!(!ctx.development());
        assert_eq!(ctx.request_id(), None);

        // …and the imperative form builds one from the actor, so a policy that
        // reads `ctx.actor()` sees the actor it was asked about.
        let alice = actor("usr_1", [Role::Admin]);
        let decision = alice.can(Publish, &draft("usr_1")).await;
        assert!(decision.allowed());
    }

    #[test]
    fn the_actor_dependency_declares_the_provider_it_needs() {
        let required = <Actor<Role> as Dependency>::PROVIDER_REQ;

        assert_eq!(required.len(), 1);
        assert!(
            required[0].name().contains("ActorSource"),
            "{}",
            required[0].name(),
        );
    }

    #[test]
    fn the_actor_dependency_documents_the_401_and_the_503() {
        use moso_schema::json_schema::SchemaGenerator;

        let mut op = moso_openapi::OperationBuilder::new(SchemaGenerator::default());
        <Actor<Role> as Dependency>::describe(&mut op);
        let spec = op.into_spec();

        assert!(spec.has_response("401"));
        assert!(spec.has_response("503"));
        assert_eq!(
            spec.security.as_ref().map(Vec::len),
            Some(1),
            "an authorised operation names its security scheme",
        );
    }

    #[test]
    fn an_actor_permissions_adapter_reports_its_registry() {
        let adapter = ActorPermissions::<Role>::new();

        assert_eq!(
            adapter.fingerprint(),
            <Perm as crate::Permission>::FINGERPRINT
        );
        assert_eq!(format!("{adapter:?}"), "ActorPermissions");
        assert_eq!(
            ActorPermissions::<Role>::default().fingerprint(),
            adapter.fingerprint(),
        );
    }

    /// The seam an application implements. Both traits are fetched from the
    /// provider map, so both have to stay dyn-compatible; a `&dyn` that does
    /// not compile here is a `provide_dyn` that does not compile there.
    #[test]
    fn the_two_seams_stay_dyn_compatible() {
        struct AlwaysAnonymous;

        impl ActorSource<Role> for AlwaysAnonymous {
            fn actor<'a>(
                &'a self,
                _ctx: &'a RequestCtx,
            ) -> BoxFuture<'a, moso_core::Result<Actor<Role>>> {
                Box::pin(async { Ok(Actor::anonymous()) })
            }
        }

        let source: std::sync::Arc<dyn ActorSource<Role>> = std::sync::Arc::new(AlwaysAnonymous);
        let permissions: std::sync::Arc<dyn PermissionSource> =
            std::sync::Arc::new(ActorPermissions::<Role>::new());

        assert_eq!(
            permissions.fingerprint(),
            <Perm as crate::Permission>::FINGERPRINT,
        );
        assert_eq!(std::sync::Arc::strong_count(&source), 1);
    }
}
