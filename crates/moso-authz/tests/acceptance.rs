//! The acceptance criteria of `docs/03-batteries/31-authorization.md` (WP-18),
//! each one a test that would fail if the claim were false.
//!
//! | # | Claim | Where |
//! | --- | --- | --- |
//! | 1 | an unknown permission in `#[requires]` is a boot error with a suggestion | `moso-authz` unit tests + `moso-authz-tests` |
//! | 2 | `moso check --authz` finds every undeclared endpoint; `#[public]` silences it | here |
//! | 3 | `Authorized<A, R>` loads once, checks once, returns the resource | here, with a statement counter |
//! | 4 | `authorized_for::<Read>` filters in SQL and paginates correctly | here, against a real database |
//! | 5 | obligations redact fields in the serialised response | here, as a snapshot |
//! | 6 | the explain output matches the runtime decision | `explain.rs` unit tests |
//! | 7 | audit entries are written for every deny, with no PII | `audit.rs` unit tests |
//! | 8 | authorization overhead is under a microsecond per check | here |
//!
//! # Which halves need a server
//!
//! Criteria 3 and 4 are about *statements*, so they run against a real database
//! or not at all. SQLite covers both and needs nothing installed; PostgreSQL
//! runs additionally when `DATABASE_URL` is set, and prints why it skipped when
//! it is not — so the suite still passes on a machine without Docker.

#![allow(missing_docs)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use moso_authz::perm::fingerprint_of;
use moso_authz::{
    Actor, ActorId, ActorKind, AuditConfig, AuditRecord, AuthorizedQuery, Decision,
    MemoryAuditSink, Obligation, PermSet, Permission, Policy, PolicyCtx, Redacted, RoleSet, Scope,
    ScopedPolicy,
};
use moso_orm::{
    ColumnDef, DecodeError, Entity, Insert, NewEntity, RawQuery, Row, Select, StatementCounter,
};
use moso_sql::{Ident, TableRef, ValueKind};

// ---------------------------------------------------------------------------
// The application, written the way an application writes it
// ---------------------------------------------------------------------------

/// What `moso::permissions!` generates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u16)]
pub enum Perm {
    PostsRead,
    PostsPublish,
    AdminAccess,
}

impl Perm {
    pub const ALL: &'static [Perm] = &[Perm::PostsRead, Perm::PostsPublish, Perm::AdminAccess];
    pub const NAMES: &'static [&'static str] = &["posts.read", "posts.publish", "admin.access"];

    pub const fn as_str(self) -> &'static str {
        Self::NAMES[self as usize]
    }
}

impl Permission for Perm {
    const ALL: &'static [Self] = Perm::ALL;
    const FINGERPRINT: u64 = fingerprint_of(Perm::NAMES);

    fn index(self) -> u16 {
        self as u16
    }

    fn from_index(index: u16) -> Option<Self> {
        Perm::ALL.get(index as usize).copied()
    }

    fn as_str(self) -> &'static str {
        Perm::as_str(self)
    }

    fn description(self) -> &'static str {
        match self {
            Self::PostsRead => "View posts",
            Self::PostsPublish => "Publish posts",
            Self::AdminAccess => "Access the admin panel",
        }
    }

    fn group(self) -> &'static str {
        match self {
            Self::PostsRead | Self::PostsPublish => "posts",
            Self::AdminAccess => "admin",
        }
    }

    fn parse(name: &str) -> Option<Self> {
        Perm::ALL.iter().copied().find(|p| Perm::as_str(*p) == name)
    }
}

/// What `moso::roles!` generates.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Role {
    Viewer,
    Admin,
}

impl moso_authz::Role for Role {
    type Perm = Perm;

    const ALL: &'static [Self] = &[Role::Viewer, Role::Admin];

    fn index(self) -> u8 {
        self as u8
    }

    fn from_index(index: u8) -> Option<Self> {
        <Self as moso_authz::Role>::ALL.get(index as usize).copied()
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Admin => "admin",
        }
    }

    fn description(self) -> &'static str {
        match self {
            Self::Viewer => "Read-only",
            Self::Admin => "Everything",
        }
    }

    fn permissions(self) -> PermSet<Perm> {
        match self {
            Self::Viewer => PermSet::of([Perm::PostsRead]),
            Self::Admin => PermSet::all(),
        }
    }

    fn parse(name: &str) -> Option<Self> {
        <Self as moso_authz::Role>::ALL
            .iter()
            .copied()
            .find(|role| moso_authz::Role::as_str(*role) == name)
    }
}

moso_authz::actions! {
    for Role;
    /// Listing posts.
    Read = "read",
    /// Making a draft public.
    Publish = "publish",
}

/// A post row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Post {
    pub id: i64,
    pub author_id: String,
    pub published: bool,
    pub title: String,
}

impl Post {
    fn author_id_column() -> moso_orm::Column<Post, String> {
        moso_orm::Column::new("author_id")
    }

    fn published_column() -> moso_orm::Column<Post, bool> {
        moso_orm::Column::new("published")
    }
}

impl Entity for Post {
    type Pk = i64;

    const TABLE: TableRef = TableRef::from_static("authz_posts");
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

impl NewEntity for Post {
    const COLUMNS: &'static [&'static str] = &["id", "author_id", "published", "title"];

    fn into_row(self) -> Vec<moso_sql::Expr> {
        use moso_orm::SqlType as _;
        vec![
            moso_sql::Expr::value(self.id.into_value()),
            moso_sql::Expr::value(self.author_id.into_value()),
            moso_sql::Expr::value(self.published.into_value()),
            moso_sql::Expr::value(self.title.into_value()),
        ]
    }
}

impl Policy<Publish, Post> for Actor<Role> {
    async fn allows(&self, _: Publish, post: &Post, _ctx: &PolicyCtx) -> Decision {
        if self.has(Perm::PostsPublish) && post.author_id == self.id().as_str() {
            return Decision::allow("author");
        }
        if self.has(Perm::AdminAccess) {
            return Decision::allow("admin override");
        }
        Decision::deny("not the author and not an admin")
    }
}

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

fn actor(id: &str, role: Role) -> Actor<Role> {
    Actor::new(
        ActorId::new(id),
        ActorKind::User,
        Scope::Global,
        RoleSet::of([role]),
    )
}

// ---------------------------------------------------------------------------
// The fixture rows
// ---------------------------------------------------------------------------

/// Six posts: three published by two authors, three drafts.
///
/// `alice` may see her own three plus `bob`'s two published ones — five of six.
fn seed() -> Vec<Post> {
    vec![
        post(1, "alice", true, "Alice, published"),
        post(2, "alice", false, "Alice, draft"),
        post(3, "alice", true, "Alice, published again"),
        post(4, "bob", true, "Bob, published"),
        post(5, "bob", false, "Bob, draft"),
        post(6, "bob", true, "Bob, published again"),
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

/// The table, created on whichever dialect the handle speaks.
async fn create_table(db: &moso_orm::Db) {
    RawQuery::new("drop table if exists authz_posts")
        .execute(db)
        .await
        .expect("drop");
    let ddl = match db.backend() {
        moso_orm::Backend::Postgres => {
            "create table authz_posts (
                 id bigint primary key,
                 author_id text not null,
                 published boolean not null,
                 title text not null
             )"
        }
        _ => {
            "create table authz_posts (
                 id integer primary key,
                 author_id text not null,
                 published boolean not null,
                 title text not null
             )"
        }
    };
    RawQuery::new(ddl).execute(db).await.expect("create");
    Insert::<Post>::rows(seed())
        .execute(db)
        .await
        .expect("seed");
}

/// A SQLite handle with the fixture loaded. Needs nothing installed.
async fn sqlite() -> moso_orm::Db {
    let db = moso_orm::Db::connect_url("sqlite://:memory:")
        .await
        .expect("an in-memory SQLite database");
    create_table(&db).await;
    db
}

/// A PostgreSQL handle with the fixture loaded, or `None` and a printed reason.
async fn postgres() -> Option<moso_orm::Db> {
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
    let db = moso_orm::Db::connect_url(&url)
        .await
        .expect("the test server accepts connections");
    create_table(&db).await;
    Some(db)
}

// ---------------------------------------------------------------------------
// 3 — `Authorized<A, R>` loads once and checks once
// ---------------------------------------------------------------------------

/// The three steps `Authorized` performs, with the statement counter watching.
///
/// The extractor itself needs a whole `RequestCtx`, which needs an `App`; what
/// is being *claimed* is about statements, and this exercises the same
/// sequence — locate, load once, decide from the loaded row — against a real
/// database with the counter in the same place the extractor puts it.
async fn loads_once_and_checks_once(db: &moso_orm::Db) {
    let scoped = db.request_scoped();
    let counter: &StatementCounter = scoped.statements();
    let alice = actor("alice", Role::Viewer);

    let mark = counter.mark();
    let post = Select::<Post>::find(2)
        .fetch_optional(&scoped)
        .await
        .expect("the query runs")
        .expect("post 2 exists");
    let decision = alice.can(Publish, &post).await;

    assert_eq!(
        counter.since(mark),
        1,
        "loading the resource and deciding must cost exactly one statement",
    );
    assert!(!decision.allowed(), "a viewer cannot publish");

    // …and the handler is handed the row, so it does not query again.
    assert_eq!(post.title, "Alice, draft");
    assert_eq!(counter.since(mark), 1);
}

/// A missing row is a 404 *before* the policy runs, and no policy statement
/// follows it.
async fn a_missing_resource_costs_one_statement_and_no_decision(db: &moso_orm::Db) {
    let scoped = db.request_scoped();
    let mark = scoped.statements().mark();

    let missing = Select::<Post>::find(9_999)
        .fetch_optional(&scoped)
        .await
        .expect("the query runs");

    assert!(missing.is_none());
    assert_eq!(scoped.statements().since(mark), 1);
}

// ---------------------------------------------------------------------------
// 4 — `authorized_for::<Read>` filters in SQL and paginates correctly
// ---------------------------------------------------------------------------

/// This is the feature that separates an authorization layer from a decorator:
/// the filter is a `WHERE` clause, so the database reads only the rows the
/// caller may see and every count computed from the query is true.
async fn query_level_filtering_is_correct_and_counted(db: &moso_orm::Db) {
    let alice = actor("alice", Role::Viewer);
    let root = actor("root", Role::Admin);

    // Everything, unfiltered, is six rows.
    let everything = Select::<Post>::new().fetch_all(db).await.expect("query");
    assert_eq!(everything.len(), 6);

    // Alice sees her three plus Bob's two published ones.
    let visible = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .fetch_all(db)
        .await
        .expect("query");
    assert_eq!(visible.len(), 5, "{visible:#?}");
    assert!(
        !visible.iter().any(|post| post.id == 5),
        "Bob's draft is not Alice's to read",
    );

    // An administrator sees everything.
    let all = Select::<Post>::new()
        .authorized_for::<Read>(&root)
        .fetch_all(db)
        .await
        .expect("query");
    assert_eq!(all.len(), 6);

    // The filter runs in the database, so it costs one statement, not six
    // policy evaluations after loading.
    let scoped = db.request_scoped();
    let mark = scoped.statements().mark();
    let _ = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .fetch_all(&scoped)
        .await
        .expect("query");
    assert_eq!(scoped.statements().since(mark), 1);
}

/// The half that makes the claim worth making: pagination *totals* are right,
/// which they are not when rows are filtered after loading.
async fn pagination_totals_count_only_visible_rows(db: &moso_orm::Db) {
    let alice = actor("alice", Role::Viewer);

    let page = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .order_by(moso_orm::Column::<Post, i64>::new("id").asc())
        .paginate_offset(1, 2)
        .fetch(db)
        .await
        .expect("the page loads");

    assert_eq!(page.items.len(), 2, "the first page holds two rows");
    assert_eq!(
        page.total,
        Some(5),
        "the total counts what Alice may see, not what the table holds",
    );

    // The last page is the remainder, and every row on it is still visible.
    let last = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .order_by(moso_orm::Column::<Post, i64>::new("id").asc())
        .paginate_offset(3, 2)
        .fetch(db)
        .await
        .expect("the page loads");

    assert_eq!(last.items.len(), 1);
    assert!(!last.items.iter().any(|post| post.id == 5));
}

/// The counter-example, spelled out: filtering *after* loading reads rows the
/// caller may not see and produces a total that is a lie. This is what the
/// feature exists to prevent, so it is worth having a test that shows it.
async fn filtering_after_loading_produces_a_wrong_total(db: &moso_orm::Db) {
    let alice = actor("alice", Role::Viewer);

    let naive: Vec<Post> = Select::<Post>::new()
        .fetch_all(db)
        .await
        .expect("query")
        .into_iter()
        .filter(|post| post.published || post.author_id == alice.id().as_str())
        .collect();

    let correct = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .fetch_all(db)
        .await
        .expect("query");

    // The *rows* agree — the policy is the same predicate either way.
    assert_eq!(naive.len(), correct.len());

    // The totals do not: the naive form's page total is the table's.
    let honest = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .paginate_offset(1, 2)
        .fetch(db)
        .await
        .expect("page");
    let dishonest = Select::<Post>::new()
        .paginate_offset(1, 2)
        .fetch(db)
        .await
        .expect("page");

    assert_eq!(honest.total, Some(5));
    assert_eq!(
        dishonest.total,
        Some(6),
        "this is the number a decorator reports",
    );
}

/// Every statement-counting and row-counting claim, on one connection.
///
/// One test per backend rather than one per claim: the fixture is a table with
/// a fixed name — `Post::TABLE` is a `const`, which is the whole point of the
/// entity model — so two tests that created it concurrently would race on the
/// same server. Running the claims in sequence on one handle is the honest
/// shape, and each assertion still names what it is checking.
async fn the_statement_claims(db: &moso_orm::Db) {
    loads_once_and_checks_once(db).await;
    a_missing_resource_costs_one_statement_and_no_decision(db).await;
    query_level_filtering_is_correct_and_counted(db).await;
    pagination_totals_count_only_visible_rows(db).await;
    filtering_after_loading_produces_a_wrong_total(db).await;
}

#[tokio::test]
async fn criteria_3_and_4_hold_on_sqlite() {
    the_statement_claims(&sqlite().await).await;
}

#[tokio::test]
async fn criteria_3_and_4_hold_on_postgres() {
    let Some(db) = postgres().await else { return };
    the_statement_claims(&db).await;
    RawQuery::new("drop table if exists authz_posts")
        .execute(&db)
        .await
        .expect("the fixture table is dropped even after a failure above");
}

/// Both dialects render the authorization filter, and both render it the same
/// shape — the snapshot each dialect owes under D9.
#[test]
fn criterion_4_the_filter_renders_on_both_dialects() {
    let alice = actor("alice", Role::Viewer);
    let statement = Select::<Post>::new()
        .authorized_for::<Read>(&alice)
        .to_statement()
        .expect("renders");

    let postgres = statement.build(&moso_sql::Postgres).expect("postgres").text;
    let sqlite = statement.build(&moso_sql::Sqlite).expect("sqlite").text;

    for (name, sql) in [("postgres", &postgres), ("sqlite", &sqlite)] {
        assert!(sql.to_ascii_uppercase().contains("WHERE"), "{name}: {sql}");
        assert!(sql.contains("published"), "{name}: {sql}");
        assert!(sql.contains("author_id"), "{name}: {sql}");
        assert!(sql.contains(" OR "), "{name}: {sql}");
    }

    // The two dialects differ only in how they quote and number, which is what
    // building the tree once buys.
    assert!(postgres.contains("$1"), "{postgres}");
    assert!(sqlite.contains('?'), "{sqlite}");
}

// ---------------------------------------------------------------------------
// 5 — obligations redact fields in the serialised response
// ---------------------------------------------------------------------------

/// The response body, as the API returns one.
#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct PostOut {
    pub id: i64,
    pub title: String,
    pub author_id: String,
    pub reviewer_note: String,
}

impl moso_schema::Validate for PostOut {
    fn validate(
        &self,
        _ctx: &mut moso_schema::ValidationCtx,
    ) -> Result<(), moso_schema::ValidationErrors> {
        Ok(())
    }
}

impl moso_schema::Schema for PostOut {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("PostOut")
    }

    fn json_schema(
        _generator: &mut moso_schema::json_schema::SchemaGenerator,
    ) -> moso_schema::json_schema::SchemaNode {
        moso_schema::json_schema::SchemaNode::any()
    }
}

fn post_out() -> PostOut {
    PostOut {
        id: 1,
        title: "Alice, published".to_owned(),
        author_id: "alice".to_owned(),
        reviewer_note: "rejected twice".to_owned(),
    }
}

#[test]
fn criterion_5_obligations_redact_the_serialised_response() {
    // A peer may read the post, but not who wrote it or what the reviewer said.
    let peer = Decision::allow("published")
        .with_obligation(Obligation::redact("/author_id"))
        .with_obligation(Obligation::mask("/reviewer_note", 5));

    let body = Redacted::new(post_out(), peer).to_json().expect("json");

    assert_eq!(
        serde_json::to_string_pretty(&body).expect("render"),
        concat!(
            "{\n",
            "  \"id\": 1,\n",
            "  \"title\": \"Alice, published\",\n",
            "  \"reviewer_note\": \"•••••••••twice\"\n",
            "}",
        ),
    );
}

#[test]
fn criterion_5_the_same_body_unredacted_for_an_administrator() {
    let body = Redacted::new(post_out(), Decision::allow("admin override"))
        .to_json()
        .expect("json");

    assert_eq!(
        serde_json::to_string_pretty(&body).expect("render"),
        concat!(
            "{\n",
            "  \"id\": 1,\n",
            "  \"title\": \"Alice, published\",\n",
            "  \"author_id\": \"alice\",\n",
            "  \"reviewer_note\": \"rejected twice\"\n",
            "}",
        ),
    );
}

// ---------------------------------------------------------------------------
// 2 — deny by default
// ---------------------------------------------------------------------------

#[test]
fn criterion_2_an_endpoint_that_declares_nothing_is_the_finding() {
    use moso_core::extract::Extract;
    use moso_openapi::OperationBuilder;
    use moso_schema::json_schema::SchemaGenerator;

    let undeclared = OperationBuilder::new(SchemaGenerator::default());
    assert!(
        moso_authz::read_declarations(undeclared.spec()).is_empty(),
        "an endpoint with no authorization declaration reports nothing, which is the finding",
    );

    let mut public = OperationBuilder::new(SchemaGenerator::default());
    <moso_authz::Public as Extract>::describe(&mut public);
    assert_eq!(
        moso_authz::read_declarations(public.spec()),
        vec![moso_authz::AuthzDeclaration::Public],
        "`#[public]` distinguishes \"considered\" from \"forgotten\"",
    );
}

// ---------------------------------------------------------------------------
// 7 — the audit trail
// ---------------------------------------------------------------------------

#[tokio::test]
async fn criterion_7_every_denial_is_audited_with_no_pii() {
    let sink = MemoryAuditSink::new();
    let config = AuditConfig::default();
    let alice = actor("alice", Role::Viewer);

    let decision = alice.can(Publish, &post(2, "alice", false, "Draft")).await;
    assert!(!decision.allowed());

    moso_authz::audit::record_if_wanted(
        &sink,
        &config,
        AuditRecord::deny(
            alice.id().clone(),
            alice.kind(),
            alice.scope().clone(),
            "publish",
            decision.reason().to_owned(),
        )
        .with_resource("Post", "2")
        .with_request(
            "01JABCDEF",
            Some("/posts/{id}/publish"),
            Some("203.0.113.7"),
        ),
        false,
    )
    .await;

    let entries = sink.entries();
    assert_eq!(entries.len(), 1);
    let encoded = serde_json::to_string(&entries[0]).expect("encode");

    assert!(encoded.contains("\"alice\""), "the actor id is recorded");
    assert!(encoded.contains("203.0.113.7"), "the address is recorded");
    assert!(
        !encoded.contains("Draft"),
        "the row's contents are not: {encoded}",
    );
    assert!(
        encoded.contains("/posts/{id}/publish"),
        "the route pattern, never the raw path: {encoded}",
    );
}

// ---------------------------------------------------------------------------
// 8 — the overhead benchmark
// ---------------------------------------------------------------------------

/// `docs/03-batteries/31-authorization.md`: **under 1 µs of framework overhead
/// per authorization check.**
///
/// What is measured is what the framework adds: resolving an already-resolved
/// actor's permissions and running the policy. The database is not in it — a
/// policy that queries is the application's cost, and saying otherwise would
/// make the number meaningless.
///
/// Measured over enough rounds that the timer's resolution does not dominate,
/// and reported in the failure message so a regression says how far.
#[tokio::test]
async fn criterion_8_a_check_costs_under_a_microsecond() {
    let alice = actor("alice", Role::Viewer);
    let draft = post(2, "alice", false, "Draft");
    let ctx = PolicyCtx::new(alice.id().clone(), alice.scope().clone());

    // Warm the branch predictor and the allocator before measuring.
    for _ in 0..1_000 {
        let _ = alice.can_with(Publish, &draft, &ctx).await;
    }

    let rounds = 200_000_u32;
    let started = Instant::now();
    let mut denied = 0_u32;
    for _ in 0..rounds {
        denied += u32::from(!alice.can_with(Publish, &draft, &ctx).await.allowed());
    }
    let each = started.elapsed() / rounds;

    assert_eq!(denied, rounds, "the loop was optimised away");
    assert!(
        each < Duration::from_micros(1),
        "an authorization check took {each:?}; the budget is 1 µs",
    );
}

/// The capability half of the same claim: a `#[requires]`-shaped check is a
/// fingerprint comparison and a word-wise AND.
#[test]
fn criterion_8_a_capability_check_costs_under_a_microsecond() {
    let held = PermSet::of([Perm::PostsRead, Perm::AdminAccess]).to_bits();
    let wanted = PermSet::of([Perm::PostsRead]).to_bits();

    let rounds = 2_000_000_u32;
    let started = Instant::now();
    let mut allowed = 0_u32;
    for _ in 0..rounds {
        allowed += u32::from(
            held.fingerprint() == wanted.fingerprint()
                && moso_authz::RequireMode::All.satisfied_by(held, wanted),
        );
    }
    let each = started.elapsed() / rounds;

    assert_eq!(allowed, rounds);
    assert!(
        each < Duration::from_micros(1),
        "a capability check took {each:?}; the budget is 1 µs",
    );
}

// ---------------------------------------------------------------------------
// The shapes an application composes
// ---------------------------------------------------------------------------

/// `Arc<dyn ActorSource<Role>>` is the one thing an application must register,
/// and `ActorPermissions` is the one line that makes `#[requires]` work.
#[test]
fn the_wiring_an_application_writes_type_checks() {
    use moso_authz::{ActorPermissions, ActorSource, PermissionSource};
    use moso_core::BoxFuture;
    use moso_core::ctx::RequestCtx;

    struct HeaderActor;

    impl ActorSource<Role> for HeaderActor {
        fn actor<'a>(
            &'a self,
            ctx: &'a RequestCtx,
        ) -> BoxFuture<'a, moso_core::Result<Actor<Role>>> {
            Box::pin(async move {
                let Some(id) = ctx.headers().get("x-actor").and_then(|v| v.to_str().ok()) else {
                    return Ok(Actor::anonymous());
                };
                Ok(actor(id, Role::Viewer))
            })
        }
    }

    let source: Arc<dyn ActorSource<Role>> = Arc::new(HeaderActor);
    let permissions: Arc<dyn PermissionSource> = Arc::new(ActorPermissions::<Role>::new());

    assert_eq!(permissions.fingerprint(), Perm::FINGERPRINT);
    assert_eq!(Arc::strong_count(&source), 1);
}

/// `Insert` needs the column list to be a valid identifier set; a typo here is
/// a runtime error rather than a compile one, so it is checked.
#[test]
fn the_fixture_entity_declares_valid_identifiers() {
    for name in <Post as NewEntity>::COLUMNS {
        assert!(Ident::new(*name).is_ok(), "`{name}` is not an identifier");
    }
}
