//! [`Authorized`] — load the resource and check the policy in one extractor —
//! and [`Requires`]/[`Required`], the two halves of `#[requires]`.
//!
//! # The two layers, at the point of use
//!
//! A *capability* check ("may this actor publish posts at all") needs no row, so
//! it is a [`Guard`] or an [`Extract`] that reads permission bits and compares
//! them. A *resource* check ("may they publish *this* post") needs the row, so
//! it is an extractor that loads it, runs a [`Policy`] and hands
//! the row to the handler. Most authorization systems fail because they model
//! only one of the two; both are here, and both write themselves into the
//! OpenAPI document, because a middleware that can return 403 without the
//! document saying so makes the document wrong.

use core::marker::PhantomData;
use core::str::FromStr;

use moso_core::BoxFuture;
use moso_core::ctx::RequestCtx;
use moso_core::di::ProviderReq;
use moso_core::extract::Extract;
use moso_core::middleware::Guard;
use moso_openapi::{OperationBuilder, Param, ResponseSpec};
use moso_orm::{Db, Entity, Select};

use crate::actor::describe_authenticated;
use crate::{
    Actor, ActorSource, AuditConfig, AuditRecord, AuditSink, HasRole, PathName, PermSet,
    Permission, PermissionRegistry, Policy, PolicyCtx, TracingAuditSink,
};

// ---------------------------------------------------------------------------
// Where the resource comes from
// ---------------------------------------------------------------------------

/// Where the resource an [`Authorized`] checks comes from.
///
/// Implemented by [`FromPathId`] (the `{id}` convention) and
/// [`FromPath<N>`](FromPath) (any named parameter), and wrapped by
/// [`Masked<S>`](Masked). An application implements it directly for anything
/// else — a resource identified by a header, or one loaded through a service
/// rather than an entity.
///
/// # The provider it needs
///
/// The two shipped sources load through the ORM and therefore need a `Db` in
/// the provider map. That requirement is **not** in
/// [`Authorized`]'s `PROVIDER_REQ`, because an associated constant cannot
/// concatenate two generic constant slices on stable Rust and the one slot
/// there is spent on [`ActorSource`] — the wiring an application is far more
/// likely to have forgotten. A missing `Db` is reported at the first request
/// with the same message any other missing provider gets.
///
/// ```no_run
/// use moso_authz::ResourceSource;
/// use moso_core::BoxFuture;
/// use moso_core::ctx::RequestCtx;
/// use moso_openapi::OperationBuilder;
///
/// /// Always finds the same fixed resource. For a test.
/// pub struct Fixed;
///
/// impl ResourceSource<u32> for Fixed {
///     const RESOURCE: &'static str = "Fixed";
///
///     fn describe(op: &mut OperationBuilder) {
///         let _ = op;
///     }
///
///     fn load<'a>(
///         _parts: &'a mut http::request::Parts,
///         _ctx: &'a RequestCtx,
///     ) -> BoxFuture<'a, moso_core::Result<Option<u32>>> {
///         Box::pin(async { Ok(Some(7)) })
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot locate a `{R}` for `Authorized`",
    label = "not a resource source",
    note = "a resource source implements `describe` and `load`, turning a request into the row \
            the policy is asked about",
    note = "help: the default is `FromPathId`, which reads `{{id}}` and loads by primary key — \
            `Authorized<Publish, Post>` already uses it",
    note = "help: for a differently-named segment, declare the name and use it: \
            `moso_authz::path_name!(PostId = \"post_id\");` then \
            `Authorized<Publish, Post, FromPath<PostId>>`"
)]
pub trait ResourceSource<R>: Send + Sync + 'static {
    /// The resource's name, for the 404, the audit record and the explain
    /// trace. `R::NAME` for an entity.
    const RESOURCE: &'static str;

    /// Whether a *denial* should be reported as a 404 rather than a 403.
    ///
    /// `false` by default. [`Masked<S>`](Masked) is the wrapper that flips it,
    /// and the reasoning is on that type.
    const MASK_NOT_FOUND: bool = false;

    /// Contribute the path parameter and the 404 to the operation.
    fn describe(op: &mut OperationBuilder);

    /// Load the resource, or `None` when it does not exist.
    ///
    /// Boxed so the trait stays dyn-compatible, which the explain machinery
    /// needs in order to describe a source it was handed at runtime.
    ///
    /// # Errors
    ///
    /// A [`moso_core::Error`] — a 400 when the path parameter is malformed, a
    /// 503 when the database is unreachable. An *absent* resource is
    /// `Ok(None)`, because whether that becomes a 404 or a 403 is
    /// [`MASK_NOT_FOUND`](ResourceSource::MASK_NOT_FOUND)'s decision.
    fn load<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<Option<R>>>;

    /// The identifier as text, for the audit record's `Name#id`.
    ///
    /// `None` by default, which produces an audit entry naming the resource
    /// type and not the row — honest, and better than inventing an id.
    fn locate(parts: &http::request::Parts, ctx: &RequestCtx) -> Option<String> {
        let _ = (parts, ctx);
        None
    }
}

/// The default resource source: read `{id}` and load by primary key.
///
/// ```no_run
/// use moso_authz::{Authorized, FromPathId};
///
/// # struct Publish;
/// # struct Post;
/// // These two are the same type.
/// type A = Authorized<Publish, Post>;
/// type B = Authorized<Publish, Post, FromPathId>;
/// # fn f(_: A, _: B) {}
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct FromPathId;

/// A resource source that reads a named path parameter.
///
/// ```no_run
/// use moso_authz::{Authorized, FromPath};
///
/// moso_authz::path_name!(
///     /// The `{post_id}` segment.
///     PostId = "post_id"
/// );
///
/// # struct Publish;
/// # struct Post;
/// type A = Authorized<Publish, Post, FromPath<PostId>>;
/// # fn f(_: A) {}
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct FromPath<N: PathName>(PhantomData<fn() -> N>);

/// Invert the 404-before-403 rule for one endpoint.
///
/// `Authorized<Read, Invoice, Masked<FromPathId>>` returns 404 for a row the
/// caller may not see, instead of 403. Use it when *existence itself* is the
/// secret — an invoice number that increments, a document whose presence
/// implies a deal — and accept the trade: the caller can no longer tell a typo
/// from a permission problem, which is a real support cost.
///
/// It is a type, not a method, on purpose. The choice changes the endpoint's
/// documented responses, so it has to be visible in the signature that
/// documents the endpoint rather than hidden in a call inside the body.
///
/// ```no_run
/// use moso_authz::{Authorized, FromPathId, Masked};
///
/// # struct Read;
/// # struct Invoice;
/// type A = Authorized<Read, Invoice, Masked<FromPathId>>;
/// # fn f(_: A) {}
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Masked<S>(PhantomData<fn() -> S>);

impl<R> ResourceSource<R> for FromPathId
where
    R: Entity,
    R::Pk: FromStr,
{
    const RESOURCE: &'static str = R::NAME;

    fn describe(op: &mut OperationBuilder) {
        describe_path_parameter::<R>(op, ID_PARAM);
    }

    fn load<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<Option<R>>> {
        let _ = parts;
        Box::pin(load_by_path_key::<R>(ctx, ID_PARAM))
    }

    fn locate(parts: &http::request::Parts, ctx: &RequestCtx) -> Option<String> {
        let _ = parts;
        path_value(ctx, ID_PARAM).map(ToOwned::to_owned)
    }
}

impl<R, N> ResourceSource<R> for FromPath<N>
where
    R: Entity,
    R::Pk: FromStr,
    N: PathName,
{
    const RESOURCE: &'static str = R::NAME;

    fn describe(op: &mut OperationBuilder) {
        describe_path_parameter::<R>(op, N::NAME);
    }

    fn load<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<Option<R>>> {
        let _ = parts;
        Box::pin(load_by_path_key::<R>(ctx, N::NAME))
    }

    fn locate(parts: &http::request::Parts, ctx: &RequestCtx) -> Option<String> {
        let _ = parts;
        path_value(ctx, N::NAME).map(ToOwned::to_owned)
    }
}

impl<R, S> ResourceSource<R> for Masked<S>
where
    S: ResourceSource<R>,
{
    const RESOURCE: &'static str = S::RESOURCE;
    const MASK_NOT_FOUND: bool = true;

    fn describe(op: &mut OperationBuilder) {
        S::describe(op);
    }

    fn load<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<Option<R>>> {
        S::load(parts, ctx)
    }

    fn locate(parts: &http::request::Parts, ctx: &RequestCtx) -> Option<String> {
        S::locate(parts, ctx)
    }
}

/// The path parameter [`FromPathId`] reads.
const ID_PARAM: &str = "id";

/// The captured value of one path parameter.
fn path_value<'a>(ctx: &'a RequestCtx, name: &str) -> Option<&'a str> {
    ctx.path_params()?.get(name)
}

/// Document the path parameter and the 404 a resource source implies.
fn describe_path_parameter<R: Entity>(op: &mut OperationBuilder, name: &str) {
    op.parameter(
        Param::path(name)
            .required(true)
            .schema_of::<String>()
            .description(format!("The identifier of the {}.", R::NAME)),
    );
    op.response(
        404,
        ResponseSpec::problem(format!("No {} has that identifier.", R::NAME)),
    );
}

/// Read the path parameter, parse it as the primary key, and fetch one row.
///
/// The single statement acceptance criterion 3 counts: one `SELECT`, and the
/// handler is handed the row rather than an identifier to look up again.
async fn load_by_path_key<R>(ctx: &RequestCtx, name: &str) -> moso_core::Result<Option<R>>
where
    R: Entity,
    R::Pk: FromStr,
{
    let Some(raw) = path_value(ctx, name) else {
        return Err(moso_core::Error::internal_msg(format!(
            "`Authorized<_, {}>` reads the `{{{name}}}` path parameter, and this route has \
             none\n  help: register the endpoint at a path that captures it, e.g. \
             `/{}s/{{{name}}}`\n  help: or name a different segment: \
             `moso_authz::path_name!(Key = \"…\");` then \
             `Authorized<_, {}, FromPath<Key>>`",
            R::NAME,
            R::NAME.to_lowercase(),
            R::NAME,
        )));
    };

    let Ok(key) = R::Pk::from_str(raw) else {
        // A 400 and not a 404: the caller sent something that cannot be an
        // identifier at all, and saying "not found" would hide the typo.
        return Err(moso_core::Error::bad_request(format!(
            "`{name}` is not a valid {} identifier",
            R::NAME
        ))
        .with_field(
            &format!("/path/{name}"),
            moso_schema::codes::TYPE,
            "this is not a valid identifier",
        ));
    };

    let db = ctx.provider::<Db>()?;
    Ok(Select::<R>::find(key).fetch_optional(&*db).await?)
}

// ---------------------------------------------------------------------------
// Authorized
// ---------------------------------------------------------------------------

/// A resource, loaded and authorised, ready to use.
///
/// One extractor does three things — read the identifier, load the row, run the
/// policy — and yields the row, so the handler does not query again. A
/// statement counter in the test suite asserts exactly one statement.
///
/// ```text
/// #[endpoint]
/// async fn publish(
///     post: Authorized<Publish, Post>,
///     Inject(db): Inject<Db>,
/// ) -> Result<PostOut> {
///     let post = post.into_inner();
///     let post = post.update().set(Post::PUBLISHED_AT, now()).fetch_one(&db).await?;
///     Ok(post.into())
/// }
/// ```
///
/// # Why it is not a tuple struct
///
/// `Authorized<A, R, S>` has to name `A` and `S` somewhere, and the only place
/// left is a `PhantomData` field — which would make the documented
/// `Authorized(post)` pattern impossible to write outside this crate. So the
/// resource comes out through [`into_inner`](Authorized::into_inner), and
/// `Deref` covers the read-only cases.
///
/// # The 404-before-403 trade
///
/// The resource is loaded before the policy runs, so an unauthorised caller can
/// tell an existing id from a missing one. That is the right default: the
/// alternative — a 403 for an id that does not exist — confirms which ids *do*.
/// When existence itself is the secret, [`Masked<S>`](Masked) inverts it and
/// both cases return 404.
pub struct Authorized<A, R, S = FromPathId> {
    /// The loaded row.
    resource: R,
    /// The decision that let it through, kept so a handler can read the
    /// obligations without running the policy again.
    decision: crate::Decision,
    /// The action and source, which hold no data.
    marker: PhantomData<fn() -> (A, S)>,
}

impl<A, R, S> Authorized<A, R, S> {
    /// The loaded resource.
    ///
    /// ```no_run
    /// # use moso_authz::Authorized;
    /// # fn f<A, R, S>(a: Authorized<A, R, S>) -> R { a.into_inner() }
    /// ```
    #[must_use]
    pub fn into_inner(self) -> R {
        self.resource
    }

    /// The decision that allowed this, including its obligations.
    ///
    /// ```no_run
    /// # use moso_authz::{Authorized, Decision};
    /// # fn f<A, R, S>(a: &Authorized<A, R, S>) -> &Decision { a.decision() }
    /// ```
    #[must_use]
    pub fn decision(&self) -> &crate::Decision {
        &self.decision
    }

    /// The resource and the decision, when the handler needs both.
    ///
    /// ```no_run
    /// # use moso_authz::{Authorized, Decision};
    /// # fn f<A, R, S>(a: Authorized<A, R, S>) -> (R, Decision) { a.into_parts() }
    /// ```
    #[must_use]
    pub fn into_parts(self) -> (R, crate::Decision) {
        (self.resource, self.decision)
    }

    /// Build one directly, for a test or for a hand-written extractor.
    ///
    /// ```no_run
    /// # use moso_authz::{Authorized, Decision, FromPathId};
    /// # fn f(post: u32) {
    /// # struct Publish;
    /// let _: Authorized<Publish, u32, FromPathId> =
    ///     Authorized::new(post, Decision::allow("test fixture"));
    /// # }
    /// ```
    #[must_use]
    pub fn new(resource: R, decision: crate::Decision) -> Self {
        Self {
            resource,
            decision,
            marker: PhantomData,
        }
    }

    /// Turn the resource into something else, keeping the decision.
    ///
    /// What a handler that returns a DTO calls, so the obligations survive the
    /// conversion and [`Redacted`](crate::Redacted) can honour them.
    ///
    /// ```no_run
    /// # use moso_authz::{Authorized, Redacted};
    /// # fn f<A, R, S>(a: Authorized<A, R, S>) -> Redacted<usize> {
    /// a.map(|_resource| 1usize).into_redacted()
    /// # }
    /// ```
    #[must_use]
    pub fn map<T>(self, transform: impl FnOnce(R) -> T) -> Authorized<A, T, S> {
        Authorized {
            resource: transform(self.resource),
            decision: self.decision,
            marker: PhantomData,
        }
    }

    /// The response that honours this decision's obligations.
    ///
    /// ```no_run
    /// # use moso_authz::{Authorized, Redacted};
    /// # fn f<A, R, S>(a: Authorized<A, R, S>) -> Redacted<R> { a.into_redacted() }
    /// ```
    #[must_use]
    pub fn into_redacted(self) -> crate::Redacted<R> {
        crate::Redacted::new(self.resource, self.decision)
    }
}

impl<A, R, S> core::ops::Deref for Authorized<A, R, S> {
    type Target = R;

    fn deref(&self) -> &R {
        &self.resource
    }
}

impl<A, R, S> core::fmt::Debug for Authorized<A, R, S>
where
    R: core::fmt::Debug,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Authorized")
            .field("resource", &self.resource)
            .field("decision", &self.decision)
            .finish()
    }
}

impl<A, R, S> Extract for Authorized<A, R, S>
where
    A: HasRole,
    R: Send + Sync + 'static,
    S: ResourceSource<R>,
    Actor<A::Role>: Policy<A, R>,
{
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<dyn ActorSource<A::Role>>()];

    fn describe(op: &mut OperationBuilder) {
        S::describe(op);
        describe_authenticated(op);
        op.response(
            403,
            ResponseSpec::problem(format!(
                "the `{}` policy for `{}` refused. In a development profile the body carries the \
                 policy's reason; elsewhere it does not, because a reason such as \"not the \
                 author\" tells the caller who the author is.",
                A::NAME,
                S::RESOURCE,
            )),
        );
        declare(
            op,
            AuthzDeclaration::Policy {
                action: A::NAME.to_owned(),
                resource: S::RESOURCE.to_owned(),
            },
        );
    }

    async fn extract(
        parts: &mut http::request::Parts,
        ctx: &RequestCtx,
    ) -> moso_core::Result<Self> {
        let development = ctx.state().profile() != moso_core::config::Profile::Production;
        let identifier = S::locate(parts, ctx);
        let actor = ctx.depends::<Actor<A::Role>>().await?;

        let Some(resource) = S::load(parts, ctx).await? else {
            return Err(crate::Error::not_found(S::RESOURCE).into_response(development));
        };

        let mut policy_ctx = PolicyCtx::new(actor.id().clone(), actor.scope().clone()).for_request(
            ctx.request_id().to_string(),
            crate::explain_requested(ctx.headers(), development),
            development,
        );
        // The registry is optional: an application that registered none gets an
        // explanation without the `policy` row rather than an invented one.
        if let Some(policies) = ctx.try_provider::<crate::PolicyRegistry>() {
            policy_ctx = policy_ctx.with_policies(policies);
        }

        let decision = actor.can_with(A::default(), &resource, &policy_ctx).await;

        let named = match &identifier {
            Some(id) => format!("{}#{id}", S::RESOURCE),
            None => S::RESOURCE.to_owned(),
        };
        audit(
            ctx,
            &actor.identity(),
            A::NAME,
            Some(&named),
            &decision,
            false,
        )
        .await;

        if decision.allowed() {
            return Ok(Self::new(resource, decision));
        }

        let denial = crate::Error::denied(A::NAME, named.clone(), decision.reason().to_owned());
        let denial = if S::MASK_NOT_FOUND {
            denial.masked(S::RESOURCE)
        } else {
            denial
        };
        let mut error = denial.into_response(development);
        if policy_ctx.explain() {
            // The same function `moso authz explain` calls, so the header's
            // block and the CLI's block are the same block.
            let mut explanation = actor.explain(&decision).with_requirement(
                Vec::new(),
                Some(A::NAME),
                Some(&named),
                None,
            );
            if let Some(policy) = policy_ctx.policy_for(A::NAME, S::RESOURCE) {
                explanation = explanation.by_policy(policy);
            }
            error = error.with_detail(format!("{}\n\n{}", decision.reason(), explanation.render()));
        }
        Err(error)
    }
}

// ---------------------------------------------------------------------------
// #[requires]
// ---------------------------------------------------------------------------

/// How many of a `#[requires]` list must hold.
///
/// ```
/// use moso_authz::RequireMode;
///
/// // `#[requires(a, b)]` means both, which is the safer reading of a list.
/// assert_eq!(RequireMode::default(), RequireMode::All);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RequireMode {
    /// Every listed permission. The default.
    #[default]
    All,
    /// At least one. Written `#[requires(any(a, b))]`.
    Any,
}

impl RequireMode {
    /// Whether `held` satisfies `required` under this mode.
    ///
    /// # An empty requirement refuses everybody
    ///
    /// `has_all` of nothing is vacuously true, so the mathematical reading of
    /// `Requires::new(PermSet::empty())` is "allow everybody" — which is the
    /// opposite of what anyone who wrote a requirement meant, and the opposite
    /// of the framework's deny-by-default posture. A permission set built from
    /// a filter that came back empty must not silently open the route it was
    /// meant to close, so an empty requirement is refused under **both** modes.
    /// `#[requires()]` is separately rejected by the macro; this is the runtime
    /// half, for the builder the macro does not go through.
    ///
    /// ```
    /// use moso_authz::RequireMode;
    /// # use moso_authz::{PermBits, perm::PERM_WORDS};
    /// let held = PermBits::new([0b101, 0, 0, 0], 1);
    /// let wanted = PermBits::new([0b110, 0, 0, 0], 1);
    /// let nothing = PermBits::new([0, 0, 0, 0], 1);
    ///
    /// assert!(!RequireMode::All.satisfied_by(held, wanted));
    /// assert!(RequireMode::Any.satisfied_by(held, wanted));
    ///
    /// // Deny by default: a requirement nobody stated is not a requirement
    /// // everybody meets.
    /// assert!(!RequireMode::All.satisfied_by(held, nothing));
    /// assert!(!RequireMode::Any.satisfied_by(held, nothing));
    /// ```
    #[must_use]
    pub fn satisfied_by(self, held: crate::PermBits, required: crate::PermBits) -> bool {
        if required.is_empty() {
            return false;
        }
        match self {
            Self::All => held.has_all(required),
            Self::Any => held.has_any(required),
        }
    }

    /// The word an error message uses.
    ///
    /// ```
    /// use moso_authz::RequireMode;
    ///
    /// assert_eq!(RequireMode::Any.description(), "at least one of");
    /// ```
    #[must_use]
    pub const fn description(self) -> &'static str {
        match self {
            Self::All => "all of",
            Self::Any => "at least one of",
        }
    }
}

/// The guard `#[requires(..)]` is built on.
///
/// A [`Guard`] and not a bare layer, so the permission it checks appears in the
/// OpenAPI document: the operation gets a `security` requirement and a 403
/// whose description names the permission and its description. That is the gap
/// this crate exists to close — a middleware that can 403 without the document
/// saying so makes the document wrong.
///
/// Use it directly to guard a whole router:
/// `Router::new().nest("/admin", admin).guard(Requires::new(PermSet::of([Perm::AdminAccess])))`.
/// [`Required<D>`](Required) is the per-endpoint form the attribute generates.
///
/// ```no_run
/// use moso_authz::{PermSet, Permission, Requires};
///
/// fn guard<P: Permission>(needed: PermSet<P>) -> Requires<P> {
///     Requires::new(needed)
/// }
/// ```
pub struct Requires<P: Permission> {
    /// What is needed.
    required: PermSet<P>,
    /// All of them, or any of them.
    mode: RequireMode,
    /// Whether an *allow* is audited too. Denials always are.
    audit: bool,
}

impl<P: Permission> Requires<P> {
    /// Require every permission in `required`.
    ///
    /// # An empty set refuses everybody
    ///
    /// `Requires::new(PermSet::empty())` denies every request through the
    /// subtree it guards, and says so once at `warn` when it is built. The
    /// alternative reading — an empty requirement is vacuously satisfied — turns
    /// a set built from a filter that came back empty into an open door, which
    /// is the failure this crate exists to make impossible. Use
    /// [`mark_public`] to declare a route open on purpose.
    ///
    /// ```
    /// use moso_authz::{PermSet, Permission, RequireMode, Requires};
    /// # use moso_authz::perm::fingerprint_of;
    /// # #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum Perm { Publish }
    /// # impl Permission for Perm {
    /// #     const ALL: &'static [Self] = &[Perm::Publish];
    /// #     const FINGERPRINT: u64 = fingerprint_of(&["posts.publish"]);
    /// #     fn index(self) -> u16 { 0 }
    /// #     fn from_index(i: u16) -> Option<Self> { (i == 0).then_some(Perm::Publish) }
    /// #     fn as_str(self) -> &'static str { "posts.publish" }
    /// #     fn description(self) -> &'static str { "Publish posts" }
    /// #     fn group(self) -> &'static str { "posts" }
    /// #     fn parse(n: &str) -> Option<Self> { (n == "posts.publish").then_some(Perm::Publish) }
    /// # }
    /// let guard = Requires::new(PermSet::of([Perm::Publish]));
    /// assert!(!guard.is_vacuous());
    ///
    /// let empty: Requires<Perm> = Requires::new(PermSet::empty());
    /// assert!(empty.is_vacuous(), "and it refuses rather than admits");
    /// ```
    #[must_use]
    pub fn new(required: PermSet<P>) -> Self {
        if required.is_empty() {
            // Once, where it is built, rather than once per refused request:
            // this is a wiring mistake, and a wiring mistake belongs in the boot
            // log rather than in the request log.
            tracing::warn!(
                target: "moso::authz",
                "`Requires::new` was given an empty permission set, so every request through \
                 this guard is refused\n  help: pass the permissions the routes need, or mark \
                 them open on purpose with `#[public]`"
            );
        }
        Self {
            required,
            mode: RequireMode::All,
            audit: false,
        }
    }

    /// Whether this guard states no requirement, and therefore refuses
    /// everybody.
    ///
    /// ```no_run
    /// # use moso_authz::{Permission, Requires};
    /// # fn f<P: Permission>(r: &Requires<P>) { let _: bool = r.is_vacuous(); }
    /// ```
    #[must_use]
    pub fn is_vacuous(&self) -> bool {
        self.required.is_empty()
    }

    /// Require at least one.
    ///
    /// ```no_run
    /// # use moso_authz::{PermSet, Permission, Requires};
    /// # fn f<P: Permission>(s: PermSet<P>) { let _ = Requires::new(s).any(); }
    /// ```
    #[must_use]
    pub fn any(mut self) -> Self {
        self.mode = RequireMode::Any;
        self
    }

    /// Write an audit entry even when the check passes.
    ///
    /// `#[requires(Perm::UsersSuspend, audit)]`. Denials are always audited;
    /// this is for the permissions where the *allows* are what a compliance
    /// review asks about.
    ///
    /// ```no_run
    /// # use moso_authz::{PermSet, Permission, Requires};
    /// # fn f<P: Permission>(s: PermSet<P>) { let _ = Requires::new(s).audited(); }
    /// ```
    #[must_use]
    pub fn audited(mut self) -> Self {
        self.audit = true;
        self
    }

    /// What is required.
    ///
    /// ```no_run
    /// # use moso_authz::{PermSet, Permission, Requires};
    /// # fn f<P: Permission>(r: &Requires<P>) { let _: PermSet<P> = r.required(); }
    /// ```
    #[must_use]
    pub fn required(&self) -> PermSet<P> {
        self.required
    }

    /// All of them, or any of them.
    ///
    /// ```no_run
    /// # use moso_authz::{Permission, RequireMode, Requires};
    /// # fn f<P: Permission>(r: &Requires<P>) { let _: RequireMode = r.mode(); }
    /// ```
    #[must_use]
    pub fn mode(&self) -> RequireMode {
        self.mode
    }
}

impl<P: Permission> Clone for Requires<P> {
    fn clone(&self) -> Self {
        Self {
            required: self.required,
            mode: self.mode,
            audit: self.audit,
        }
    }
}

impl<P: Permission> core::fmt::Debug for Requires<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Requires")
            .field("required", &self.required)
            .field("mode", &self.mode)
            .finish()
    }
}

impl<P: Permission> Guard for Requires<P> {
    fn describe(&self, op: &mut OperationBuilder) {
        describe_requirement::<P>(op, self.required.names(), self.mode);
    }

    fn check<'a>(
        &'a self,
        parts: &'a http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> BoxFuture<'a, moso_core::Result<()>> {
        let _ = parts;
        Box::pin(
            async move { check_permissions::<P>(ctx, self.required, self.mode, self.audit).await },
        )
    }
}

/// One `#[requires(..)]` declaration, frozen into a type.
///
/// `#[requires]` cannot put a `PermSet` in a type parameter — a `[u64; 4]` is
/// not a valid const-generic argument on stable — so it generates a unit type
/// per call site that carries the declaration as associated constants, and
/// injects `Required<ThatType>` as the handler's first parameter.
///
/// ```
/// use moso_authz::{PermSet, Permission, RequireMode, Requirement};
/// # use moso_authz::perm::fingerprint_of;
/// # #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub enum Perm { Publish }
/// # impl Permission for Perm {
/// #     const ALL: &'static [Self] = &[Perm::Publish];
/// #     const FINGERPRINT: u64 = fingerprint_of(&["posts.publish"]);
/// #     fn index(self) -> u16 { 0 }
/// #     fn from_index(i: u16) -> Option<Self> { (i == 0).then_some(Perm::Publish) }
/// #     fn as_str(self) -> &'static str { "posts.publish" }
/// #     fn description(self) -> &'static str { "Publish posts" }
/// #     fn group(self) -> &'static str { "posts" }
/// #     fn parse(n: &str) -> Option<Self> { (n == "posts.publish").then_some(Perm::Publish) }
/// # }
/// /// What `#[requires(Perm::PostsPublish)]` generates.
/// pub struct MayPublish;
///
/// impl Requirement for MayPublish {
///     type Perm = Perm;
///     const NAMES: &'static [&'static str] = &["posts.publish"];
/// }
///
/// assert_eq!(MayPublish::MODE, RequireMode::All);
/// assert_eq!(MayPublish::resolve().0, PermSet::of([Perm::Publish]));
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a `#[requires]` declaration",
    label = "not a requirement",
    note = "`Required<D>` is generated by `#[requires(..)]`; it is not written by hand",
    note = "help: write `#[requires(Perm::PostsPublish)]` *above* `#[endpoint]` on the handler",
    note = "help: to guard a whole router instead, use the runtime form: \
            `Router::guard(Requires::new(PermSet::of([Perm::PostsPublish])))`"
)]
pub trait Requirement: Send + Sync + 'static {
    /// The permission registry the names belong to.
    type Perm: Permission;

    /// The permission names, exactly as the call site wrote them.
    ///
    /// Names and not values, because `#[requires("posts.publish")]` is also
    /// legal and a proc macro cannot turn a string into an enum variant. The
    /// enum form emits `&[Perm::PostsPublish.as_str()]`, so both arrive here in
    /// the same shape and both are checked against the registry at boot.
    const NAMES: &'static [&'static str];

    /// All of them, or any of them.
    const MODE: RequireMode = RequireMode::All;

    /// Whether an *allow* is audited too. Denials always are.
    const AUDIT: bool = false;

    /// The resolved set, and any name the registry does not know.
    ///
    /// An unknown name is a boot error, not a silent `false`: a `#[requires]`
    /// nobody can satisfy is indistinguishable from an endpoint that is simply
    /// broken, and that is the failure this crate exists to make impossible.
    #[must_use]
    fn resolve() -> (PermSet<Self::Perm>, Vec<&'static str>) {
        let mut set = PermSet::empty();
        let mut unknown = Vec::new();
        for name in Self::NAMES {
            match Self::Perm::parse(name) {
                Some(permission) => set = set.with(permission),
                None => unknown.push(*name),
            }
        }
        (set, unknown)
    }
}

/// The extractor `#[requires(..)]` injects.
///
/// It runs before the handler body, in parameter order, and a failure
/// short-circuits with a 403 — which is what "checked before the handler body
/// runs" means. Being an [`Extract`] rather than a layer is what puts the
/// permission in the OpenAPI document.
///
/// Never written by hand; [`Requires`] is the hand-written form.
///
/// ```text
/// #[requires(Perm::PostsCreate)]
/// #[endpoint]
/// async fn create(Json(body): Json<CreatePost>) -> Result<Created<PostOut>> { … }
///
/// // expands to, in part:
/// #[endpoint]
/// async fn create(
///     _: ::moso::__private::Required<__moso_authz_create>,
///     Json(body): Json<CreatePost>,
/// ) -> Result<Created<PostOut>> { … }
/// ```
pub struct Required<D: Requirement>(PhantomData<fn() -> D>);

impl<D: Requirement> Required<D> {
    /// The permissions this declaration requires.
    ///
    /// ```no_run
    /// # use moso_authz::{PermSet, Required, Requirement};
    /// # fn f<D: Requirement>() { let _: PermSet<D::Perm> = Required::<D>::permissions(); }
    /// ```
    #[must_use]
    pub fn permissions() -> PermSet<D::Perm> {
        D::resolve().0
    }
}

impl<D: Requirement> core::fmt::Debug for Required<D> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Required")
            .field("names", &D::NAMES)
            .field("mode", &D::MODE)
            .finish()
    }
}

impl<D: Requirement> Extract for Required<D> {
    const PROVIDER_REQ: &'static [ProviderReq] =
        &[ProviderReq::of::<dyn crate::PermissionSource>()];

    fn describe(op: &mut OperationBuilder) {
        describe_requirement::<D::Perm>(op, D::NAMES.to_vec(), D::MODE);
    }

    fn extract<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> impl Future<Output = moso_core::Result<Self>> + Send + 'a {
        let _ = parts;
        async move {
            let (required, unknown) = D::resolve();
            if !unknown.is_empty() {
                // Fail closed. This should have been caught at boot by
                // `boot_problems`; refusing is the only safe reading of a
                // requirement nobody can satisfy.
                return Err(moso_core::Error::internal_msg(format!(
                    "`#[requires]` names {} permission(s) this application does not \
                     declare: {}\n  help: run the boot check — a `permissions!` registry and a \
                     `#[requires]` that disagree is a boot error, not a runtime one",
                    unknown.len(),
                    unknown.join(", "),
                )));
            }
            check_permissions::<D::Perm>(ctx, required, D::MODE, D::AUDIT).await?;
            Ok(Self(PhantomData))
        }
    }
}

/// Document a permission requirement: the security scheme, the 403, and the
/// declaration `moso check --authz` reads back.
fn describe_requirement<P: Permission>(
    op: &mut OperationBuilder,
    names: Vec<&'static str>,
    mode: RequireMode,
) {
    let registry = PermissionRegistry::of::<P>();
    describe_authenticated(op);

    let listed = names
        .iter()
        .map(|name| match registry.lookup(name) {
            Some(entry) => format!("`{}` ({})", entry.name(), entry.description()),
            None => format!("`{name}` — **not declared by `permissions!`**"),
        })
        .collect::<Vec<_>>()
        .join(", ");
    // An empty requirement refuses everybody, so the document says that rather
    // than "does not hold all of " with nothing after it. A document that reads
    // as though the route is reachable is worse than one that admits it is not.
    let forbidden = if names.is_empty() {
        "this operation states an empty permission requirement, which refuses every caller. \
         That is deny-by-default doing its job: give the guard the permissions the route needs, \
         or mark the route `#[public]`."
            .to_owned()
    } else {
        format!("the caller does not hold {} {listed}.", mode.description(),)
    };
    op.response(403, ResponseSpec::problem(forbidden));

    let unknown: Vec<crate::Error> = registry.check(&names).err().unwrap_or_default();
    if unknown.is_empty() {
        declare(
            op,
            AuthzDeclaration::Permissions {
                names: names.iter().map(|name| (*name).to_owned()).collect(),
                all: mode == RequireMode::All,
            },
        );
    } else {
        declare(
            op,
            AuthzDeclaration::Unknown {
                names: unknown
                    .iter()
                    .map(|error| match error {
                        crate::Error::UnknownPermission { name, suggestion } => {
                            (name.clone(), suggestion.clone())
                        }
                        other => (other.to_string(), None),
                    })
                    .collect(),
            },
        );
    }
}

/// Read the caller's permission bits and compare them against a requirement.
///
/// Shared by [`Requires`] (the router guard) and [`Required`] (the per-endpoint
/// extractor), so the two cannot disagree about what `#[requires]` means.
async fn check_permissions<P: Permission>(
    ctx: &RequestCtx,
    required: PermSet<P>,
    mode: RequireMode,
    audit_allows: bool,
) -> moso_core::Result<()> {
    let development = ctx.state().profile() != moso_core::config::Profile::Production;
    let source = ctx.provider::<dyn crate::PermissionSource>()?;

    if source.fingerprint() != P::FINGERPRINT {
        // Two registries in one process: every bit would mean something else,
        // and comparing them would produce a confident wrong answer.
        return Err(moso_core::Error::internal_msg(
            "the registered `PermissionSource` was built against a different `permissions!` \
             registry from the one this endpoint requires\n  help: an application has exactly \
             one permission registry; check that `ActorPermissions::<Role>` and the `Perm` in \
             `#[requires]` come from the same crate",
        ));
    }

    let held = source.permissions(ctx).await?;
    let allowed = mode.satisfied_by(held, required.to_bits());

    let missing = PermSet::<P>::from_bits(held)
        .map(|held| required.difference(held).names().join(", "))
        .unwrap_or_default();
    let reason: std::borrow::Cow<'static, str> = if allowed {
        std::borrow::Cow::Borrowed("holds the required permissions")
    } else if required.is_empty() {
        std::borrow::Cow::Borrowed("the requirement is empty, and an empty requirement refuses")
    } else {
        std::borrow::Cow::Owned(format!("missing {} {missing}", mode.description()))
    };
    let action = required.names().join(", ");

    let identity = actor_identity(ctx);
    let decision = if allowed {
        crate::Decision::allow(reason.clone())
    } else {
        crate::Decision::deny(reason.clone())
    };
    audit(ctx, &identity, &action, None, &decision, audit_allows).await;

    if allowed {
        return Ok(());
    }
    Err(crate::Error::denied("required permissions", action, reason).into_response(development))
}

/// Who is acting, for an audit record, without naming the role type.
///
/// The permission path is deliberately erased — `#[requires]` names the
/// permission enum and not the role enum — so the identity comes from whatever
/// put one in the request extensions: [`actor_layer`](crate::actor_layer), or
/// an application's own middleware. An anonymous placeholder is the honest
/// answer when nothing did.
///
/// A bare [`ActorId`](crate::ActorId) is still accepted, because that is what
/// the documentation told applications to insert before
/// [`ActorIdentity`](crate::ActorIdentity) existed. It carries no kind and no
/// scope, so those read as a user acting globally.
fn actor_identity(ctx: &RequestCtx) -> crate::ActorIdentity {
    if let Some(identity) = ctx.extension::<crate::ActorIdentity>() {
        return identity;
    }
    match ctx.extension::<crate::ActorId>() {
        Some(id) => crate::ActorIdentity::new(id, crate::ActorKind::User, crate::Scope::Global),
        None => crate::ActorIdentity::anonymous(),
    }
}

/// Write an audit entry, honouring the configured policy.
///
/// The sink and the configuration are optional providers: an application that
/// registers neither still gets a trail, on the `moso::authz::audit` tracing
/// target, because an authorization layer whose audit is opt-in is an
/// authorization layer with no audit.
async fn audit(
    ctx: &RequestCtx,
    identity: &crate::ActorIdentity,
    action: &str,
    resource: Option<&str>,
    decision: &crate::Decision,
    forced: bool,
) {
    let config = ctx
        .try_provider::<AuditConfig>()
        .map_or_else(AuditConfig::default, |config| *config);
    if decision.allowed() && !(config.allows || forced) {
        return;
    }
    if !decision.allowed() && !config.denies {
        return;
    }

    let (id, kind, scope) = identity.clone().into_parts();
    let mut entry = if decision.allowed() {
        AuditRecord::allow(id, kind, scope, action, decision.reason().to_owned())
    } else {
        AuditRecord::deny(id, kind, scope, action, decision.reason().to_owned())
    };
    if let Some((name, key)) = resource.and_then(|full| full.split_once('#')) {
        entry = entry.with_resource(name, key);
    } else if let Some(name) = resource {
        entry = entry.with_resource(name, "");
    }
    entry = entry.with_request(
        &ctx.request_id().to_string(),
        ctx.matched_path(),
        client_ip(ctx).as_deref(),
    );

    match ctx.try_provider::<dyn AuditSink>() {
        Some(sink) => sink.record(entry).await,
        None => TracingAuditSink::new().record(entry).await,
    }
}

/// The caller's address, as the trusted-proxy configuration resolved it.
///
/// Read from the request extensions rather than from a header directly: a
/// `X-Forwarded-For` an untrusted client can set is not an address, and this
/// crate is not where that decision belongs.
fn client_ip(ctx: &RequestCtx) -> Option<String> {
    ctx.extension::<std::net::IpAddr>().map(|ip| ip.to_string())
}

// ---------------------------------------------------------------------------
// Deny by default
// ---------------------------------------------------------------------------

/// What an endpoint declared about its authorization, for `moso check --authz`.
///
/// Deny-by-default is only provable if "nothing declared" is distinguishable
/// from "declared public". This is that distinction, and it lives in the
/// OpenAPI document under [`AUTHZ_EXTENSION`] so the check needs no separate
/// index.
///
/// ```
/// use moso_authz::AuthzDeclaration;
///
/// assert!(AuthzDeclaration::Public.is_declared());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum AuthzDeclaration {
    /// `#[public]`: considered, and deliberately open.
    Public,
    /// `#[requires(..)]`: these permissions, in this mode.
    Permissions {
        /// The wire names.
        names: Vec<String>,
        /// Whether all or any are needed.
        all: bool,
    },
    /// An `Authorized<A, R>` parameter: this action on this resource.
    Policy {
        /// The action's name.
        action: String,
        /// The resource's name.
        resource: String,
    },
    /// `#[requires(..)]` naming something `permissions!` does not declare.
    ///
    /// Written by `describe`, which runs at boot, so
    /// [`boot_problems`] can turn it into a boot error
    /// with a "did you mean" rather than a request that can never succeed.
    Unknown {
        /// Each unknown name and the closest registered one, when close enough.
        names: Vec<(String, Option<String>)>,
    },
}

impl AuthzDeclaration {
    /// Whether anything was declared at all. Always true — the absence of a
    /// declaration is the absence of this value.
    ///
    /// ```
    /// use moso_authz::AuthzDeclaration;
    ///
    /// assert!(AuthzDeclaration::Public.is_declared());
    /// ```
    #[must_use]
    pub fn is_declared(&self) -> bool {
        true
    }

    /// Whether this declaration is a problem rather than a statement.
    ///
    /// ```
    /// use moso_authz::AuthzDeclaration;
    ///
    /// assert!(!AuthzDeclaration::Public.is_problem());
    /// ```
    #[must_use]
    pub fn is_problem(&self) -> bool {
        matches!(self, Self::Unknown { .. })
    }
}

/// The OpenAPI extension key an authorization declaration is written under.
///
/// ```
/// assert_eq!(moso_authz::AUTHZ_EXTENSION, "x-moso-authz");
/// ```
pub const AUTHZ_EXTENSION: &str = "x-moso-authz";

/// The OpenAPI extension key an operation's source location is written under.
///
/// [`undeclared_operations`] reports it as the third element of each tuple,
/// because a finding nobody can navigate to is a finding nobody fixes.
///
/// ```
/// assert_eq!(moso_authz::SOURCE_EXTENSION, "x-moso-source");
/// ```
pub const SOURCE_EXTENSION: &str = "x-moso-source";

/// Record where an operation is written, as `file:line`.
///
/// The first writer wins, so an operation whose source `#[endpoint]` already
/// recorded keeps that one and a hand-written call cannot overwrite it with
/// something less precise.
///
/// # What this does not do
///
/// It does not find the location for you. `#[endpoint]` is the only thing that
/// knows where a handler is written, and it does not write this extension yet
/// — which is why the third element of an [`undeclared_operations`] tuple is
/// `None` for a handler nobody located. Until it does, [`source!`](crate::source!)
/// captures the call site and this writes it:
///
/// ```
/// use moso_authz::{mark_source, source, source_of};
/// use moso_openapi::OperationBuilder;
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// mark_source(&mut op, source!());
///
/// assert!(source_of(op.spec()).expect("a location").contains(".rs:"));
/// ```
pub fn mark_source(op: &mut OperationBuilder, location: &str) {
    let spec = op.spec_mut();
    spec.extensions
        .entry(SOURCE_EXTENSION.to_owned())
        .or_insert_with(|| serde_json::Value::String(location.to_owned()));
}

/// This call site, as `file:line`.
///
/// What [`mark_source`] wants. A macro rather than a function because
/// `file!()`/`line!()` have to expand where the reader is, not where the
/// helper is written.
///
/// ```
/// let here = moso_authz::source!();
/// assert!(here.contains(':'), "{here}");
/// ```
#[macro_export]
macro_rules! source {
    () => {
        ::core::concat!(::core::file!(), ":", ::core::line!())
    };
}

/// Where an operation was recorded as being written, if anywhere.
///
/// The builder-side reader; [`undeclared_operations`] does the same for an
/// assembled document.
///
/// ```
/// use moso_authz::source_of;
/// use moso_openapi::OperationBuilder;
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let op = OperationBuilder::new(SchemaGenerator::default());
/// assert!(source_of(op.spec()).is_none());
/// ```
#[must_use]
pub fn source_of(spec: &moso_openapi::OperationSpec) -> Option<String> {
    read_source(spec.extensions.get(SOURCE_EXTENSION))
}

/// Where an assembled document's operation was recorded as being written.
///
/// ```
/// use moso_authz::source_at;
/// use moso_openapi::path::Operation;
///
/// assert!(source_at(&Operation::default()).is_none());
/// ```
#[must_use]
pub fn source_at(operation: &moso_openapi::path::Operation) -> Option<String> {
    read_source(operation.extensions.get(SOURCE_EXTENSION))
}

/// Decode whatever is stored under [`SOURCE_EXTENSION`].
fn read_source(value: Option<&serde_json::Value>) -> Option<String> {
    value?.as_str().map(ToOwned::to_owned)
}

/// Write a declaration into the operation.
///
/// One operation can carry several: a handler with `#[requires]` *and* an
/// `Authorized<..>` parameter declares both, and `moso check --authz` should
/// see both. `merge_extension` keeps the first writer, so the value is an
/// array and each contributor appends to it.
fn declare(op: &mut OperationBuilder, declaration: AuthzDeclaration) {
    let value = serde_json::to_value(&declaration)
        .unwrap_or_else(|_| serde_json::Value::String(format!("{declaration:?}")));
    let spec = op.spec_mut();
    match spec.extensions.get_mut(AUTHZ_EXTENSION) {
        Some(serde_json::Value::Array(existing)) => {
            if !existing.contains(&value) {
                existing.push(value);
            }
        }
        _ => {
            spec.extensions.insert(
                AUTHZ_EXTENSION.to_owned(),
                serde_json::Value::Array(vec![value]),
            );
        }
    }
}

/// Record that an endpoint is deliberately public.
///
/// What `#[public]` expands to. Without it an endpoint with no `#[requires]`
/// and no `Authorized<..>` parameter is reported by `moso check --authz`, and
/// with `lints.missing_authz = "deny"` in `moso.toml` it fails the build.
///
/// ```
/// use moso_authz::{mark_public, read_declarations, AuthzDeclaration};
/// use moso_openapi::OperationBuilder;
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// mark_public(&mut op);
///
/// assert_eq!(read_declarations(op.spec()), vec![AuthzDeclaration::Public]);
/// ```
pub fn mark_public(op: &mut OperationBuilder) {
    // An explicitly public operation also overrides any document-level security
    // requirement, so the document says "no credentials needed" rather than
    // inheriting one it does not enforce.
    op.public();
    declare(op, AuthzDeclaration::Public);
}

/// The extractor `#[public]` injects.
///
/// A zero-cost parameter whose only job is to run [`mark_public`] at boot.
/// `#[public]` uses the same mechanism as `#[requires]` so that the two read
/// the same way and neither depends on attribute ordering beyond being written
/// above `#[endpoint]`.
///
/// ```text
/// #[public]
/// #[endpoint]
/// async fn health() -> Result<NoContent> { Ok(NoContent) }
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct Public;

impl Extract for Public {
    const PROVIDER_REQ: &'static [ProviderReq] = &[];

    fn describe(op: &mut OperationBuilder) {
        mark_public(op);
    }

    fn extract<'a>(
        parts: &'a mut http::request::Parts,
        ctx: &'a RequestCtx,
    ) -> impl Future<Output = moso_core::Result<Self>> + Send + 'a {
        let _ = (parts, ctx);
        async { Ok(Self) }
    }
}

/// Read an endpoint's authorization declarations back out of an operation.
///
/// What `moso check --authz` calls, once per operation in the assembled
/// document. An empty list means nothing was declared, which is the finding.
///
/// ```
/// use moso_authz::read_declarations;
/// use moso_openapi::OperationBuilder;
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let op = OperationBuilder::new(SchemaGenerator::default());
/// assert!(read_declarations(op.spec()).is_empty());
/// ```
#[must_use]
pub fn read_declarations(spec: &moso_openapi::OperationSpec) -> Vec<AuthzDeclaration> {
    parse_declarations(spec.extensions.get(AUTHZ_EXTENSION))
}

/// The first declaration an operation carries, if any.
///
/// The single-valued form, kept because most callers only want to know whether
/// an operation declared *anything*.
///
/// ```
/// use moso_authz::read_declaration;
/// use moso_openapi::OperationBuilder;
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let op = OperationBuilder::new(SchemaGenerator::default());
/// assert!(read_declaration(op.spec()).is_none());
/// ```
#[must_use]
pub fn read_declaration(spec: &moso_openapi::OperationSpec) -> Option<AuthzDeclaration> {
    read_declarations(spec).into_iter().next()
}

/// The declarations an assembled `Operation` carries.
///
/// The document-level twin of [`read_declarations`], which takes the *builder's*
/// spec. `moso check --authz` reads the assembled document, because that is the
/// artefact that exists after boot.
///
/// ```
/// use moso_authz::declarations_of;
/// use moso_openapi::path::Operation;
///
/// assert!(declarations_of(&Operation::default()).is_empty());
/// ```
#[must_use]
pub fn declarations_of(operation: &moso_openapi::path::Operation) -> Vec<AuthzDeclaration> {
    parse_declarations(operation.extensions.get(AUTHZ_EXTENSION))
}

/// Every operation in `document` that declared no authorization at all.
///
/// Acceptance criterion 2, as one call: `moso check --authz` prints this list
/// and `lints.missing_authz = "deny"` turns a non-empty one into a build
/// failure. `#[public]` is what takes an operation off it.
///
/// Returned as `(method, path, source)` — the source being the `file:line`
/// under [`SOURCE_EXTENSION`], when something wrote one — because a finding
/// nobody can navigate to is a finding nobody fixes. See [`mark_source`] for
/// who writes it and why it is `None` more often than it should be.
///
/// ```
/// use moso_authz::undeclared_operations;
/// use moso_openapi::document::Document;
///
/// assert!(undeclared_operations(&Document::default()).is_empty());
/// ```
#[must_use]
pub fn undeclared_operations(
    document: &moso_openapi::document::Document,
) -> Vec<(&'static str, String, Option<String>)> {
    document
        .paths
        .iter()
        .flat_map(|(path, item)| {
            item.operations()
                .filter(|(_, operation)| declarations_of(operation).is_empty())
                .map(move |(method, operation)| {
                    (method.as_upper_str(), path.clone(), source_at(operation))
                })
        })
        .collect()
}

/// Every boot problem the operations in `document` carry.
///
/// The document-level twin of [`boot_problems`]. What `App::build()` should
/// call, so `#[requires("posts.pubish")]` is a boot error with a "did you mean"
/// rather than a request that can never succeed.
///
/// ```
/// use moso_authz::document_problems;
/// use moso_openapi::document::Document;
///
/// assert!(document_problems(&Document::default()).is_empty());
/// ```
#[must_use]
pub fn document_problems(
    document: &moso_openapi::document::Document,
) -> Vec<(String, crate::Error)> {
    document
        .paths
        .iter()
        .flat_map(|(path, item)| {
            item.operations().flat_map(move |(method, operation)| {
                let location = format!("{} {path}", method.as_upper_str());
                problems_in(declarations_of(operation))
                    .into_iter()
                    .map(move |error| (location.clone(), error))
            })
        })
        .collect()
}

/// The problems a list of declarations carries.
fn problems_in(declarations: Vec<AuthzDeclaration>) -> Vec<crate::Error> {
    declarations
        .into_iter()
        .flat_map(|declaration| match declaration {
            AuthzDeclaration::Unknown { names } => names
                .into_iter()
                .map(|(name, suggestion)| crate::Error::UnknownPermission { name, suggestion })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        })
        .collect()
}

/// Decode whatever is stored under [`AUTHZ_EXTENSION`].
fn parse_declarations(value: Option<&serde_json::Value>) -> Vec<AuthzDeclaration> {
    match value {
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect(),
        Some(other) => serde_json::from_value(other.clone())
            .ok()
            .into_iter()
            .collect(),
        None => Vec::new(),
    }
}

/// Every boot problem an operation's declarations carry.
///
/// Acceptance criterion 1: `#[requires("posts.pubish")]` is a **boot** error
/// with a suggestion, not a request that silently never succeeds. `describe`
/// runs during `App::build()`, which is where the registry is known, so the
/// problem is found there and reported here.
///
/// ```
/// use moso_authz::{boot_problems, AuthzDeclaration, AUTHZ_EXTENSION};
/// use moso_openapi::OperationBuilder;
/// use moso_schema::json_schema::SchemaGenerator;
///
/// let mut op = OperationBuilder::new(SchemaGenerator::default());
/// let declaration = AuthzDeclaration::Unknown {
///     names: vec![("posts.pubish".to_owned(), Some("posts.publish".to_owned()))],
/// };
/// op.extension(
///     AUTHZ_EXTENSION,
///     serde_json::json!([serde_json::to_value(&declaration).unwrap()]),
/// );
///
/// let problems = boot_problems(op.spec());
/// assert_eq!(problems.len(), 1);
/// assert!(problems[0].to_string().contains("did you mean `posts.publish`"));
/// ```
#[must_use]
pub fn boot_problems(spec: &moso_openapi::OperationSpec) -> Vec<crate::Error> {
    problems_in(read_declarations(spec))
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso_schema::json_schema::SchemaGenerator;

    use crate::fixture::{Perm, Post, PostId, Publish, Read, Role};
    use crate::{Actor, Decision, PermBits};

    fn builder() -> OperationBuilder {
        OperationBuilder::new(SchemaGenerator::default())
    }

    /// A documented response's description, which is optional in the model and
    /// never optional in anything this crate writes.
    fn description_of(spec: &moso_openapi::OperationSpec, status: &str) -> String {
        spec.responses[status]
            .description
            .clone()
            .expect("every response this crate documents carries a description")
    }

    /// What `#[requires(Perm::PostsPublish)]` generates.
    struct MayPublish;

    impl Requirement for MayPublish {
        type Perm = Perm;
        const NAMES: &'static [&'static str] = &[Perm::PostsPublish.as_str()];
    }

    /// What `#[requires(any(Perm::PostsRead, Perm::AdminAccess), audit)]` generates.
    struct MayReadOrAdminister;

    impl Requirement for MayReadOrAdminister {
        type Perm = Perm;
        const NAMES: &'static [&'static str] =
            &[Perm::PostsRead.as_str(), Perm::AdminAccess.as_str()];
        const MODE: RequireMode = RequireMode::Any;
        const AUDIT: bool = true;
    }

    /// What `#[requires("posts.pubish")]` generates — the typo the boot check
    /// exists to catch.
    struct Mistyped;

    impl Requirement for Mistyped {
        type Perm = Perm;
        const NAMES: &'static [&'static str] = &["posts.pubish"];
    }

    // ── the requirement itself ────────────────────────────────────────────

    #[test]
    fn a_requirement_resolves_its_names_into_a_set() {
        let (set, unknown) = MayPublish::resolve();

        assert_eq!(set, PermSet::of([Perm::PostsPublish]));
        assert!(unknown.is_empty());
        assert_eq!(MayPublish::MODE, RequireMode::All);
        const { assert!(!MayPublish::AUDIT) };
        assert_eq!(Required::<MayPublish>::permissions(), set);
    }

    #[test]
    fn a_mistyped_requirement_reports_the_name_rather_than_silently_missing() {
        let (set, unknown) = Mistyped::resolve();

        assert!(set.is_empty());
        assert_eq!(unknown, vec!["posts.pubish"]);
    }

    #[test]
    fn require_mode_reads_the_way_the_attribute_does() {
        let held = PermSet::of([Perm::PostsRead]).to_bits();
        let both = PermSet::of([Perm::PostsRead, Perm::AdminAccess]).to_bits();
        let neither = PermSet::<Perm>::of([Perm::AdminAccess]).to_bits();
        let nothing = PermSet::<Perm>::empty().to_bits();

        assert!(RequireMode::All.satisfied_by(held, held));
        assert!(!RequireMode::All.satisfied_by(held, both));
        assert!(RequireMode::Any.satisfied_by(held, both));
        assert!(!RequireMode::Any.satisfied_by(held, neither));

        assert_eq!(RequireMode::All.description(), "all of");
        assert_eq!(RequireMode::Any.description(), "at least one of");
        assert_eq!(RequireMode::default(), RequireMode::All);

        let _ = nothing;
    }

    /// The mathematical reading of `has_all` over an empty set is "everybody
    /// satisfies it", which turns `Requires::new(PermSet::empty())` into an
    /// open door. Deny-by-default is the framework's posture, so an empty
    /// requirement refuses — under both modes, and for the actor who holds
    /// every permission as much as for the one who holds none.
    #[test]
    fn an_empty_requirement_refuses_everybody_rather_than_admitting_them() {
        let nothing = PermSet::<Perm>::empty().to_bits();
        let everything = PermSet::<Perm>::all().to_bits();

        for mode in [RequireMode::All, RequireMode::Any] {
            assert!(
                !mode.satisfied_by(nothing, nothing),
                "{mode:?}: an empty requirement is not vacuously satisfied",
            );
            assert!(
                !mode.satisfied_by(everything, nothing),
                "{mode:?}: not even for an actor holding every permission",
            );
        }
    }

    /// The guard says so at construction, because an empty set is a wiring
    /// mistake and a wiring mistake belongs in the boot log.
    #[test]
    fn a_guard_with_no_permissions_reports_itself_as_vacuous() {
        let stated = Requires::new(PermSet::of([Perm::PostsPublish]));
        assert!(!stated.is_vacuous());

        let empty: Requires<Perm> = Requires::new(PermSet::empty());
        assert!(empty.is_vacuous());
        assert!(empty.required().is_empty());
    }

    /// A document that reads as though the route is reachable is worse than one
    /// that admits it is not.
    #[test]
    fn an_empty_requirement_documents_that_it_refuses_everybody() {
        let guard: Requires<Perm> = Requires::new(PermSet::empty());
        let mut op = builder();
        Guard::describe(&guard, &mut op);
        let spec = op.into_spec();

        let forbidden = description_of(&spec, "403");
        assert!(
            forbidden.contains("empty permission requirement"),
            "{forbidden}"
        );
        assert!(forbidden.contains("#[public]"), "{forbidden}");
        assert!(
            !forbidden.contains("does not hold all of ."),
            "the empty list must not be rendered as prose with nothing in it: {forbidden}",
        );
    }

    #[test]
    fn erased_bits_are_what_the_check_compares() {
        let bits: PermBits = PermSet::of([Perm::PostsRead]).to_bits();
        assert_eq!(bits.fingerprint(), <Perm as Permission>::FINGERPRINT);
    }

    // ── what the document says ────────────────────────────────────────────

    #[test]
    fn a_requirement_documents_the_403_with_each_permission_and_its_description() {
        let mut op = builder();
        <Required<MayPublish> as Extract>::describe(&mut op);
        let spec = op.into_spec();

        let forbidden = spec.responses.get("403").expect("a documented 403");
        let description = forbidden.description.clone().unwrap_or_default();
        assert!(description.contains("posts.publish"), "{description}");
        assert!(description.contains("Publish posts"), "{description}");
        assert!(description.contains("all of"), "{description}");
        assert!(spec.has_response("401"));
        assert_eq!(spec.security.as_ref().map(Vec::len), Some(1));
    }

    #[test]
    fn the_any_mode_says_so_in_the_document() {
        let mut op = builder();
        <Required<MayReadOrAdminister> as Extract>::describe(&mut op);
        let spec = op.into_spec();

        let description = spec.responses["403"]
            .description
            .clone()
            .unwrap_or_default();
        assert!(description.contains("at least one of"), "{description}");
    }

    #[test]
    fn a_requirement_declares_itself_for_the_deny_by_default_check() {
        let mut op = builder();
        <Required<MayPublish> as Extract>::describe(&mut op);

        assert_eq!(
            read_declarations(op.spec()),
            vec![AuthzDeclaration::Permissions {
                names: vec!["posts.publish".to_owned()],
                all: true,
            }],
        );
        assert!(boot_problems(op.spec()).is_empty());
    }

    /// Acceptance criterion 1: an unknown permission in `#[requires]` is a boot
    /// error with a suggestion. `describe` runs during `App::build()`, which is
    /// the only place the registry is known, so the problem is found there.
    #[test]
    fn a_mistyped_permission_becomes_a_boot_error_with_a_suggestion() {
        let mut op = builder();
        <Required<Mistyped> as Extract>::describe(&mut op);

        let declarations = read_declarations(op.spec());
        assert!(declarations[0].is_problem());

        let problems = boot_problems(op.spec());
        assert_eq!(problems.len(), 1);
        assert_eq!(
            problems[0].to_string(),
            "unknown permission `posts.pubish` — did you mean `posts.publish`?",
        );
    }

    #[test]
    fn a_requirement_declares_the_permission_source_it_needs() {
        let required = <Required<MayPublish> as Extract>::PROVIDER_REQ;

        assert_eq!(required.len(), 1);
        assert!(
            required[0].name().contains("PermissionSource"),
            "{}",
            required[0].name(),
        );
    }

    // ── the guard form ────────────────────────────────────────────────────

    #[test]
    fn the_guard_documents_the_same_contract_the_extractor_does() {
        let guard = Requires::new(PermSet::of([Perm::PostsPublish])).audited();
        let mut op = builder();
        Guard::describe(&guard, &mut op);
        let spec = op.into_spec();

        assert!(description_of(&spec, "403").contains("posts.publish"));
        assert_eq!(guard.required(), PermSet::of([Perm::PostsPublish]));
        assert_eq!(guard.mode(), RequireMode::All);
        assert_eq!(
            Requires::new(guard.required()).any().mode(),
            RequireMode::Any
        );
        assert!(format!("{guard:?}").contains("posts.publish"));
        assert_eq!(guard.clone().required(), guard.required());
    }

    // ── Authorized ────────────────────────────────────────────────────────

    #[test]
    fn the_extractor_documents_the_path_parameter_the_404_and_the_403() {
        let mut op = builder();
        <Authorized<Publish, Post> as Extract>::describe(&mut op);
        let spec = op.into_spec();

        assert_eq!(spec.parameters.len(), 1);
        assert_eq!(spec.parameters[0].name, "id");
        assert!(spec.parameters[0].required);
        assert!(description_of(&spec, "404").contains("Post"));
        assert!(description_of(&spec, "403").contains("publish"));
        assert!(spec.has_response("401"));
    }

    #[test]
    fn a_named_path_parameter_is_documented_under_its_own_name() {
        let mut op = builder();
        <Authorized<Publish, Post, FromPath<PostId>> as Extract>::describe(&mut op);

        assert_eq!(op.spec().parameters[0].name, "post_id");
    }

    #[test]
    fn the_extractor_declares_the_policy_it_runs() {
        let mut op = builder();
        <Authorized<Publish, Post> as Extract>::describe(&mut op);

        assert_eq!(
            read_declarations(op.spec()),
            vec![AuthzDeclaration::Policy {
                action: "publish".to_owned(),
                resource: "Post".to_owned(),
            }],
        );
    }

    #[test]
    fn the_extractor_declares_the_actor_source_it_needs() {
        let required = <Authorized<Publish, Post> as Extract>::PROVIDER_REQ;

        assert_eq!(required.len(), 1);
        assert!(
            required[0].name().contains("ActorSource"),
            "{}",
            required[0].name(),
        );
    }

    /// The masked form inverts 404-before-403 at the *type* level, so the
    /// choice is visible in the signature that documents the endpoint.
    #[test]
    fn masking_is_a_type_level_switch_that_delegates_everything_else() {
        const { assert!(!<FromPathId as ResourceSource<Post>>::MASK_NOT_FOUND) };
        const { assert!(<Masked<FromPathId> as ResourceSource<Post>>::MASK_NOT_FOUND) };
        assert_eq!(
            <Masked<FromPathId> as ResourceSource<Post>>::RESOURCE,
            <FromPathId as ResourceSource<Post>>::RESOURCE,
        );

        let mut plain = builder();
        <FromPathId as ResourceSource<Post>>::describe(&mut plain);
        let mut masked = builder();
        <Masked<FromPathId> as ResourceSource<Post>>::describe(&mut masked);

        assert_eq!(plain.into_spec().parameters, masked.into_spec().parameters);
    }

    #[test]
    fn a_masked_denial_becomes_a_missing_resource() {
        let denial = crate::Error::denied("read", "Invoice#1", "not yours");
        assert!(denial.is_denied());

        let masked = crate::Error::denied("read", "Invoice#1", "not yours").masked("Invoice");
        assert!(!masked.is_denied());
        assert_eq!(masked.to_string(), "no Invoice with that identifier");
    }

    #[test]
    fn a_loaded_resource_comes_out_with_its_decision() {
        let post = Post {
            id: 1,
            author_id: "usr_1".to_owned(),
            published: true,
            title: "Hello".to_owned(),
        };
        let authorized: Authorized<Publish, Post> =
            Authorized::new(post, Decision::allow("author"));

        assert_eq!(authorized.title, "Hello", "Deref reaches the resource");
        assert_eq!(authorized.decision().reason(), "author");
        assert!(format!("{authorized:?}").contains("Hello"));

        let mapped = authorized.map(|post| post.id);
        assert_eq!(mapped.into_inner(), 1);
    }

    #[test]
    fn into_parts_hands_back_both_halves() {
        let post = Post {
            id: 7,
            author_id: "usr_1".to_owned(),
            published: false,
            title: "Draft".to_owned(),
        };
        let (resource, decision): (Post, Decision) =
            Authorized::<Publish, Post>::new(post, Decision::allow("author")).into_parts();

        assert_eq!(resource.id, 7);
        assert!(decision.allowed());
    }

    // ── deny by default ───────────────────────────────────────────────────

    /// Acceptance criterion 2: an endpoint with nothing declared is the
    /// finding, and `#[public]` silences it.
    #[test]
    fn an_undeclared_operation_declares_nothing_and_public_declares_something() {
        let bare = builder();
        assert!(read_declarations(bare.spec()).is_empty());
        assert!(read_declaration(bare.spec()).is_none());

        let mut public = builder();
        <Public as Extract>::describe(&mut public);
        assert_eq!(
            read_declarations(public.spec()),
            vec![AuthzDeclaration::Public],
        );
        assert_eq!(
            public.spec().security.as_deref(),
            Some(&[][..]),
            "`#[public]` overrides a document-level requirement rather than inheriting it",
        );
    }

    /// A handler with `#[requires]` *and* an `Authorized<..>` parameter
    /// declares both, and the check should see both.
    #[test]
    fn several_declarations_accumulate_on_one_operation() {
        let mut op = builder();
        <Required<MayPublish> as Extract>::describe(&mut op);
        <Authorized<Publish, Post> as Extract>::describe(&mut op);

        let declarations = read_declarations(op.spec());
        assert_eq!(declarations.len(), 2);
        assert!(
            declarations
                .iter()
                .any(|d| matches!(d, AuthzDeclaration::Permissions { .. }))
        );
        assert!(
            declarations
                .iter()
                .any(|d| matches!(d, AuthzDeclaration::Policy { .. }))
        );
    }

    #[test]
    fn declaring_the_same_thing_twice_is_recorded_once() {
        let mut op = builder();
        <Public as Extract>::describe(&mut op);
        <Public as Extract>::describe(&mut op);

        assert_eq!(read_declarations(op.spec()).len(), 1);
    }

    #[test]
    fn a_declaration_round_trips_through_the_extension() {
        let mut op = builder();
        declare(
            &mut op,
            AuthzDeclaration::Permissions {
                names: vec!["posts.read".to_owned()],
                all: false,
            },
        );

        assert_eq!(
            read_declarations(op.spec()),
            vec![AuthzDeclaration::Permissions {
                names: vec!["posts.read".to_owned()],
                all: false,
            }],
        );
        assert_eq!(AUTHZ_EXTENSION, "x-moso-authz");
        assert!(AuthzDeclaration::Public.is_declared());
        assert!(!AuthzDeclaration::Public.is_problem());
    }

    #[test]
    fn a_single_declaration_written_as_an_object_is_still_read() {
        // Forward compatibility with anything that writes the extension by hand.
        let mut op = builder();
        op.extension(
            AUTHZ_EXTENSION,
            serde_json::to_value(AuthzDeclaration::Public).expect("encode"),
        );

        assert_eq!(read_declarations(op.spec()), vec![AuthzDeclaration::Public]);
    }

    // ── the document-level check `moso check --authz` runs ────────────────

    /// Assemble a one-operation document the way `App::build()` does.
    ///
    /// The describing closure is handed the *document's* schema generator, so
    /// the `Problem` schema an extractor's 403 refers to lands in
    /// `components.schemas` instead of dangling — the same arrangement
    /// [`moso_openapi::DocumentBuilder::operation`] gives a real handler. The
    /// two things a route contributes that an extractor cannot are supplied
    /// here for the same reason: the `id` path parameter the template declares,
    /// and the `moso_auth` security scheme `App::build()` registers.
    fn document_with(
        describe: impl FnOnce(&mut OperationBuilder),
    ) -> moso_openapi::document::Document {
        let mut builder = moso_openapi::DocumentBuilder::new();
        builder.title("test").version("0.0.0");
        builder.security_scheme(
            crate::AUTH_SCHEME,
            moso_openapi::security::SecurityScheme::http_bearer("JWT"),
        );
        builder.operation(moso_openapi::path::HttpMethod::Get, "/posts/{id}", |op| {
            op.parameter(moso_openapi::builder::Param::path("id").schema_of::<String>());
            describe(op);
        });
        builder.build().expect("the document assembles")
    }

    // ── where an operation is written ─────────────────────────────────────

    /// The third element of an `undeclared_operations` tuple is `None` unless
    /// something wrote the source, and `mark_source` is that something until
    /// `#[endpoint]` does it itself.
    #[test]
    fn a_located_operation_is_reported_with_its_file_and_line() {
        let document = document_with(|op| mark_source(op, "src/routes/posts.rs:42"));

        assert_eq!(
            undeclared_operations(&document),
            vec![(
                "GET",
                "/posts/{id}".to_owned(),
                Some("src/routes/posts.rs:42".to_owned()),
            )],
        );
    }

    /// First writer wins, so a location `#[endpoint]` records cannot be
    /// overwritten by a later, vaguer one.
    #[test]
    fn the_first_recorded_location_is_the_one_that_survives() {
        let mut op = builder();
        mark_source(&mut op, "src/routes/posts.rs:42");
        mark_source(&mut op, "somewhere else");

        assert_eq!(
            source_of(op.spec()).as_deref(),
            Some("src/routes/posts.rs:42"),
        );
        assert_eq!(SOURCE_EXTENSION, "x-moso-source");
    }

    #[test]
    fn the_source_macro_captures_this_file_and_this_line() {
        let here = crate::source!();

        assert!(
            here.starts_with("crates/moso-authz/src/extract.rs:"),
            "{here}"
        );
    }

    #[test]
    fn an_operation_nobody_located_reports_nothing_rather_than_guessing() {
        let op = builder();

        assert!(source_of(op.spec()).is_none());
        assert!(source_at(&moso_openapi::path::Operation::default()).is_none());
    }

    /// Acceptance criterion 2, on the artefact that exists after boot.
    #[test]
    fn an_undeclared_operation_is_reported_and_a_public_one_is_not() {
        let bare = document_with(|_| {});
        assert_eq!(
            undeclared_operations(&bare),
            vec![("GET", "/posts/{id}".to_owned(), None)],
        );

        let public = document_with(<Public as Extract>::describe);
        assert!(undeclared_operations(&public).is_empty());

        let required = document_with(<Required<MayPublish> as Extract>::describe);
        assert!(undeclared_operations(&required).is_empty());

        let authorized = document_with(<Authorized<Publish, Post> as Extract>::describe);
        assert!(undeclared_operations(&authorized).is_empty());
    }

    /// Acceptance criterion 1, on the same artefact: the boot check names the
    /// operation as well as the permission.
    #[test]
    fn a_mistyped_permission_is_reported_against_its_operation() {
        let document = document_with(<Required<Mistyped> as Extract>::describe);

        let problems = document_problems(&document);

        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].0, "GET /posts/{id}");
        assert!(
            problems[0]
                .1
                .to_string()
                .contains("did you mean `posts.publish`"),
            "{}",
            problems[0].1,
        );
    }

    #[test]
    fn a_clean_document_reports_nothing() {
        let document = document_with(<Required<MayPublish> as Extract>::describe);

        assert!(document_problems(&document).is_empty());
        assert_eq!(
            declarations_of(
                document.paths["/posts/{id}"]
                    .get
                    .as_ref()
                    .expect("the operation")
            )
            .len(),
            1,
        );
    }

    // ── the policy the fixture registers ──────────────────────────────────

    #[tokio::test]
    async fn the_read_policy_attaches_the_obligation_the_documentation_promises() {
        use crate::fixture::actor;

        let published = Post {
            id: 1,
            author_id: "usr_9".to_owned(),
            published: true,
            title: "Hello".to_owned(),
        };

        let peer: Actor<Role> = actor("usr_1", [Role::Viewer]);
        let decision = peer.can(Read, &published).await;
        assert!(decision.allowed());
        assert_eq!(decision.obligations().len(), 1);
        assert_eq!(decision.obligations()[0].pointer(), Some("/author_id"));

        let root: Actor<Role> = actor("usr_9", [Role::Owner]);
        assert!(root.can(Read, &published).await.obligations().is_empty());
    }
}
