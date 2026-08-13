//! Proving that a [`Policy`] and its [`ScopedPolicy`] agree.
//!
//! # Why this module exists
//!
//! `Policy<Read, Post>` answers "may this actor read *this* row"; `ScopedPolicy<Read,
//! Post>` answers "which rows may this actor read" by contributing a `WHERE`
//! clause. They are two `impl`s of two traits, written by hand, and nothing in
//! the type system relates them. When they drift, one of two things happens:
//!
//! | Drift | What it is |
//! | --- | --- |
//! | The filter admits a row the policy denies | a **data leak** — a list endpoint hands over rows a detail endpoint refuses |
//! | The filter hides a row the policy allows | a missing row, reported as a bug nobody can reproduce |
//!
//! The first is why this is the highest-value test in the crate. It cannot be
//! caught by reading the two `impl`s side by side, because the whole point of
//! `scope_query` is that it is written in a different language from `allows`.
//!
//! # How it is checked
//!
//! Against a real database, because a filter is SQL and a mocked data layer
//! proves nothing about SQL. For each actor, the harness runs the query the
//! scoped policy builds and runs the row policy over every row it was given,
//! then compares the two sets of primary keys:
//!
//! ```text
//! admitted by the WHERE clause ─┐
//!                               ├─ in both      → agreement
//!                               ├─ query only   → LEAKED   (the data leak)
//! allowed by `allows` ──────────┘  policy only  → HIDDEN   (the missing row)
//! ```
//!
//! # Using it on your own policies
//!
//! ```text
//! #[tokio::test]
//! async fn the_read_policy_and_its_filter_agree() {
//!     let Some(db) = test_database().await else { return };   // skip without one
//!     let rows = seed(&db).await;
//!
//!     moso_authz::testing::assert_policies_agree::<Read, Post, Role>(
//!         &db,
//!         &[alice(), bob(), an_admin(), Actor::anonymous()],
//!         &rows,
//!     )
//!     .await;
//! }
//! ```
//!
//! Give it the actors whose *shapes* differ — an owner, a peer, an
//! administrator, nobody — rather than every actor in your database. The
//! disagreements this finds are about branches in the two `impl`s, and one actor
//! per branch is what exercises them.

use std::collections::BTreeSet;

use moso_orm::{Db, Entity, Select};

use crate::{Action, Actor, ActorId, AuthorizedQuery, Policy, PolicyCtx, Role, ScopedPolicy};

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// Which way a [`Policy`] and a [`ScopedPolicy`] disagreed about one row.
///
/// ```
/// use moso_authz::testing::Divergence;
///
/// assert!(Divergence::Leaked.is_leak());
/// assert!(!Divergence::Hidden.is_leak());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum Divergence {
    /// The query filter admitted a row the row policy refuses.
    ///
    /// A data leak: a list endpoint hands the caller a row the detail endpoint
    /// would 403 on.
    Leaked,
    /// The query filter hid a row the row policy allows.
    ///
    /// Not a leak, but a bug: the caller may open the row by its identifier and
    /// cannot find it in any list.
    Hidden,
}

impl Divergence {
    /// Whether this is the direction that leaks data.
    ///
    /// ```
    /// use moso_authz::testing::Divergence;
    ///
    /// assert!(Divergence::Leaked.is_leak());
    /// ```
    #[must_use]
    pub const fn is_leak(self) -> bool {
        matches!(self, Self::Leaked)
    }

    /// The word a report prints.
    ///
    /// ```
    /// use moso_authz::testing::Divergence;
    ///
    /// assert_eq!(Divergence::Hidden.as_str(), "HIDDEN");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leaked => "LEAKED",
            Self::Hidden => "HIDDEN",
        }
    }
}

/// One row the two policies answered differently about.
///
/// ```
/// use moso_authz::testing::{Disagreement, Divergence};
/// use moso_authz::ActorId;
///
/// let found = Disagreement::new(ActorId::new("usr_1"), "Post#5", Divergence::Leaked);
/// assert!(found.render().starts_with("LEAKED"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Disagreement {
    /// Who the two policies were asked about.
    actor: ActorId,
    /// The row, as `Name#key`.
    resource: String,
    /// Which way they disagreed.
    divergence: Divergence,
}

impl Disagreement {
    /// Record a disagreement.
    ///
    /// ```
    /// use moso_authz::testing::{Disagreement, Divergence};
    /// use moso_authz::ActorId;
    ///
    /// let _ = Disagreement::new(ActorId::anonymous(), "Post#1", Divergence::Hidden);
    /// ```
    #[must_use]
    pub fn new(actor: ActorId, resource: impl Into<String>, divergence: Divergence) -> Self {
        Self {
            actor,
            resource: resource.into(),
            divergence,
        }
    }

    /// Who.
    ///
    /// ```
    /// # use moso_authz::testing::Disagreement;
    /// # use moso_authz::ActorId;
    /// # fn f(d: &Disagreement) { let _: &ActorId = d.actor(); }
    /// ```
    #[must_use]
    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// Which row.
    ///
    /// ```
    /// # use moso_authz::testing::Disagreement;
    /// # fn f(d: &Disagreement) { let _: &str = d.resource(); }
    /// ```
    #[must_use]
    pub fn resource(&self) -> &str {
        &self.resource
    }

    /// Which way.
    ///
    /// ```
    /// # use moso_authz::testing::{Disagreement, Divergence};
    /// # fn f(d: &Disagreement) { let _: Divergence = d.divergence(); }
    /// ```
    #[must_use]
    pub fn divergence(&self) -> Divergence {
        self.divergence
    }

    /// One line naming the direction, the actor and the row.
    ///
    /// ```
    /// use moso_authz::testing::{Disagreement, Divergence};
    /// use moso_authz::ActorId;
    ///
    /// let found = Disagreement::new(ActorId::new("usr_1"), "Post#5", Divergence::Leaked);
    /// assert_eq!(
    ///     found.render(),
    ///     "LEAKED  usr_1  Post#5  — the query admits it, the policy refuses it",
    /// );
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        let explanation = match self.divergence {
            Divergence::Leaked => "the query admits it, the policy refuses it",
            Divergence::Hidden => "the policy allows it, the query hides it",
        };
        format!(
            "{}  {}  {}  — {explanation}",
            self.divergence.as_str(),
            self.actor,
            self.resource,
        )
    }
}

/// What [`policy_agreement`] found.
///
/// ```
/// use moso_authz::testing::Agreement;
///
/// let clean = Agreement::default();
/// assert!(clean.holds());
/// assert_eq!(clean.render(), "no disagreements");
/// ```
#[derive(Clone, Debug, Default)]
pub struct Agreement {
    /// How many (actor, row) pairs were compared.
    comparisons: usize,
    /// Every pair the two policies answered differently about.
    disagreements: Vec<Disagreement>,
}

impl Agreement {
    /// Whether the two policies agreed about every row for every actor.
    ///
    /// ```
    /// use moso_authz::testing::Agreement;
    ///
    /// assert!(Agreement::default().holds());
    /// ```
    #[must_use]
    pub fn holds(&self) -> bool {
        self.disagreements.is_empty()
    }

    /// Whether any disagreement is the direction that leaks data.
    ///
    /// Worth branching on: a `HIDDEN` row is a bug, a `LEAKED` row is an
    /// incident.
    ///
    /// ```
    /// use moso_authz::testing::Agreement;
    ///
    /// assert!(!Agreement::default().leaks());
    /// ```
    #[must_use]
    pub fn leaks(&self) -> bool {
        self.disagreements
            .iter()
            .any(|found| found.divergence.is_leak())
    }

    /// Every disagreement, actor by actor and row by row.
    ///
    /// ```
    /// use moso_authz::testing::{Agreement, Disagreement};
    ///
    /// let report = Agreement::default();
    /// let found: &[Disagreement] = report.disagreements();
    ///
    /// assert!(found.is_empty());
    /// ```
    #[must_use]
    pub fn disagreements(&self) -> &[Disagreement] {
        &self.disagreements
    }

    /// How many (actor, row) pairs were compared.
    ///
    /// A zero here means the harness proved nothing, which is worth asserting
    /// on: an empty actor list or an empty row list passes vacuously.
    ///
    /// ```
    /// use moso_authz::testing::Agreement;
    ///
    /// assert_eq!(Agreement::default().comparisons(), 0);
    /// ```
    #[must_use]
    pub fn comparisons(&self) -> usize {
        self.comparisons
    }

    /// The report, one disagreement per line.
    ///
    /// ```
    /// use moso_authz::testing::{Agreement, Disagreement, Divergence};
    /// use moso_authz::ActorId;
    ///
    /// let mut report = Agreement::default();
    /// report.push(Disagreement::new(ActorId::new("usr_1"), "Post#5", Divergence::Leaked));
    ///
    /// assert!(report.render().contains("Post#5"));
    /// assert!(!report.holds());
    /// ```
    #[must_use]
    pub fn render(&self) -> String {
        if self.disagreements.is_empty() {
            return "no disagreements".to_owned();
        }
        self.disagreements
            .iter()
            .map(Disagreement::render)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Add a disagreement.
    ///
    /// Public so a harness of your own — one that walks a different set of
    /// rows, or asks a second question about each — reports in the same shape.
    ///
    /// ```
    /// use moso_authz::testing::{Agreement, Disagreement, Divergence};
    /// use moso_authz::ActorId;
    ///
    /// let mut report = Agreement::default();
    /// report.push(Disagreement::new(ActorId::anonymous(), "Post#1", Divergence::Hidden));
    ///
    /// assert_eq!(report.disagreements().len(), 1);
    /// ```
    pub fn push(&mut self, disagreement: Disagreement) {
        self.disagreements.push(disagreement);
    }
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Compare what `ScopedPolicy<A, R>` admits against what `Policy<A, R>` permits.
///
/// `rows` must already be in the table — the harness reads through the database
/// so that the `WHERE` clause is the one the database actually ran, and it needs
/// the values in hand to ask the row policy about them.
///
/// # Errors
///
/// Anything the database reports while running the scoped query.
///
/// # What a clean result proves
///
/// That for every actor given and every row given, the filter and the policy
/// answer the same. It does not prove anything about a row or an actor shape you
/// did not supply — which is why [`Agreement::comparisons`] is reported, so a
/// test can refuse to pass vacuously.
///
/// ```no_run
/// // Needs a live database, an `Entity`, and both policy impls, which is more
/// // registry than a doctest should carry. The runnable form is
/// // `crates/moso-authz-tests/tests/agreement.rs`.
/// # use moso_authz::testing::policy_agreement;
/// # use moso_authz::{Action, Actor, Policy, Role, ScopedPolicy};
/// # use moso_orm::{Db, Entity};
/// # async fn check<A, R, Rl>(db: &Db, actors: &[Actor<Rl>], rows: &[R])
/// # where
/// #     A: Action,
/// #     R: Entity + Sync,
/// #     R::Pk: PartialEq + core::fmt::Debug,
/// #     Rl: Role,
/// #     Actor<Rl>: Policy<A, R> + ScopedPolicy<A, R>,
/// # {
/// let report = policy_agreement::<A, R, Rl>(db, actors, rows).await.expect("the query runs");
/// assert!(report.holds(), "{}", report.render());
/// # }
/// ```
pub async fn policy_agreement<A, R, Rl>(
    db: &Db,
    actors: &[Actor<Rl>],
    rows: &[R],
) -> moso_orm::Result<Agreement>
where
    A: Action,
    R: Entity + Sync,
    R::Pk: PartialEq + core::fmt::Debug,
    Rl: Role,
    Actor<Rl>: Policy<A, R> + ScopedPolicy<A, R>,
{
    let mut report = Agreement::default();
    for actor in actors {
        let admitted = admitted_keys::<A, R, Rl>(db, actor).await?;
        let ctx = PolicyCtx::new(actor.id().clone(), actor.scope().clone());

        for row in rows {
            report.comparisons += 1;
            let key = describe_key::<R>(&row.pk());
            let permitted = actor.allows(A::default(), row, &ctx).await.allowed();

            let divergence = match (admitted.contains(&key), permitted) {
                (true, false) => Divergence::Leaked,
                (false, true) => Divergence::Hidden,
                _ => continue,
            };
            report.push(Disagreement::new(
                actor.id().clone(),
                format!("{}#{key}", R::NAME),
                divergence,
            ));
        }
    }
    Ok(report)
}

/// [`policy_agreement`], asserted.
///
/// The form a test writes. On a disagreement it panics with the *whole* report,
/// actor by actor and row by row, because a message naming one row out of six
/// sends the reader looking for the other five. A database error panics too,
/// with a message that says so — a harness that returned `Err` from a
/// `#[tokio::test]` would report "the database is down" as a policy failure, and
/// the two need different responses.
///
/// # Panics
///
/// If the query cannot run, if the two policies disagree about any row, or if
/// nothing was compared — an empty actor or row list passes vacuously
/// otherwise, which is the one way this harness could lie.
///
/// ```no_run
/// // Same reason as `policy_agreement`: this needs a live database and both
/// // policy impls. `crates/moso-authz-tests/tests/agreement.rs` runs it.
/// # use moso_authz::testing::assert_policies_agree;
/// # use moso_authz::{Action, Actor, Policy, Role, ScopedPolicy};
/// # use moso_orm::{Db, Entity};
/// # async fn check<A, R, Rl>(db: &Db, actors: &[Actor<Rl>], rows: &[R])
/// # where
/// #     A: Action,
/// #     R: Entity + Sync,
/// #     R::Pk: PartialEq + core::fmt::Debug,
/// #     Rl: Role,
/// #     Actor<Rl>: Policy<A, R> + ScopedPolicy<A, R>,
/// # {
/// assert_policies_agree::<A, R, Rl>(db, actors, rows).await;
/// # }
/// ```
pub async fn assert_policies_agree<A, R, Rl>(db: &Db, actors: &[Actor<Rl>], rows: &[R])
where
    A: Action,
    R: Entity + Sync,
    R::Pk: PartialEq + core::fmt::Debug,
    Rl: Role,
    Actor<Rl>: Policy<A, R> + ScopedPolicy<A, R>,
{
    let report = policy_agreement::<A, R, Rl>(db, actors, rows)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "the scoped query for `{}` could not be run: {error}",
                R::NAME
            )
        });

    assert!(
        report.comparisons() > 0,
        "`assert_policies_agree::<_, {}, _>` compared nothing: give it at least one actor and \
         one row, or it passes without proving anything",
        R::NAME,
    );
    assert!(
        report.holds(),
        "`Policy<{}, {}>` and `ScopedPolicy<{}, {}>` disagree:\n{}",
        A::NAME,
        R::NAME,
        A::NAME,
        R::NAME,
        report.render(),
    );
}

/// The primary keys the scoped query admits for `actor`.
///
/// Rendered as text and collected into a set: `Entity::Pk` is only guaranteed to
/// be [`SqlType`](moso_orm::SqlType) plus `Clone`, so it is neither `Ord` nor
/// `Hash` and cannot key a set itself. The rendering is
/// [`Debug`](core::fmt::Debug), which is injective enough for keys — two rows
/// whose keys debug-print identically are the same row for every key type an
/// entity can have.
async fn admitted_keys<A, R, Rl>(db: &Db, actor: &Actor<Rl>) -> moso_orm::Result<BTreeSet<String>>
where
    A: Action,
    R: Entity,
    R::Pk: core::fmt::Debug,
    Rl: Role,
    Actor<Rl>: ScopedPolicy<A, R>,
{
    let admitted = Select::<R>::new()
        .authorized_for::<A>(actor)
        .unlimited()
        .fetch_all(db)
        .await?;
    Ok(admitted
        .iter()
        .map(|row| describe_key::<R>(&row.pk()))
        .collect())
}

/// One primary key, as text — the set key *and* what the report prints, so the
/// two cannot name the same row differently.
fn describe_key<R: Entity>(key: &R::Pk) -> String
where
    R::Pk: core::fmt::Debug,
{
    format!("{key:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture::{Post, Read, Role, actor};

    /// The rows the fixture's `ScopedPolicy<Read, Post>` and
    /// `Policy<Read, Post>` are both written against.
    fn seed() -> Vec<Post> {
        vec![
            post(1, "usr_1", true, "Alice, published"),
            post(2, "usr_1", false, "Alice, draft"),
            post(3, "usr_2", true, "Bob, published"),
            post(4, "usr_2", false, "Bob, draft"),
        ]
    }

    fn post(id: i64, author: &str, published: bool, title: &str) -> Post {
        Post {
            id,
            author_id: author.to_owned(),
            published,
            title: title.to_owned(),
        }
    }

    /// A SQLite handle with the fixture loaded. Needs nothing installed, which
    /// is what keeps this running on a machine with no Docker.
    async fn sqlite() -> moso_orm::Db {
        let db = moso_orm::Db::connect_url("sqlite://:memory:")
            .await
            .expect("an in-memory SQLite database");
        moso_orm::RawQuery::new(
            "create table posts (
                 id integer primary key,
                 author_id text not null,
                 published boolean not null,
                 title text not null
             )",
        )
        .execute(&db)
        .await
        .expect("the table is created");

        for row in seed() {
            moso_orm::RawQuery::new(
                "insert into posts (id, author_id, published, title) values (?, ?, ?, ?)",
            )
            .bind(row.id)
            .bind(row.author_id)
            .bind(row.published)
            .bind(row.title)
            .execute(&db)
            .await
            .expect("the row is seeded");
        }
        db
    }

    /// The four actor shapes the fixture's two `Read` policies branch on: an
    /// author, a peer, an administrator and nobody.
    fn actors() -> Vec<Actor<Role>> {
        vec![
            actor("usr_1", [Role::Viewer]),
            actor("usr_2", [Role::Viewer]),
            actor("usr_9", [Role::Owner]),
            Actor::anonymous(),
        ]
    }

    #[tokio::test]
    async fn the_fixture_s_read_policy_and_its_filter_admit_the_same_rows() {
        let db = sqlite().await;
        let report = policy_agreement::<Read, Post, Role>(&db, &actors(), &seed())
            .await
            .expect("the query runs");

        assert!(report.holds(), "{}", report.render());
        assert!(!report.leaks());
        assert_eq!(report.comparisons(), 16, "four actors over four rows");
    }

    #[tokio::test]
    async fn the_assertion_form_passes_on_a_policy_pair_that_agrees() {
        let db = sqlite().await;
        assert_policies_agree::<Read, Post, Role>(&db, &actors(), &seed()).await;
    }

    /// The harness has to *find* a leak, or it is only proving that it compiles.
    ///
    /// `Leaky` is a scoped policy that returns the query unfiltered while the
    /// row policy still refuses a draft to a stranger — the exact drift a
    /// refactor introduces when somebody "simplifies" a `WHERE` clause.
    #[tokio::test]
    async fn a_filter_wider_than_its_policy_is_reported_as_a_leak() {
        use crate::Decision;

        #[derive(Clone, Copy, Debug, Default)]
        struct Leaky;

        impl Action for Leaky {
            const NAME: &'static str = "leaky";
        }

        impl Policy<Leaky, Post> for Actor<Role> {
            async fn allows(&self, _: Leaky, post: &Post, _ctx: &PolicyCtx) -> Decision {
                if post.published {
                    return Decision::allow("published");
                }
                Decision::deny("a draft, and this policy has no owner rule")
            }
        }

        impl ScopedPolicy<Leaky, Post> for Actor<Role> {
            fn scope_query(&self, query: Select<Post>) -> Select<Post> {
                // The drift: no filter at all.
                query
            }
        }

        let db = sqlite().await;
        let report = policy_agreement::<Leaky, Post, Role>(&db, &actors(), &seed())
            .await
            .expect("the query runs");

        assert!(!report.holds());
        assert!(
            report.leaks(),
            "an unfiltered query over refused rows leaks"
        );
        assert_eq!(
            report.disagreements().len(),
            8,
            "two drafts, four actors: {}",
            report.render(),
        );
        assert!(
            report
                .disagreements()
                .iter()
                .all(|found| found.divergence() == Divergence::Leaked)
        );
        assert!(report.render().contains("Post#2"), "{}", report.render());
    }

    /// The other direction: a filter narrower than its policy hides rows.
    #[tokio::test]
    async fn a_filter_narrower_than_its_policy_is_reported_as_hidden() {
        use crate::Decision;

        #[derive(Clone, Copy, Debug, Default)]
        struct Narrow;

        impl Action for Narrow {
            const NAME: &'static str = "narrow";
        }

        impl Policy<Narrow, Post> for Actor<Role> {
            async fn allows(&self, _: Narrow, _post: &Post, _ctx: &PolicyCtx) -> Decision {
                Decision::allow("everybody may read everything")
            }
        }

        impl ScopedPolicy<Narrow, Post> for Actor<Role> {
            fn scope_query(&self, query: Select<Post>) -> Select<Post> {
                query.filter(Post::published().eq(true))
            }
        }

        let db = sqlite().await;
        let report = policy_agreement::<Narrow, Post, Role>(&db, &actors(), &seed())
            .await
            .expect("the query runs");

        assert!(!report.holds());
        assert!(!report.leaks(), "hiding a row is a bug, not an incident");
        assert_eq!(report.disagreements().len(), 8, "two drafts, four actors");
        assert!(
            report
                .disagreements()
                .iter()
                .all(|found| found.divergence() == Divergence::Hidden)
        );
    }

    #[test]
    fn a_report_renders_the_direction_the_actor_and_the_row() {
        let mut report = Agreement::default();
        report.push(Disagreement::new(
            ActorId::new("usr_1"),
            "Post#5",
            Divergence::Leaked,
        ));

        assert_eq!(
            report.render(),
            "LEAKED  usr_1  Post#5  — the query admits it, the policy refuses it",
        );
        assert_eq!(report.disagreements()[0].actor().as_str(), "usr_1");
        assert_eq!(report.disagreements()[0].resource(), "Post#5");
        assert!(report.leaks());
    }

    #[test]
    fn an_empty_report_says_so() {
        let report = Agreement::default();

        assert!(report.holds());
        assert!(!report.leaks());
        assert_eq!(report.render(), "no disagreements");
        assert_eq!(report.comparisons(), 0);
        assert_eq!(Divergence::Leaked.as_str(), "LEAKED");
        assert!(!Divergence::Hidden.is_leak());
    }
}
