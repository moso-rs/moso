//! Why a request was allowed or denied, in a form a human can read.
//!
//! "It just says forbidden" is the recurring support ticket of every
//! authorization system. An [`Explanation`] turns that into self-service: the
//! actor, their roles, the permissions those roles grant, what was required,
//! which policy ran, and the trace of every check inside it.
//!
//! Two ways to get one: `moso authz explain --user … --action … --resource …`
//! offline, and the `X-Moso-Authz-Explain: 1` header on a live request. The
//! header is honoured **only in a development profile** — an explain trace in
//! production hands the authorization model to whoever asked for it.

use serde::{Deserialize, Serialize};

use crate::{ActorId, Decision, PermRef, Scope, TraceStep};

/// The request header that asks for a trace in the 403 body.
///
/// ```
/// assert_eq!(moso_authz::EXPLAIN_HEADER, "x-moso-authz-explain");
/// ```
pub const EXPLAIN_HEADER: &str = "x-moso-authz-explain";

/// A full account of one authorization decision.
///
/// ```no_run
/// use moso_authz::Explanation;
///
/// # fn f(e: &Explanation) {
/// print!("{}", e.render());
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Explanation {
    /// Whether the action was allowed.
    pub allowed: bool,
    /// Who was asking.
    pub actor: ActorId,
    /// A human label for the actor, when the application supplied one — an
    /// email, a key prefix, a service name.
    pub actor_label: Option<String>,
    /// The scope the check happened in.
    pub scope: Scope,
    /// The roles held, with the scope each is held in.
    pub roles: Vec<(String, Scope)>,
    /// Every permission the actor holds, with its description.
    pub permissions: Vec<PermRef>,
    /// What the endpoint required, when it required permissions.
    pub required: Vec<PermRef>,
    /// The action's name, when a policy ran.
    pub action: Option<String>,
    /// The resource, as `Name#id`.
    pub resource: Option<String>,
    /// Where the policy is written, as `file:line`.
    pub policy_location: Option<String>,
    /// The policy as it is written in source, e.g.
    /// `Policy<Publish, Post> for Actor`.
    pub policy_signature: Option<String>,
    /// The role the listed permissions came from, when exactly one did.
    pub granted_by: Option<String>,
    /// The reason the decision carried.
    pub reason: String,
    /// Every check, in order.
    pub trace: Vec<TraceStep>,
}

impl Explanation {
    /// Build an explanation from a decision and its surroundings.
    ///
    /// ```no_run
    /// # use moso_authz::{ActorId, Decision, Explanation, Scope};
    /// # fn f(d: &Decision, id: &ActorId) {
    /// let _ = Explanation::of(d, id, &Scope::Global);
    /// # }
    /// ```
    #[must_use]
    pub fn of(decision: &Decision, actor: &ActorId, scope: &Scope) -> Self {
        Self {
            allowed: decision.allowed(),
            actor: actor.clone(),
            actor_label: None,
            scope: scope.clone(),
            roles: Vec::new(),
            permissions: Vec::new(),
            required: Vec::new(),
            action: None,
            resource: None,
            policy_location: None,
            policy_signature: None,
            granted_by: None,
            reason: decision.reason().to_owned(),
            trace: decision.trace().to_vec(),
        }
    }

    /// Attach the human label an application knows the actor by.
    ///
    /// An email, an API key prefix, a service name. Optional because this crate
    /// does not know how to find one — it does not depend on `moso-auth` — and
    /// because an explanation without it is still useful.
    ///
    /// ```
    /// use moso_authz::{ActorId, Decision, Explanation, Scope};
    ///
    /// let explanation = Explanation::of(&Decision::deny("no"), &ActorId::new("usr_1"), &Scope::Global)
    ///     .labelled("alice@example.com");
    ///
    /// assert_eq!(explanation.actor_label.as_deref(), Some("alice@example.com"));
    /// ```
    #[must_use]
    pub fn labelled(mut self, label: impl Into<String>) -> Self {
        self.actor_label = Some(label.into());
        self
    }

    /// Attach the actor's roles and permissions.
    ///
    /// Separate from [`of`](Explanation::of) because resolving them costs a
    /// query, and an explanation that nobody asked for should not.
    ///
    /// ```no_run
    /// # use moso_authz::{Explanation, PermRef, Scope};
    /// # fn f(e: Explanation, r: Vec<(String, Scope)>, p: Vec<PermRef>) {
    /// let _ = e.with_grants(r, p);
    /// # }
    /// ```
    #[must_use]
    pub fn with_grants(mut self, roles: Vec<(String, Scope)>, permissions: Vec<PermRef>) -> Self {
        self.roles = roles;
        self.permissions = permissions;
        self
    }

    /// Attach what the endpoint required and which policy ran.
    ///
    /// ```no_run
    /// # use moso_authz::{Explanation, PermRef};
    /// # fn f(e: Explanation, req: Vec<PermRef>) {
    /// let _ = e.with_requirement(req, Some("publish"), Some("Post#456"), None);
    /// # }
    /// ```
    #[must_use]
    pub fn with_requirement(
        mut self,
        required: Vec<PermRef>,
        action: Option<&str>,
        resource: Option<&str>,
        policy_location: Option<String>,
    ) -> Self {
        self.required = required;
        self.action = action.map(ToOwned::to_owned);
        self.resource = resource.map(ToOwned::to_owned);
        self.policy_location = policy_location;
        self
    }

    /// Render the block `moso authz explain` prints.
    ///
    /// The exact format is part of the CLI's contract and is snapshot-tested:
    ///
    /// ```text
    /// DENY  posts.publish
    ///
    ///   actor      usr_123 (alice@example.com)
    ///   roles      Editor (global), Viewer (org:acme)
    ///   perms      posts.read, posts.create, posts.update  (from Editor)
    ///   required   posts.publish
    ///   policy     Policy<Publish, Post> for Actor  src/authz/post.rs:14
    ///   reason     "not the author and not an admin"
    ///   trace
    ///     ✓ authenticated
    ///     ✗ has(posts.publish) → false
    /// ```
    ///
    /// ```
    /// use moso_authz::{ActorId, Decision, Explanation, Scope};
    ///
    /// let decision = Decision::deny("not the author and not an admin");
    /// let rendered = Explanation::of(&decision, &ActorId::new("usr_123"), &Scope::Global).render();
    ///
    /// assert!(rendered.starts_with("DENY"));
    /// assert!(rendered.contains("  reason     \"not the author and not an admin\"\n"));
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let verdict = if self.allowed { "ALLOW" } else { "DENY" };
        out.push_str(&format!("{verdict}  {}\n\n", self.subject()));

        let actor = match &self.actor_label {
            Some(label) => format!("{} ({label})", self.actor),
            None => self.actor.to_string(),
        };
        out.push_str(&row("actor", &actor));

        if !self.roles.is_empty() {
            let roles = self
                .roles
                .iter()
                .map(|(name, scope)| format!("{name} ({})", scope.as_key()))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&row("roles", &roles));
        }

        if !self.permissions.is_empty() {
            let names = join_names(&self.permissions);
            let perms = match &self.granted_by {
                Some(role) => format!("{names}  (from {role})"),
                None => names,
            };
            out.push_str(&row("perms", &perms));
        }

        if !self.required.is_empty() {
            out.push_str(&row("required", &join_names(&self.required)));
        }

        if let Some(location) = &self.policy_location {
            let signature = self.policy_signature.as_deref().unwrap_or("policy");
            out.push_str(&row("policy", &format!("{signature}  {location}")));
        }

        out.push_str(&row("reason", &format!("{:?}", self.reason)));

        if !self.trace.is_empty() {
            out.push_str("  trace\n");
            for step in &self.trace {
                out.push_str(&format!("    {}\n", step.render()));
            }
        }
        out
    }

    /// What the decision was *about*, for the header line.
    ///
    /// The permission if one was required, the action if a policy ran, the
    /// resource if neither — in that order, because that is the order of
    /// specificity a reader wants.
    ///
    /// ```
    /// use moso_authz::{ActorId, Decision, Explanation, Scope};
    ///
    /// let e = Explanation::of(&Decision::allow("ok"), &ActorId::anonymous(), &Scope::Global);
    /// assert_eq!(e.subject(), "-");
    /// ```
    #[must_use]
    pub fn subject(&self) -> String {
        if let Some(first) = self.required.first() {
            return first.name().to_owned();
        }
        if let Some(action) = &self.action {
            return action.clone();
        }
        self.resource.clone().unwrap_or_else(|| "-".to_owned())
    }

    /// Name the role the listed permissions came from.
    ///
    /// Rendered as the `(from Editor)` annotation. Set when exactly one role is
    /// held, which is when the annotation is true rather than a guess.
    ///
    /// ```
    /// use moso_authz::{ActorId, Decision, Explanation, Scope};
    ///
    /// let e = Explanation::of(&Decision::deny("no"), &ActorId::anonymous(), &Scope::Global)
    ///     .granted_by("Editor");
    /// assert_eq!(e.granted_by.as_deref(), Some("Editor"));
    /// ```
    #[must_use]
    pub fn granted_by(mut self, role: impl Into<String>) -> Self {
        self.granted_by = Some(role.into());
        self
    }

    /// Name the policy that produced the decision, as it is written in source.
    ///
    /// ```
    /// use moso_authz::{ActorId, Decision, Explanation, PolicyRef, Scope};
    ///
    /// const REF: PolicyRef = PolicyRef::typed("publish", "Publish", "Post", "src/a.rs", 14);
    /// let e = Explanation::of(&Decision::deny("no"), &ActorId::anonymous(), &Scope::Global)
    ///     .by_policy(REF);
    ///
    /// assert!(e.render().contains("Policy<Publish, Post> for Actor  src/a.rs:14"));
    /// ```
    #[must_use]
    pub fn by_policy(mut self, policy: crate::PolicyRef) -> Self {
        self.policy_signature = Some(policy.signature());
        self.policy_location = Some(policy.location());
        self.action
            .get_or_insert_with(|| policy.action().to_owned());
        self
    }
}

/// One `  label      value` line of the rendered block.
///
/// The value column is 13 characters in, which is what lines the six labels up
/// with each other. It is part of the CLI's contract and is snapshot-tested.
fn row(label: &str, value: &str) -> String {
    format!("  {label:<11}{value}\n")
}

/// Permission names, comma-separated, in registry order.
fn join_names(permissions: &[PermRef]) -> String {
    permissions
        .iter()
        .map(PermRef::name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether a request asked for an explanation, and is allowed to have one.
///
/// Both halves matter: the header is a request, and the profile is the answer.
/// A production process returns `false` however the header is set.
///
/// ```
/// use moso_authz::{explain_requested, EXPLAIN_HEADER};
///
/// let mut headers = http::HeaderMap::new();
/// headers.insert(EXPLAIN_HEADER, http::HeaderValue::from_static("1"));
///
/// assert!(explain_requested(&headers, true));
/// assert!(!explain_requested(&headers, false));
/// assert!(!explain_requested(&http::HeaderMap::new(), true));
/// ```
#[must_use]
pub fn explain_requested(headers: &http::HeaderMap, development: bool) -> bool {
    development
        && headers
            .get(EXPLAIN_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(is_truthy)
}

/// Whether a header value asks for the trace.
///
/// Generous about spelling — `1`, `true`, `yes`, `on` — and about case, because
/// this is a debugging affordance typed by hand into a `curl` command, and
/// silently ignoring `-H 'X-Moso-Authz-Explain: true'` would waste exactly the
/// time the header exists to save.
fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Perm, Role, actor};
    use crate::{Decision, PermSet, PolicyRef, ScopeId, TraceStep};

    /// Acceptance criterion 6: `moso authz explain` prints the block in
    /// `docs/03-batteries/31-authorization.md`, byte for byte.
    #[test]
    fn the_rendered_block_matches_the_documented_one() {
        let decision = Decision::deny("not the author and not an admin")
            .with_step(TraceStep::new("authenticated", true))
            .with_step(
                TraceStep::new("resource loaded", true).with_detail("post:456, author=usr_999"),
            )
            .with_step(TraceStep::new("has(posts.publish) → false", false))
            .with_step(TraceStep::new("post.author_id == actor.id → false", false))
            .with_step(TraceStep::new("has(admin.access) → false", false));

        let permissions = PermSet::of([Perm::PostsRead, Perm::PostsCreate, Perm::PostsUpdate])
            .iter()
            .map(PermRef::of)
            .collect();
        let required = vec![PermRef::of(Perm::PostsPublish)];

        let explanation = Explanation::of(&decision, &ActorId::new("usr_123"), &Scope::Global)
            .labelled("alice@example.com")
            .with_grants(
                vec![
                    ("Editor".to_owned(), Scope::Global),
                    ("Viewer".to_owned(), Scope::Org(ScopeId::new("acme"))),
                ],
                permissions,
            )
            .granted_by("Editor")
            .with_requirement(required, Some("publish"), Some("Post#456"), None)
            .by_policy(PolicyRef::typed(
                "publish",
                "Publish",
                "Post",
                "src/authz/post.rs",
                14,
            ));

        let expected = concat!(
            "DENY  posts.publish\n",
            "\n",
            "  actor      usr_123 (alice@example.com)\n",
            "  roles      Editor (global), Viewer (org:acme)\n",
            "  perms      posts.read, posts.create, posts.update  (from Editor)\n",
            "  required   posts.publish\n",
            "  policy     Policy<Publish, Post> for Actor  src/authz/post.rs:14\n",
            "  reason     \"not the author and not an admin\"\n",
            "  trace\n",
            "    ✓ authenticated\n",
            "    ✓ resource loaded (post:456, author=usr_999)\n",
            "    ✗ has(posts.publish) → false\n",
            "    ✗ post.author_id == actor.id → false\n",
            "    ✗ has(admin.access) → false\n",
        );

        assert_eq!(explanation.render(), expected);
    }

    #[test]
    fn an_allow_says_allow() {
        let explanation = Explanation::of(
            &Decision::allow("author"),
            &ActorId::new("usr_1"),
            &Scope::Global,
        );

        assert!(explanation.allowed);
        assert!(explanation.render().starts_with("ALLOW  -\n"));
    }

    #[test]
    fn the_subject_falls_back_from_permission_to_action_to_resource() {
        let decision = Decision::deny("no");
        let base = Explanation::of(&decision, &ActorId::anonymous(), &Scope::Global);

        assert_eq!(base.clone().subject(), "-");
        assert_eq!(
            base.clone()
                .with_requirement(Vec::new(), Some("publish"), Some("Post#1"), None)
                .subject(),
            "publish",
        );
        assert_eq!(
            base.clone()
                .with_requirement(Vec::new(), None, Some("Post#1"), None)
                .subject(),
            "Post#1",
        );
        assert_eq!(
            base.with_requirement(vec![PermRef::of(Perm::PostsPublish)], None, None, None)
                .subject(),
            "posts.publish",
        );
    }

    /// Acceptance criterion 6 again, from the other side: the explanation an
    /// offline `moso authz explain` renders must describe the *same* decision a
    /// live request produces, because both are built from one `Decision`.
    #[tokio::test]
    async fn the_explanation_agrees_with_the_runtime_decision() {
        use crate::fixture::{Post, Publish};

        let bob = actor("usr_2", [Role::Editor]);
        let post = Post {
            id: 456,
            author_id: "usr_999".to_owned(),
            published: false,
            title: "Draft".to_owned(),
        };

        let decision = bob.can(Publish, &post).await;
        let explanation = Explanation::of(&decision, bob.id(), bob.scope());

        assert_eq!(explanation.allowed, decision.allowed());
        assert_eq!(explanation.reason, decision.reason());
        assert_eq!(explanation.trace.len(), decision.trace().len());
        assert!(explanation.render().starts_with("DENY"));
    }

    #[test]
    fn an_explanation_round_trips_through_json() {
        let explanation = Explanation::of(
            &Decision::deny("no").with_step(TraceStep::new("x", false)),
            &ActorId::new("usr_1"),
            &Scope::Org(ScopeId::new("acme")),
        );

        let encoded = serde_json::to_string(&explanation).expect("encode");
        let decoded: Explanation = serde_json::from_str(&encoded).expect("decode");

        assert_eq!(decoded.render(), explanation.render());
    }

    // ── the header ────────────────────────────────────────────────────────

    #[test]
    fn the_header_is_only_honoured_in_development() {
        let mut headers = http::HeaderMap::new();
        headers.insert(EXPLAIN_HEADER, http::HeaderValue::from_static("1"));

        assert!(explain_requested(&headers, true));
        assert!(
            !explain_requested(&headers, false),
            "an explain trace in production describes the model to whoever asked",
        );
    }

    #[test]
    fn the_header_accepts_the_spellings_people_actually_type() {
        for value in ["1", "true", "TRUE", "yes", "on", " true "] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                EXPLAIN_HEADER,
                http::HeaderValue::from_str(value).expect("valid header"),
            );
            assert!(explain_requested(&headers, true), "`{value}` should count");
        }

        for value in ["0", "false", "no", "off", ""] {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                EXPLAIN_HEADER,
                http::HeaderValue::from_str(value).expect("valid header"),
            );
            assert!(
                !explain_requested(&headers, true),
                "`{value}` should not count",
            );
        }
    }

    #[test]
    fn an_absent_header_asks_for_nothing() {
        assert!(!explain_requested(&http::HeaderMap::new(), true));
    }
}
