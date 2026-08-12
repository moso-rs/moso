//! `Policy` and `ScopedPolicy` say the same thing, proved against a database.
//!
//! Two `impl`s written in two languages — one Rust `if`, one `WHERE` clause —
//! that nothing in the type system relates. When they drift, a list endpoint
//! hands over rows a detail endpoint refuses. That is a data leak, and it is
//! invisible to a reviewer reading the two blocks side by side, because they do
//! not look alike even when they agree.
//!
//! [`moso_authz::testing::assert_policies_agree`] is the harness that catches
//! it. This file is both its own test and the worked example an application
//! copies: a registry built through the facade macros, two policies over a real
//! entity, and the harness run over four actor shapes and six rows.
//!
//! SQLite runs always and needs nothing installed. PostgreSQL runs additionally
//! when `DATABASE_URL` is set, and prints why it skipped when it is not.

#![allow(missing_docs)]

use moso_authz::testing::{Divergence, assert_policies_agree, policy_agreement};
use moso_authz::{
    Actor, ActorId, ActorKind, Decision, Policy, PolicyCtx, RoleSet, Scope, ScopedPolicy,
};
use moso_orm::{ColumnDef, Db, DecodeError, Entity, RawQuery, Row, Select};
use moso_sql::{TableRef, ValueKind};

// ---------------------------------------------------------------------------
// The registry, through the macros an application uses
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

moso_authz::actions! {
    for Role;
    /// Listing posts.
    Read = "read",
}

// ---------------------------------------------------------------------------
// The resource
// ---------------------------------------------------------------------------

/// A post row, as `#[derive(Entity)]` would produce it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Post {
    pub id: i64,
    pub author_id: String,
    pub published: bool,
    pub title: String,
}

impl Post {
    /// `Post::AUTHOR_ID`.
    fn author_id_column() -> moso_orm::Column<Post, String> {
        moso_orm::Column::new("author_id")
    }

    /// `Post::PUBLISHED`.
    fn published_column() -> moso_orm::Column<Post, bool> {
        moso_orm::Column::new("published")
    }
}

impl Entity for Post {
    type Pk = i64;

    const TABLE: TableRef = TableRef::from_static("authz_agreement_posts");
    const COLUMNS: &'static [ColumnDef] = &[
        ColumnDef::new("id", ValueKind::I64).primary_key(),
        ColumnDef::new("author_id", ValueKind::Text),
        ColumnDef::new("published", ValueKind::Bool),
        ColumnDef::new("title", ValueKind::Text),
    ];
    const NAME: &'static str = "Post";

    fn pk(&self) -> i64 {
        self.id
    }

    fn from_row(row: &Row) -> Result<Self, DecodeError> {
        Ok(Self {
            id: row.get_i64(0)?,
            author_id: row.get_string(1)?,
            published: row.get_bool(2)?,
            title: row.get_string(3)?,
        })
    }

    fn descriptor() -> &'static moso_orm::descriptor::EntityDescriptor {
        static DESCRIPTOR: std::sync::OnceLock<moso_orm::descriptor::EntityDescriptor> =
            std::sync::OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            moso_orm::descriptor::EntityDescriptor::builder("Post", Self::TABLE).build()
        })
    }
}

// ---------------------------------------------------------------------------
// The two policies, written the way the documentation writes them
// ---------------------------------------------------------------------------

/// "A published post is readable by anybody; a draft only by its author or an
/// administrator", as one `if` chain.
impl Policy<Read, Post> for Actor<Role> {
    async fn allows(&self, _: Read, post: &Post, _ctx: &PolicyCtx) -> Decision {
        if self.has(Perm::AdminAccess) {
            return Decision::allow("admin override");
        }
        if post.published {
            return Decision::allow("published");
        }
        if post.author_id == self.id().as_str() {
            return Decision::allow("author");
        }
        Decision::deny("a draft, and not the author")
    }
}

/// The same sentence, as a `WHERE` clause. Keeping these two in step is what
/// the harness below exists to check.
impl ScopedPolicy<Read, Post> for Actor<Role> {
    fn scope_query(&self, query: Select<Post>) -> Select<Post> {
        if self.has(Perm::AdminAccess) {
            return query;
        }
        query.filter(
            Post::published_column().eq(true) | Post::author_id_column().eq(self.id().as_str()),
        )
    }
}

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// Six posts by two authors: three published, three drafts.
fn seed() -> Vec<Post> {
    vec![
        post(1, "usr_1", true, "Alice, published"),
        post(2, "usr_1", false, "Alice, draft"),
        post(3, "usr_1", true, "Alice, published again"),
        post(4, "usr_2", true, "Bob, published"),
        post(5, "usr_2", false, "Bob, draft"),
        post(6, "usr_2", true, "Bob, published again"),
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

/// The four actor *shapes* the two policies branch on: an author, a peer, an
/// administrator, and nobody. One per branch is what exercises them; every
/// actor in the database would only be slower.
fn actors() -> Vec<Actor<Role>> {
    vec![
        actor("usr_1", [Role::Viewer]),
        actor("usr_2", [Role::Viewer]),
        actor("usr_9", [Role::Admin]),
        Actor::anonymous(),
    ]
}

fn actor(id: &str, roles: impl IntoIterator<Item = Role>) -> Actor<Role> {
    Actor::new(
        ActorId::new(id),
        ActorKind::User,
        Scope::Global,
        RoleSet::of(roles),
    )
}

/// The table, on whichever dialect the handle speaks, seeded.
async fn create_table(db: &Db) {
    RawQuery::new("drop table if exists authz_agreement_posts")
        .execute(db)
        .await
        .expect("drop");
    let key = match db.backend() {
        moso_orm::Backend::Postgres => "bigint primary key",
        _ => "integer primary key",
    };
    RawQuery::new(format!(
        "create table authz_agreement_posts (
             id {key},
             author_id text not null,
             published boolean not null,
             title text not null
         )"
    ))
    .execute(db)
    .await
    .expect("create");

    for row in seed() {
        let placeholders = match db.backend() {
            moso_orm::Backend::Postgres => "$1, $2, $3, $4",
            _ => "?, ?, ?, ?",
        };
        RawQuery::new(format!(
            "insert into authz_agreement_posts (id, author_id, published, title) \
             values ({placeholders})"
        ))
        .bind(row.id)
        .bind(row.author_id)
        .bind(row.published)
        .bind(row.title)
        .execute(db)
        .await
        .expect("seed");
    }
}

/// A SQLite handle with the fixture loaded. Needs nothing installed.
async fn sqlite() -> Db {
    let db = Db::connect_url("sqlite://:memory:")
        .await
        .expect("an in-memory SQLite database");
    create_table(&db).await;
    db
}

/// A PostgreSQL handle with the fixture loaded, or `None` and a printed reason.
async fn postgres() -> Option<Db> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping the PostgreSQL half: DATABASE_URL is not set. Start the test server with \
             `scripts/test-db.sh` and re-run."
        );
        return None;
    };
    if url.is_empty() {
        return None;
    }
    let db = Db::connect_url(&url)
        .await
        .expect("the test server accepts connections");
    create_table(&db).await;
    Some(db)
}

// ---------------------------------------------------------------------------
// The claims
// ---------------------------------------------------------------------------

/// The example an application copies: one call, and a filter that has drifted
/// from its policy fails the build.
async fn the_two_policies_agree(db: &Db) {
    assert_policies_agree::<Read, Post, Role>(db, &actors(), &seed()).await;
}

/// …and the report says how much it actually compared, so the test cannot pass
/// by comparing nothing.
async fn the_agreement_covers_every_actor_and_every_row(db: &Db) {
    let report = policy_agreement::<Read, Post, Role>(db, &actors(), &seed())
        .await
        .expect("the scoped query runs");

    assert!(report.holds(), "{}", report.render());
    assert!(!report.leaks());
    assert_eq!(report.comparisons(), 24, "four actors over six rows");
    assert_eq!(report.render(), "no disagreements");
}

/// The harness has to *find* a leak, or it is only proving that it compiles.
///
/// `Sloppy` is the drift a refactor introduces: somebody widens the `WHERE`
/// clause to "everything this author can see" and forgets that the row policy
/// still refuses another author's draft.
#[derive(Clone, Copy, Debug, Default)]
struct Sloppy;

impl moso_authz::Action for Sloppy {
    const NAME: &'static str = "sloppy";
}

impl Policy<Sloppy, Post> for Actor<Role> {
    async fn allows(&self, _: Sloppy, post: &Post, _ctx: &PolicyCtx) -> Decision {
        if post.published || post.author_id == self.id().as_str() {
            return Decision::allow("published, or the author");
        }
        Decision::deny("a draft, and not the author")
    }
}

impl ScopedPolicy<Sloppy, Post> for Actor<Role> {
    fn scope_query(&self, query: Select<Post>) -> Select<Post> {
        // The drift: the author check was dropped from the filter but not from
        // the policy, so the query admits every draft.
        query
    }
}

async fn a_drifted_filter_is_reported_as_a_leak(db: &Db) {
    let report = policy_agreement::<Sloppy, Post, Role>(db, &actors(), &seed())
        .await
        .expect("the scoped query runs");

    assert!(!report.holds(), "the harness must not miss this");
    assert!(
        report.leaks(),
        "an unfiltered query over refused rows leaks"
    );
    assert!(
        report
            .disagreements()
            .iter()
            .all(|found| found.divergence() == Divergence::Leaked),
    );
    // Two drafts; each is refused to the two actors who do not own it and to
    // the anonymous one, and allowed to its own author.
    assert_eq!(report.disagreements().len(), 6, "{}", report.render());
    assert!(report.render().contains("Post#2"), "{}", report.render());
    assert!(report.render().contains("Post#5"), "{}", report.render());
}

/// Every claim on one handle: the fixture is a table with a fixed name, so two
/// tests that created it concurrently would race on the same server.
async fn the_agreement_claims(db: &Db) {
    the_two_policies_agree(db).await;
    the_agreement_covers_every_actor_and_every_row(db).await;
    a_drifted_filter_is_reported_as_a_leak(db).await;
}

#[tokio::test]
async fn a_policy_and_its_query_filter_agree_on_sqlite() {
    the_agreement_claims(&sqlite().await).await;
}

#[tokio::test]
async fn a_policy_and_its_query_filter_agree_on_postgres() {
    let Some(db) = postgres().await else { return };
    the_agreement_claims(&db).await;
    RawQuery::new("drop table if exists authz_agreement_posts")
        .execute(&db)
        .await
        .expect("the fixture table is dropped even after a failure above");
}
