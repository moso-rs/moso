//! Resource policies: [`Policy`], [`ScopedPolicy`], and the [`Decision`] they
//! return.
//!
//! Permissions cannot express "the author may edit their own post" — that
//! depends on the row. Policies can, and they are ordinary Rust: testable,
//! debuggable, refactorable, and visible to `rust-analyzer`. Moso deliberately
//! does not ship a policy language; the trade against something like Polar is
//! expressiveness for tooling, and the docs say so.

use std::borrow::Cow;
use std::sync::Arc;

use moso_orm::{Entity, Select};
use serde::{Deserialize, Serialize};

use crate::{ActorId, Scope};

/// A side effect a decision requires of whoever acts on it.
///
/// The feature that turns a boolean into something useful: a policy can allow
/// the read *and* require that a field be redacted, so "managers see salaries,
/// peers do not" needs one policy instead of two response types.
///
/// ```
/// use moso_authz::Obligation;
///
/// let redact = Obligation::redact("/salary");
/// assert_eq!(redact.pointer(), Some("/salary"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum Obligation {
    /// Remove a field from the serialised response.
    ///
    /// The pointer is an RFC 6901 JSON Pointer against the response body, the
    /// same shape a validation error's field pointer uses.
    Redact {
        /// Which field.
        pointer: String,
    },
    /// Replace all but the last `keep` characters of a field with `•`.
    Mask {
        /// Which field.
        pointer: String,
        /// How many trailing characters survive.
        keep: usize,
    },
    /// Something the application interprets.
    ///
    /// The escape hatch: "log this at `warn`", "add a banner", "require
    /// re-authentication within 5 minutes". Unrecognised keys are ignored, so
    /// an obligation the serialiser does not understand cannot silently drop a
    /// redaction.
    Custom {
        /// What kind of obligation.
        key: String,
        /// Its parameters.
        value: serde_json::Value,
    },
}

impl Obligation {
    /// Require a field to be removed.
    ///
    /// ```
    /// use moso_authz::Obligation;
    ///
    /// let _ = Obligation::redact("/salary");
    /// ```
    #[must_use]
    pub fn redact(pointer: impl Into<String>) -> Self {
        Self::Redact {
            pointer: pointer.into(),
        }
    }

    /// Require a field to be masked, keeping the last `keep` characters.
    ///
    /// ```
    /// use moso_authz::Obligation;
    ///
    /// let _ = Obligation::mask("/card_number", 4);
    /// ```
    #[must_use]
    pub fn mask(pointer: impl Into<String>, keep: usize) -> Self {
        Self::Mask {
            pointer: pointer.into(),
            keep,
        }
    }

    /// The field an obligation applies to, when it applies to one.
    ///
    /// ```
    /// use moso_authz::Obligation;
    ///
    /// assert_eq!(Obligation::redact("/a").pointer(), Some("/a"));
    /// ```
    #[must_use]
    pub fn pointer(&self) -> Option<&str> {
        match self {
            Self::Redact { pointer } | Self::Mask { pointer, .. } => Some(pointer),
            Self::Custom { .. } => None,
        }
    }

    /// Apply this obligation to a serialised body.
    ///
    /// [`Obligation::Custom`] does nothing here, by design: the application
    /// interprets it, and a serialiser that guessed at "require
    /// re-authentication" would be inventing behaviour. A pointer that matches
    /// nothing is also a no-op — an obligation on a field a particular response
    /// shape does not carry is satisfied vacuously, and failing the request
    /// instead would turn a policy refactor into an outage.
    ///
    /// The pointer is RFC 6901, so `/items/0/salary` reaches into an array and
    /// `~0`/`~1` escape `~` and `/`.
    ///
    /// ```
    /// use moso_authz::Obligation;
    ///
    /// let mut body = serde_json::json!({ "card": "4242424242424242" });
    /// Obligation::mask("/card", 4).apply(&mut body);
    ///
    /// assert_eq!(body["card"], "••••••••••••4242");
    /// ```
    pub fn apply(&self, body: &mut serde_json::Value) {
        match self {
            Self::Redact { pointer } => {
                remove_at(body, pointer);
            }
            Self::Mask { pointer, keep } => {
                if let Some(target) = body.pointer_mut(pointer) {
                    *target = serde_json::Value::String(mask_value(target, *keep));
                }
            }
            Self::Custom { .. } => {}
        }
    }
}

/// Remove whatever a JSON Pointer addresses, if anything.
///
/// `serde_json` has `pointer_mut` but no `remove`, and setting the field to
/// `null` is not redaction: a `null` still says the field exists and that this
/// caller was not allowed to see it, which is one bit more than they should
/// have.
fn remove_at(body: &mut serde_json::Value, pointer: &str) {
    let Some((parent, last)) = pointer.rsplit_once('/') else {
        return;
    };
    let key = unescape_token(last);
    let parent = if parent.is_empty() {
        Some(body)
    } else {
        body.pointer_mut(parent)
    };
    match parent {
        Some(serde_json::Value::Object(map)) => {
            map.shift_remove(&key);
        }
        Some(serde_json::Value::Array(items)) => {
            if let Ok(index) = key.parse::<usize>()
                && index < items.len()
            {
                items.remove(index);
            }
        }
        _ => {}
    }
}

/// Undo RFC 6901's two escapes. `~1` is `/` and `~0` is `~`, in that order.
fn unescape_token(token: &str) -> String {
    token.replace("~1", "/").replace("~0", "~")
}

/// Replace all but the last `keep` characters with `•`.
///
/// Counted in `char`s, not bytes: masking a card number is one thing, and
/// slicing a multi-byte grapheme in half is a panic.
fn mask_value(value: &serde_json::Value, keep: usize) -> String {
    let rendered = match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    let total = rendered.chars().count();
    let hidden = total.saturating_sub(keep);
    let mut masked = "•".repeat(hidden);
    masked.extend(rendered.chars().skip(hidden));
    masked
}

/// One line of an explain trace.
///
/// ```
/// use moso_authz::TraceStep;
///
/// let step = TraceStep::new("has(posts.publish)", false);
/// assert!(!step.passed());
/// assert_eq!(step.render(), "✗ has(posts.publish)");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceStep {
    /// What was checked, as the policy author would write it.
    check: Cow<'static, str>,
    /// Whether it held.
    passed: bool,
    /// Optional detail, e.g. the values that were compared.
    detail: Option<String>,
}

impl TraceStep {
    /// Record a check and its outcome.
    ///
    /// ```
    /// use moso_authz::TraceStep;
    ///
    /// let _ = TraceStep::new("post.author_id == actor.id", true);
    /// ```
    #[must_use]
    pub fn new(check: impl Into<Cow<'static, str>>, passed: bool) -> Self {
        Self {
            check: check.into(),
            passed,
            detail: None,
        }
    }

    /// Attach the values that were compared.
    ///
    /// ```
    /// use moso_authz::TraceStep;
    ///
    /// let _ = TraceStep::new("author", false).with_detail("author=usr_999, actor=usr_123");
    /// ```
    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Whether the check held.
    ///
    /// ```
    /// use moso_authz::TraceStep;
    ///
    /// assert!(TraceStep::new("x", true).passed());
    /// ```
    #[must_use]
    pub fn passed(&self) -> bool {
        self.passed
    }

    /// What was checked.
    ///
    /// ```
    /// use moso_authz::TraceStep;
    ///
    /// assert_eq!(TraceStep::new("x", true).check(), "x");
    /// ```
    #[must_use]
    pub fn check(&self) -> &str {
        &self.check
    }

    /// One line, as `moso authz explain` prints it.
    ///
    /// ```
    /// use moso_authz::TraceStep;
    ///
    /// assert_eq!(TraceStep::new("x", true).render(), "✓ x");
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let mark = if self.passed { '✓' } else { '✗' };
        match &self.detail {
            Some(detail) => format!("{mark} {} ({detail})", self.check),
            None => format!("{mark} {}", self.check),
        }
    }
}

/// What a policy decided, and why.
///
/// ```no_run
/// use moso_authz::Decision;
///
/// let allowed = Decision::allow("author");
/// assert!(allowed.allowed());
/// assert_eq!(allowed.reason(), "author");
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Decision {
    /// Yes or no.
    allowed: bool,
    /// Why, in the policy author's words.
    reason: Cow<'static, str>,
    /// What acting on this decision requires.
    obligations: Vec<Obligation>,
    /// The steps that led here, when explaining was asked for.
    trace: Vec<TraceStep>,
}

impl Decision {
    /// Allow, with a reason.
    ///
    /// The reason is not optional. A decision with no reason is a decision
    /// nobody can debug six months later, and the explain trace is the feature
    /// that makes this crate worth using.
    ///
    /// ```no_run
    /// use moso_authz::Decision;
    ///
    /// let _ = Decision::allow("admin override");
    /// ```
    #[must_use]
    pub fn allow(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            allowed: true,
            reason: reason.into(),
            obligations: Vec::new(),
            trace: Vec::new(),
        }
    }

    /// Deny, with a reason.
    ///
    /// ```no_run
    /// use moso_authz::Decision;
    ///
    /// let _ = Decision::deny("not the author and not an admin");
    /// ```
    #[must_use]
    pub fn deny(reason: impl Into<Cow<'static, str>>) -> Self {
        Self {
            allowed: false,
            reason: reason.into(),
            obligations: Vec::new(),
            trace: Vec::new(),
        }
    }

    /// Attach a required side effect.
    ///
    /// ```no_run
    /// use moso_authz::{Decision, Obligation};
    ///
    /// let _ = Decision::allow("peer").with_obligation(Obligation::redact("/salary"));
    /// ```
    #[must_use]
    pub fn with_obligation(mut self, obligation: Obligation) -> Self {
        self.obligations.push(obligation);
        self
    }

    /// Record a step in the explain trace.
    ///
    /// Cheap to call unconditionally: [`PolicyCtx::explain`] gates whether the
    /// trace is kept, and a policy that always records reads better than one
    /// full of `if ctx.explain()`.
    ///
    /// ```no_run
    /// use moso_authz::{Decision, TraceStep};
    ///
    /// let _ = Decision::deny("no").with_step(TraceStep::new("has(posts.publish)", false));
    /// ```
    #[must_use]
    pub fn with_step(mut self, step: TraceStep) -> Self {
        self.trace.push(step);
        self
    }

    /// Whether the action is allowed.
    ///
    /// ```no_run
    /// # use moso_authz::Decision;
    /// # fn f(d: &Decision) { let _: bool = d.allowed(); }
    /// ```
    #[must_use]
    pub fn allowed(&self) -> bool {
        self.allowed
    }

    /// Why.
    ///
    /// ```no_run
    /// # use moso_authz::Decision;
    /// # fn f(d: &Decision) { let _: &str = d.reason(); }
    /// ```
    #[must_use]
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// What acting on this decision requires.
    ///
    /// ```no_run
    /// # use moso_authz::{Decision, Obligation};
    /// # fn f(d: &Decision) { let _: &[Obligation] = d.obligations(); }
    /// ```
    #[must_use]
    pub fn obligations(&self) -> &[Obligation] {
        &self.obligations
    }

    /// The steps that led here.
    ///
    /// ```no_run
    /// # use moso_authz::{Decision, TraceStep};
    /// # fn f(d: &Decision) { let _: &[TraceStep] = d.trace(); }
    /// ```
    #[must_use]
    pub fn trace(&self) -> &[TraceStep] {
        &self.trace
    }

    /// Turn a denial into the error it means.
    ///
    /// `Ok(self)` when allowed, so a call site reads
    /// `let decision = actor.can(..).await.into_result("publish", "Post#42")?;`
    /// and keeps the obligations.
    ///
    /// # Errors
    ///
    /// [`Error::Denied`](crate::Error::Denied) carrying the reason.
    ///
    /// ```no_run
    /// # use moso_authz::Decision;
    /// # fn f(d: Decision) -> moso_authz::Result<Decision> {
    /// d.into_result("publish", "Post#42")
    /// # }
    /// ```
    pub fn into_result(
        self,
        action: &'static str,
        resource: impl Into<Cow<'static, str>>,
    ) -> crate::Result<Self> {
        if self.allowed {
            return Ok(self);
        }
        Err(crate::Error::Denied {
            action,
            resource: resource.into(),
            reason: self.reason,
        })
    }

    /// Apply this decision's obligations to a serialised body.
    ///
    /// The half of the obligation story that makes it more than documentation:
    /// a policy that allows a read *and* requires `/salary` to be redacted only
    /// means something if something removes the field. Called by
    /// [`Redacted<T>`](crate::Redacted), and public so a handler that builds its
    /// own response can honour the same obligations.
    ///
    /// ```
    /// use moso_authz::{Decision, Obligation};
    ///
    /// let decision = Decision::allow("peer").with_obligation(Obligation::redact("/salary"));
    /// let mut body = serde_json::json!({ "name": "Ada", "salary": 120_000 });
    /// decision.apply_obligations(&mut body);
    ///
    /// assert_eq!(body, serde_json::json!({ "name": "Ada" }));
    /// ```
    pub fn apply_obligations(&self, body: &mut serde_json::Value) {
        for obligation in &self.obligations {
            obligation.apply(body);
        }
    }
}

/// What a policy knows besides the actor and the resource.
///
/// ```no_run
/// use moso_authz::PolicyCtx;
///
/// # fn f(ctx: &PolicyCtx) {
/// if ctx.explain() {
///     // record extra detail
/// }
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct PolicyCtx {
    /// The correlation id of the request being authorised, when there is one.
    request_id: Option<String>,
    /// Who is asking. Duplicated from the actor so an audit record needs only
    /// the context.
    actor: ActorId,
    /// The scope the check is happening in.
    scope: Scope,
    /// Whether to keep the explain trace.
    explain: bool,
    /// Whether the process is running in a development profile, which decides
    /// whether a denial's reason reaches the client.
    development: bool,
    /// The policies the application registered, when it registered any. What
    /// puts the `policy` row in a live explain block.
    policies: Option<Arc<PolicyRegistry>>,
}

impl PolicyCtx {
    /// The context for a check about `actor` in `scope`, outside a request.
    ///
    /// [`detached_ctx`](crate::detached_ctx) is the no-argument form.
    ///
    /// ```
    /// use moso_authz::{ActorId, PolicyCtx, Scope};
    ///
    /// let ctx = PolicyCtx::new(ActorId::new("usr_1"), Scope::Global);
    /// assert!(!ctx.explain());
    /// assert_eq!(ctx.actor().as_str(), "usr_1");
    /// ```
    #[must_use]
    pub fn new(actor: ActorId, scope: Scope) -> Self {
        Self {
            request_id: None,
            actor,
            scope,
            explain: false,
            development: false,
            policies: None,
        }
    }

    /// Attach the request this check belongs to.
    ///
    /// ```
    /// use moso_authz::{ActorId, PolicyCtx, Scope};
    ///
    /// let ctx = PolicyCtx::new(ActorId::anonymous(), Scope::Global)
    ///     .for_request("01JABCDEF", true, true);
    ///
    /// assert!(ctx.explain());
    /// assert_eq!(ctx.request_id(), Some("01JABCDEF"));
    /// ```
    #[must_use]
    pub fn for_request(
        mut self,
        request_id: impl Into<String>,
        explain: bool,
        development: bool,
    ) -> Self {
        self.request_id = Some(request_id.into());
        // An explain trace describes the authorization model to whoever asked
        // for it, so the profile is the answer and the header is only a request.
        self.explain = explain && development;
        self.development = development;
        self
    }

    /// Attach the registry of policies the application declared.
    ///
    /// [`Authorized`](crate::Authorized) calls this with whatever
    /// `PolicyRegistry` is in the provider map, which is what puts the `policy`
    /// row — the `impl` signature and its `file:line` — into a live explain
    /// block. Without it the block is still rendered, just without that row.
    ///
    /// ```
    /// use std::sync::Arc;
    ///
    /// use moso_authz::{ActorId, PolicyCtx, PolicyRef, PolicyRegistry, Scope};
    ///
    /// let registry = PolicyRegistry::new()
    ///     .register(PolicyRef::typed("publish", "Publish", "Post", "src/authz/post.rs", 14));
    ///
    /// let ctx = PolicyCtx::new(ActorId::new("usr_1"), Scope::Global)
    ///     .with_policies(Arc::new(registry));
    ///
    /// assert_eq!(
    ///     ctx.policy_for("publish", "Post").map(PolicyRef::location),
    ///     Some("src/authz/post.rs:14".to_owned()),
    /// );
    /// assert!(ctx.policy_for("publish", "Invoice").is_none());
    /// ```
    #[must_use]
    pub fn with_policies(mut self, policies: Arc<PolicyRegistry>) -> Self {
        self.policies = Some(policies);
        self
    }

    /// The registry, when the application registered one.
    ///
    /// ```
    /// use moso_authz::{ActorId, PolicyCtx, Scope};
    ///
    /// assert!(PolicyCtx::new(ActorId::anonymous(), Scope::Global).policies().is_none());
    /// ```
    #[must_use]
    pub fn policies(&self) -> Option<&Arc<PolicyRegistry>> {
        self.policies.as_ref()
    }

    /// Where the policy for `action` on `resource` is written.
    ///
    /// `None` when no registry was attached or the pair was never registered —
    /// which is honest: a location this crate guessed at would be worse than no
    /// `policy` row at all.
    ///
    /// ```
    /// use moso_authz::{ActorId, PolicyCtx, Scope};
    ///
    /// let ctx = PolicyCtx::new(ActorId::anonymous(), Scope::Global);
    /// assert!(ctx.policy_for("publish", "Post").is_none());
    /// ```
    #[must_use]
    pub fn policy_for(&self, action: &str, resource: &str) -> Option<PolicyRef> {
        self.policies
            .as_ref()
            .and_then(|registry| registry.lookup(action, resource))
    }

    /// Whether the process is running in a development profile.
    ///
    /// What decides whether a denial's reason reaches the client.
    ///
    /// ```
    /// use moso_authz::{ActorId, PolicyCtx, Scope};
    ///
    /// assert!(!PolicyCtx::new(ActorId::anonymous(), Scope::Global).development());
    /// ```
    #[must_use]
    pub fn development(&self) -> bool {
        self.development
    }

    /// Whether the caller asked for an explain trace.
    ///
    /// Set by the `X-Moso-Authz-Explain: 1` header, and only honoured in a
    /// development profile — an explain trace in production is a description of
    /// the authorization model handed to whoever asked.
    ///
    /// ```no_run
    /// # use moso_authz::PolicyCtx;
    /// # fn f(c: &PolicyCtx) { let _: bool = c.explain(); }
    /// ```
    #[must_use]
    pub fn explain(&self) -> bool {
        self.explain
    }

    /// Who is asking.
    ///
    /// ```no_run
    /// # use moso_authz::{ActorId, PolicyCtx};
    /// # fn f(c: &PolicyCtx) { let _: &ActorId = c.actor(); }
    /// ```
    #[must_use]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// The scope the check is happening in.
    ///
    /// ```no_run
    /// # use moso_authz::{PolicyCtx, Scope};
    /// # fn f(c: &PolicyCtx) { let _: &Scope = c.scope(); }
    /// ```
    #[must_use]
    pub fn scope(&self) -> &Scope {
        &self.scope
    }

    /// The correlation id of the request, for the audit record.
    ///
    /// ```no_run
    /// # use moso_authz::PolicyCtx;
    /// # fn f(c: &PolicyCtx) { let _: Option<&str> = c.request_id(); }
    /// ```
    #[must_use]
    pub fn request_id(&self) -> Option<&str> {
        self.request_id.as_deref()
    }
}

/// May this actor do this to this resource?
///
/// Implemented by the application on [`Actor<R>`](crate::Actor), once per
/// (action, resource) pair. RPITIT rather than a boxed future: policies are
/// generic and monomorphised, and the framework adds a tracing span and nothing
/// else.
///
/// ```text
/// // `Role` here is the enum `moso::roles!` generated in this crate. The impl
/// // must name it concretely: `impl<R: Role> Policy<Edit, Post> for Actor<R>`
/// // is rejected by the orphan rule, because `R` is an uncovered parameter in
/// // a foreign type ahead of the first local one. `crates/moso-authz/tests`
/// // compiles the concrete form, which is what a real application writes.
/// impl Policy<Edit, Post> for Actor<Role> {
///     async fn allows(&self, _: Edit, post: &Post, _ctx: &PolicyCtx) -> Decision {
///         if post.author_id == self.id().as_str() {
///             return Decision::allow("author");
///         }
///         if self.has(Perm::AdminAccess) {
///             return Decision::allow("admin override");
///         }
///         Decision::deny("not the author and not an admin")
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "no policy says whether `{Self}` may perform this action on `{R}`",
    label = "no policy for this action and resource",
    note = "authorization is deny-by-default: a missing policy is not an allow",
    note = "help: write `impl Policy<{A}, {R}> for Actor<Role>` with one `allows` method \
            returning `Decision::allow(reason)` or `Decision::deny(reason)`",
    note = "help: for a check that only needs a permission and not the row, use \
            `#[requires(Perm::…)]` instead — no policy needed"
)]
pub trait Policy<A, R>: Send + Sync {
    /// Decide.
    ///
    /// A policy does not fail: an unreachable store is
    /// [`ActorSource`](crate::ActorSource)'s problem, and by the time a policy
    /// runs everything it needs is in hand. That is what makes the return type
    /// a [`Decision`] and not a `Result<Decision>`, and it is what keeps
    /// "denied" and "broken" from being the same value.
    fn allows(
        &self,
        action: A,
        resource: &R,
        ctx: &PolicyCtx,
    ) -> impl Future<Output = Decision> + Send;
}

/// The same question, asked of a query instead of a row.
///
/// Checking each row after loading is wrong at scale: it makes pagination
/// counts lie and reads rows the caller may not see. A scoped policy contributes
/// a filter, so the database does the work and the totals are right.
///
/// ```text
/// // Again, `Role` is the concrete generated enum — see `Policy` above.
/// impl ScopedPolicy<Read, Post> for Actor<Role> {
///     fn scope_query(&self, query: Select<Post>) -> Select<Post> {
///         if self.has(Perm::AdminAccess) {
///             return query;
///         }
///         query.filter(Post::PUBLISHED.eq(true) | Post::AUTHOR_ID.eq(self.id().as_str()))
///     }
/// }
/// ```
///
/// # Why it takes `Select<R>` and not `Select<R, J>`
///
/// The second parameter of [`Select`] is the tenant obligation, and tenant
/// scoping must happen *before* authorization — an authorization filter applied
/// to an unscoped query is a filter across every tenant's rows. Taking the
/// `Ready` shape makes that ordering a compile error instead of a review
/// comment.
#[diagnostic::on_unimplemented(
    message = "no scoped policy says which `{R}` rows `{Self}` may list",
    label = "no query filter for this action and resource",
    note = "`authorized_for::<{A}>` needs a `ScopedPolicy<{A}, {R}>`, which contributes a \
            `WHERE` clause rather than filtering rows after loading",
    note = "help: write `impl ScopedPolicy<{A}, {R}> for Actor<Role>` with one `scope_query` \
            method returning the query, filtered",
    note = "help: filtering after `fetch_all` instead makes pagination counts wrong; that is \
            what this trait exists to prevent"
)]
pub trait ScopedPolicy<A, R: Entity>: Send + Sync {
    /// Narrow a query to the rows this actor may see.
    ///
    /// Synchronous, and it must stay that way: it runs while a statement is
    /// being built, and an `await` here would mean a query per query.
    fn scope_query(&self, query: Select<R>) -> Select<R>;
}

/// Where a policy is written, for `moso check --authz` and the explain output.
///
/// ```
/// use moso_authz::PolicyRef;
///
/// const EDIT_POST: PolicyRef = PolicyRef::new("edit", "Post", "src/authz/post.rs", 14);
/// assert_eq!(EDIT_POST.action(), "edit");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PolicyRef {
    /// The action's wire name, e.g. `"publish"`.
    action: &'static str,
    /// The action *type*'s name, e.g. `"Publish"`, so an explain trace can
    /// print the `impl` as it is written rather than as it is serialised.
    action_type: &'static str,
    /// The resource's name.
    resource: &'static str,
    /// The file the `impl` is in.
    file: &'static str,
    /// The line it starts on.
    line: u32,
}

impl PolicyRef {
    /// Describe a policy.
    ///
    /// ```
    /// use moso_authz::PolicyRef;
    ///
    /// let _ = PolicyRef::new("publish", "Post", "src/authz/post.rs", 14);
    /// ```
    #[must_use]
    pub const fn new(
        action: &'static str,
        resource: &'static str,
        file: &'static str,
        line: u32,
    ) -> Self {
        Self::typed(action, action, resource, file, line)
    }

    /// Describe a policy, naming the action's *type* as well as its wire name.
    ///
    /// What [`policy!`](crate::policy!) emits, because `stringify!` has the type
    /// and `Action::NAME` has the wire name, and an explain trace wants to
    /// print `Policy<Publish, Post>` rather than `Policy<publish, Post>`.
    ///
    /// ```
    /// use moso_authz::PolicyRef;
    ///
    /// const P: PolicyRef = PolicyRef::typed("publish", "Publish", "Post", "src/a.rs", 14);
    /// assert_eq!(P.signature(), "Policy<Publish, Post> for Actor");
    /// ```
    #[must_use]
    pub const fn typed(
        action: &'static str,
        action_type: &'static str,
        resource: &'static str,
        file: &'static str,
        line: u32,
    ) -> Self {
        Self {
            action,
            action_type,
            resource,
            file,
            line,
        }
    }

    /// The action type's name.
    ///
    /// ```
    /// # use moso_authz::PolicyRef;
    /// # const P: PolicyRef = PolicyRef::typed("publish", "Publish", "Post", "f", 1);
    /// assert_eq!(P.action_type(), "Publish");
    /// ```
    #[must_use]
    pub const fn action_type(self) -> &'static str {
        self.action_type
    }

    /// The `impl` header, as it is written in source.
    ///
    /// ```
    /// # use moso_authz::PolicyRef;
    /// # const P: PolicyRef = PolicyRef::new("edit", "Post", "f", 1);
    /// assert_eq!(P.signature(), "Policy<edit, Post> for Actor");
    /// ```
    #[must_use]
    pub fn signature(self) -> String {
        format!("Policy<{}, {}> for Actor", self.action_type, self.resource)
    }

    /// The action's name.
    ///
    /// ```
    /// # use moso_authz::PolicyRef;
    /// # const P: PolicyRef = PolicyRef::new("a", "R", "f", 1);
    /// assert_eq!(P.action(), "a");
    /// ```
    #[must_use]
    pub const fn action(self) -> &'static str {
        self.action
    }

    /// The resource's name.
    ///
    /// ```
    /// # use moso_authz::PolicyRef;
    /// # const P: PolicyRef = PolicyRef::new("a", "R", "f", 1);
    /// assert_eq!(P.resource(), "R");
    /// ```
    #[must_use]
    pub const fn resource(self) -> &'static str {
        self.resource
    }

    /// Where it is written, as `file:line`.
    ///
    /// ```
    /// # use moso_authz::PolicyRef;
    /// # const P: PolicyRef = PolicyRef::new("a", "R", "src/x.rs", 9);
    /// assert_eq!(P.location(), "src/x.rs:9");
    /// ```
    #[must_use]
    pub fn location(self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

/// Every policy the application registered, enumerable at boot.
///
/// Registration is explicit, like every other registry in Moso (ADR-0004: no
/// link-time magic). What it buys is an explain block — offline or from a live
/// 403 — being able to name the file and line of the policy that produced a
/// denial.
///
/// # Wiring it up
///
/// Build it with [`policy!`](crate::policy!), which captures the call site, and
/// register it as a provider. [`Authorized`](crate::Authorized) looks for it
/// there, hands it to the [`PolicyCtx`], and uses it when it renders an
/// explanation:
///
/// ```text
/// let policies = moso_authz::policy!(PolicyRegistry::new(), Publish, "Post");
/// let policies = moso_authz::policy!(policies, Read, "Post");
///
/// App::new(config)
///     .provide(policies)          // ← the `policy` row in a live explain block
///     .provide_dyn::<dyn ActorSource<Role>>(Arc::new(SessionActor))
/// ```
///
/// An application that registers nothing is not broken: the block is rendered
/// without the `policy` row, because a location this crate invented would be
/// worse than an admitted gap.
///
/// ```no_run
/// use moso_authz::{PolicyRef, PolicyRegistry};
///
/// let registry = PolicyRegistry::new()
///     .register(PolicyRef::new("publish", "Post", "src/authz/post.rs", 14));
/// assert_eq!(registry.len(), 1);
/// ```
#[derive(Clone, Debug, Default)]
pub struct PolicyRegistry {
    /// Every registered policy.
    entries: Vec<PolicyRef>,
}

impl PolicyRegistry {
    /// An empty registry.
    ///
    /// ```no_run
    /// use moso_authz::PolicyRegistry;
    ///
    /// let _ = PolicyRegistry::new();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a policy.
    ///
    /// ```no_run
    /// # use moso_authz::{PolicyRef, PolicyRegistry};
    /// # fn f(r: PolicyRegistry, p: PolicyRef) { let _ = r.register(p); }
    /// ```
    #[must_use]
    pub fn register(mut self, policy: PolicyRef) -> Self {
        self.entries.push(policy);
        self
    }

    /// Every registered policy.
    ///
    /// ```no_run
    /// # use moso_authz::{PolicyRef, PolicyRegistry};
    /// # fn f(r: &PolicyRegistry) { let _: &[PolicyRef] = r.all(); }
    /// ```
    #[must_use]
    pub fn all(&self) -> &[PolicyRef] {
        &self.entries
    }

    /// The policy for an action and resource, when one was registered.
    ///
    /// ```no_run
    /// # use moso_authz::{PolicyRef, PolicyRegistry};
    /// # fn f(r: &PolicyRegistry) { let _: Option<PolicyRef> = r.lookup("publish", "Post"); }
    /// ```
    #[must_use]
    pub fn lookup(&self, action: &str, resource: &str) -> Option<PolicyRef> {
        self.entries
            .iter()
            .copied()
            .find(|entry| entry.action() == action && entry.resource() == resource)
    }

    /// How many policies are registered.
    ///
    /// ```no_run
    /// # use moso_authz::PolicyRegistry;
    /// # fn f(r: &PolicyRegistry) { let _: usize = r.len(); }
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is registered.
    ///
    /// ```no_run
    /// # use moso_authz::PolicyRegistry;
    /// # fn f(r: &PolicyRegistry) { let _: bool = r.is_empty(); }
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Register a policy with its source location filled in.
///
/// ```no_run
/// use moso_authz::PolicyRegistry;
///
/// moso_authz::actions! {
///     /// Making a draft public.
///     Publish = "publish",
/// }
///
/// let registry = PolicyRegistry::new();
/// let registry = moso_authz::policy!(registry, Publish, "Post");
/// assert_eq!(registry.len(), 1);
/// ```
#[macro_export]
macro_rules! policy {
    ($registry:expr, $action:ty, $resource:literal) => {
        $registry.register($crate::PolicyRef::typed(
            <$action as $crate::Action>::NAME,
            ::core::stringify!($action),
            $resource,
            ::core::file!(),
            ::core::line!(),
        ))
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Perm, Post, Publish, Read, Role, actor};
    use crate::{Actor, AuthorizedQuery, PermSet};

    #[test]
    fn a_decision_carries_its_reason_and_nothing_else_by_default() {
        let allowed = Decision::allow("author");

        assert!(allowed.allowed());
        assert_eq!(allowed.reason(), "author");
        assert!(allowed.obligations().is_empty());
        assert!(allowed.trace().is_empty());

        let denied = Decision::deny("not the author");
        assert!(!denied.allowed());
        assert_eq!(denied.reason(), "not the author");
    }

    #[test]
    fn obligations_and_steps_accumulate_in_order() {
        let decision = Decision::allow("peer")
            .with_obligation(Obligation::redact("/salary"))
            .with_obligation(Obligation::mask("/card", 4))
            .with_step(TraceStep::new("first", true))
            .with_step(TraceStep::new("second", false));

        assert_eq!(decision.obligations().len(), 2);
        assert_eq!(decision.obligations()[0].pointer(), Some("/salary"));
        assert_eq!(decision.trace()[1].check(), "second");
    }

    #[test]
    fn into_result_keeps_an_allow_and_names_a_denial() {
        let kept = Decision::allow("ok")
            .with_obligation(Obligation::redact("/x"))
            .into_result("read", "Post#1")
            .expect("allowed");
        assert_eq!(kept.obligations().len(), 1);

        let error = Decision::deny("nope")
            .into_result("publish", "Post#42")
            .expect_err("denied");
        assert_eq!(error.to_string(), "publish on Post#42 denied: nope");
    }

    // ── obligations, applied ──────────────────────────────────────────────

    /// Acceptance criterion 5: the serialiser honours obligations. The full
    /// snapshot lives in `redact.rs`; this is the primitive.
    #[test]
    fn a_redaction_removes_a_top_level_field() {
        let mut body = serde_json::json!({ "name": "Ada", "salary": 1 });
        Obligation::redact("/salary").apply(&mut body);

        assert_eq!(body, serde_json::json!({ "name": "Ada" }));
    }

    #[test]
    fn a_redaction_reaches_into_a_nested_object_and_an_array() {
        let mut body = serde_json::json!({
            "user": { "name": "Ada", "salary": 1 },
            "items": [{ "secret": 1 }, { "secret": 2 }],
        });
        Obligation::redact("/user/salary").apply(&mut body);
        Obligation::redact("/items/0/secret").apply(&mut body);

        assert_eq!(body["user"], serde_json::json!({ "name": "Ada" }));
        assert_eq!(body["items"][0], serde_json::json!({}));
        assert_eq!(body["items"][1]["secret"], 2);
    }

    #[test]
    fn a_redaction_can_remove_an_array_element() {
        let mut body = serde_json::json!({ "items": [1, 2, 3] });
        Obligation::redact("/items/1").apply(&mut body);

        assert_eq!(body, serde_json::json!({ "items": [1, 3] }));
    }

    /// RFC 6901: `~1` is `/` and `~0` is `~`, decoded in that order.
    #[test]
    fn a_pointer_token_is_unescaped_the_way_rfc_6901_says() {
        let mut body = serde_json::json!({ "a/b": 1, "c~d": 2 });
        Obligation::redact("/a~1b").apply(&mut body);
        Obligation::redact("/c~0d").apply(&mut body);

        assert_eq!(body, serde_json::json!({}));
    }

    #[test]
    fn a_mask_counts_characters_and_not_bytes() {
        let mut body = serde_json::json!({ "name": "Ævar Bjarkan" });
        Obligation::mask("/name", 7).apply(&mut body);

        assert_eq!(body["name"], "•••••Bjarkan");
    }

    #[test]
    fn a_mask_of_a_number_renders_it_first() {
        let mut body = serde_json::json!({ "account": 123_456_789 });
        Obligation::mask("/account", 4).apply(&mut body);

        assert_eq!(body["account"], "•••••6789");
    }

    #[test]
    fn a_mask_that_keeps_more_than_there_is_hides_nothing() {
        let mut body = serde_json::json!({ "pin": "12" });
        Obligation::mask("/pin", 9).apply(&mut body);

        assert_eq!(body["pin"], "12");
    }

    #[test]
    fn an_unknown_obligation_never_silently_drops_a_redaction() {
        let decision = Decision::allow("peer")
            .with_obligation(Obligation::Custom {
                key: "banner".to_owned(),
                value: serde_json::json!("careful"),
            })
            .with_obligation(Obligation::redact("/salary"));

        let mut body = serde_json::json!({ "name": "Ada", "salary": 1 });
        decision.apply_obligations(&mut body);

        assert_eq!(body, serde_json::json!({ "name": "Ada" }));
    }

    #[test]
    fn an_obligation_round_trips_through_json() {
        let obligation = Obligation::mask("/card", 4);
        let encoded = serde_json::to_string(&obligation).expect("encode");

        assert!(encoded.contains("\"kind\":\"mask\""), "{encoded}");
        assert_eq!(
            serde_json::from_str::<Obligation>(&encoded).expect("decode"),
            obligation,
        );
    }

    // ── trace steps ───────────────────────────────────────────────────────

    #[test]
    fn a_trace_step_renders_with_a_tick_or_a_cross() {
        assert_eq!(
            TraceStep::new("authenticated", true).render(),
            "✓ authenticated"
        );
        assert_eq!(
            TraceStep::new("has(posts.publish) → false", false).render(),
            "✗ has(posts.publish) → false",
        );
        assert_eq!(
            TraceStep::new("resource loaded", true)
                .with_detail("post:456, author=usr_999")
                .render(),
            "✓ resource loaded (post:456, author=usr_999)",
        );
    }

    // ── the policy context ────────────────────────────────────────────────

    /// An explain trace in production hands the authorization model to whoever
    /// asked for it, so the profile is the answer and the header is a request.
    #[test]
    fn explain_is_only_honoured_in_a_development_profile() {
        let dev =
            PolicyCtx::new(ActorId::new("usr_1"), Scope::Global).for_request("01J", true, true);
        let prod =
            PolicyCtx::new(ActorId::new("usr_1"), Scope::Global).for_request("01J", true, false);

        assert!(dev.explain());
        assert!(!prod.explain());
        assert!(!prod.development());
        assert_eq!(dev.request_id(), Some("01J"));
        assert_eq!(dev.actor().as_str(), "usr_1");
        assert!(dev.scope().is_global());
    }

    // ── the policy registry ───────────────────────────────────────────────

    #[test]
    fn the_registry_finds_a_policy_by_action_and_resource() {
        let registry = PolicyRegistry::new()
            .register(PolicyRef::typed(
                "publish", "Publish", "Post", "src/a.rs", 14,
            ))
            .register(PolicyRef::typed("read", "Read", "Post", "src/a.rs", 30));

        assert_eq!(registry.len(), 2);
        assert!(!registry.is_empty());
        assert_eq!(
            registry.lookup("publish", "Post").map(PolicyRef::location),
            Some("src/a.rs:14".to_owned()),
        );
        assert!(registry.lookup("publish", "User").is_none());
        assert_eq!(registry.all().len(), 2);
    }

    #[test]
    fn the_policy_macro_captures_the_action_type_and_the_call_site() {
        let registry = crate::policy!(PolicyRegistry::new(), Publish, "Post");
        let entry = registry.lookup("publish", "Post").expect("registered");

        assert_eq!(entry.action(), "publish");
        assert_eq!(entry.action_type(), "Publish");
        assert_eq!(entry.resource(), "Post");
        assert_eq!(entry.signature(), "Policy<Publish, Post> for Actor");
        assert!(
            entry.location().contains("policy.rs"),
            "{}",
            entry.location()
        );
    }

    // ── scoped policies ───────────────────────────────────────────────────

    /// Acceptance criterion 4, the SQL half. The row-count half needs a real
    /// database and lives in `tests/acceptance.rs`.
    #[test]
    fn a_scoped_policy_contributes_a_where_clause() {
        let alice: Actor<Role> = actor("usr_1", [Role::Editor]);
        let query = moso_orm::Select::<Post>::new().authorized_for::<Read>(&alice);
        let sql = query
            .to_statement()
            .expect("statement")
            .build(&moso_sql::Postgres)
            .expect("renders")
            .text;

        assert!(sql.to_ascii_uppercase().contains("WHERE"), "{sql}");
        assert!(sql.contains("published"), "{sql}");
        assert!(sql.contains("author_id"), "{sql}");
        assert!(
            sql.contains(" OR "),
            "the two branches are a disjunction: {sql}"
        );
    }

    #[test]
    fn an_administrator_gets_the_query_back_unfiltered() {
        let root: Actor<Role> = actor("usr_9", [Role::Owner]);
        let query = moso_orm::Select::<Post>::new().authorized_for::<Read>(&root);

        assert!(query.filters().is_empty(), "an admin sees everything");
    }

    /// Non-negotiable N1: the builder's type never changes, so authorization
    /// composes with `filter`, `order_by` and `paginate` in any order.
    #[test]
    fn authorizing_a_query_is_shape_stable() {
        let alice: Actor<Role> = actor("usr_1", [Role::Editor]);

        let before: moso_orm::Select<Post> = moso_orm::Select::<Post>::new()
            .authorized_for::<Read>(&alice)
            .filter(Post::published().eq(true));
        let after: moso_orm::Select<Post> = moso_orm::Select::<Post>::new()
            .filter(Post::published().eq(true))
            .authorized_for::<Read>(&alice);

        assert_eq!(before.filters().len(), after.filters().len());
    }

    #[test]
    fn the_conditional_form_leaves_the_query_alone_when_it_is_off() {
        let alice: Actor<Role> = actor("usr_1", [Role::Editor]);

        let unfiltered = moso_orm::Select::<Post>::new().authorized_for_if::<Read>(false, &alice);
        let filtered = moso_orm::Select::<Post>::new().authorized_for_if::<Read>(true, &alice);

        assert!(unfiltered.filters().is_empty());
        assert_eq!(filtered.filters().len(), 1);
    }

    #[test]
    fn the_scoped_policy_reads_the_same_permissions_the_row_policy_does() {
        let viewer: Actor<Role> = actor("usr_1", [Role::Viewer]);
        assert!(!viewer.has(Perm::AdminAccess));
        assert_eq!(
            PermSet::of([Perm::PostsRead, Perm::UsersRead]),
            viewer.permissions(),
        );
    }
}
