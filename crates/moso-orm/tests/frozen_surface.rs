//! The invariants that must not move, asserted from outside the crate.
//!
//! A unit test proves a body; this file proves the *shape*. Every assertion
//! here corresponds to a numbered non-negotiable in
//! `docs/02-data/20-orm-overview.md` or to an acceptance criterion in
//! `21-entities-queries.md` / `22-relations.md`, and a body-filling pass that
//! breaks one has changed the contract rather than an implementation.

use std::sync::OnceLock;

use moso_orm::descriptor::{EntityDescriptor, RelationDescriptor, RelationKind};
use moso_orm::{
    Column, ColumnDef, DecodeError, Delete, Entity, NeedsTenant, Predicate, Preload, Related, Row,
    Select, TenantId, Update,
};
use moso_sql::{Expr, TableRef, ValueKind};

/// A post, with a soft-delete column and one relation.
#[derive(Clone, Debug)]
pub struct Post {
    /// The primary key.
    pub id: i64,
    /// Whether it is visible.
    pub published: bool,
    /// Who wrote it — never loaded unless the query asked.
    pub author: Related<User>,
}

impl Entity for Post {
    type Pk = i64;

    const TABLE: TableRef = TableRef::from_static("posts");
    const COLUMNS: &'static [ColumnDef] = &[
        ColumnDef::new("id", ValueKind::I64).primary_key(),
        ColumnDef::new("published", ValueKind::Bool),
        ColumnDef::new("author_id", ValueKind::I64),
    ];
    const NAME: &'static str = "Post";

    fn pk(&self) -> i64 {
        self.id
    }

    fn from_row(row: &Row) -> Result<Self, DecodeError> {
        Ok(Self {
            id: row.get_i64(0)?,
            published: row.get_bool(1)?,
            author: Related::NotLoaded,
        })
    }

    fn descriptor() -> &'static EntityDescriptor {
        static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| {
            EntityDescriptor::builder("Post", Self::TABLE)
                .soft_delete("deleted_at")
                .relation(
                    RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build(),
                )
                .build()
        })
    }
}

impl Post {
    /// The primary key.
    pub const ID: Column<Post, i64> = Column::new("id");
    /// Whether it is visible.
    pub const PUBLISHED: Column<Post, bool> = Column::new("published");
    /// The author's key, filterable without a join.
    pub const AUTHOR_ID: Column<Post, i64> = Column::new("author_id");
}

/// A user.
#[derive(Clone, Debug)]
pub struct User {
    /// The primary key.
    pub id: i64,
}

impl Entity for User {
    type Pk = i64;

    const TABLE: TableRef = TableRef::from_static("users");
    const COLUMNS: &'static [ColumnDef] = &[
        ColumnDef::new("id", ValueKind::I64).primary_key(),
        ColumnDef::new("is_admin", ValueKind::Bool),
    ];
    const NAME: &'static str = "User";

    fn pk(&self) -> i64 {
        self.id
    }

    fn from_row(row: &Row) -> Result<Self, DecodeError> {
        Ok(Self {
            id: row.get_i64(0)?,
        })
    }

    fn descriptor() -> &'static EntityDescriptor {
        static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
        DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
    }
}

impl User {
    /// Whether the user is an administrator.
    pub const IS_ADMIN: Column<User, bool> = Column::new("is_admin");
}

/// N1 — acceptance criterion 1 of `21-entities-queries.md`: `Select<Post>`
/// survives ten chained combinators with its type unchanged.
///
/// The proof is the *annotation*: if any combinator changed the shape, the
/// binding would not type-check.
#[test]
fn n1_ten_combinators_leave_the_type_alone() {
    let query: Select<Post> = Select::<Post>::new()
        .filter(Post::ID.gt(0))
        .filter_opt(Some(Post::PUBLISHED.eq(true)))
        .filter_if(true, || Post::AUTHOR_ID.eq(1))
        .when(true, |q| q.limit(20))
        .apply(|q| q.offset(0))
        .order_by(Post::ID.desc())
        .distinct()
        .with_deleted()
        .clear_order()
        .order_by(Post::ID.asc());

    assert_eq!(query.filters().len(), 3);
    assert_eq!(query.order_terms().len(), 1);

    // …and a type-equality assertion, so a future `.join()` that changes the
    // shape fails here rather than in a user's crate.
    fn same_type(_: &Select<Post>) {}
    same_type(&query);
}

/// N1 — the user-visible type stays short (diagnostics rule 2).
#[test]
fn n1_no_user_visible_type_is_long() {
    for name in [
        core::any::type_name::<Select<Post>>(),
        core::any::type_name::<Select<Post, NeedsTenant>>(),
        core::any::type_name::<Update<Post>>(),
        core::any::type_name::<Delete<Post>>(),
    ] {
        let short = name
            .replace("frozen_surface::", "")
            .replace("moso_orm::", "");
        assert!(short.len() <= 80, "{short} is {} characters", short.len());
    }
}

/// N2 — reading an unloaded relation errors and never queries.
#[test]
fn n2_an_unloaded_relation_never_queries() {
    let post = Post {
        id: 1,
        published: true,
        author: Related::NotLoaded,
    };
    // No executor is in scope, so this could not query even if it wanted to;
    // the point is that the API offers no way to make it try.
    assert!(post.author.get().is_err());
    assert!(post.author.is_not_loaded());
}

/// N3 — a preload tree costs one statement per node, whatever the row count.
#[test]
fn n3_preloads_are_one_statement_each() {
    let author = Preload::new("author", RelationKind::BelongsTo, "User");
    assert_eq!(author.statement_count(), 1);

    let nested = Preload::new("comments", RelationKind::HasMany, "Comment").with(Preload::new(
        "author",
        RelationKind::BelongsTo,
        "User",
    ));
    assert_eq!(nested.statement_count(), 2, "a two-level preload is +2");

    let query = Select::<Post>::new().with(author).with(nested);
    let total: usize = query.preloads().iter().map(Preload::statement_count).sum();
    assert_eq!(total, 3, "the base query plus these is four statements");
}

/// N4 — a conditional join is an ordinary `if`, which is the ergonomic the
/// joined-set decision was made to protect.
#[test]
fn n4_a_conditional_join_compiles() {
    let mut query = Select::<Post>::new();
    for (wanted, id) in [(true, 1_i64), (false, 2)] {
        query = query
            .filter_if(wanted, || Post::AUTHOR_ID.eq(id))
            .when(wanted, |q| q.limit(10));
    }
    assert_eq!(query.filters().len(), 1);
    assert_eq!(query.limit_value(), Some(10));
}

/// The joined-set decision, asserted end to end: filtering on an unjoined
/// entity is refused when the statement is built, with the documented message
/// and the caller's own line.
#[test]
fn the_unjoined_check_refuses_before_any_sql_is_sent() {
    let query = Select::<Post>::new().filter(User::IS_ADMIN.eq(true));

    let error = query
        .check_scope()
        .expect_err("`User` is not joined into a query over `Post`");
    let text = error.to_string();

    assert!(
        text.contains("`User` is not joined in this query"),
        "{text}"
    );
    assert!(
        text.contains("frozen_surface.rs:"),
        "the message must name the user's file: {text}"
    );
    assert!(error.is_programmer_error());

    // …and the same filter is fine on a query over `User`.
    assert!(
        Select::<User>::new()
            .filter(User::IS_ADMIN.eq(true))
            .check_scope()
            .is_ok()
    );
}

/// A predicate assembled with `&` and `|` keeps an exact entity set, so the
/// check is precise rather than best-effort.
#[test]
fn the_scope_check_survives_boolean_composition() {
    let mixed = Post::PUBLISHED.eq(true) & (User::IS_ADMIN.eq(true) | Post::ID.gt(0));
    assert_eq!(mixed.entities(), ["Post", "User"]);
    assert_eq!(mixed.missing_from(&["Post"]), Some("User"));
    assert_eq!(mixed.missing_from(&["Post", "User"]), None);
}

/// A raw expression carries no entity set, so it is never refused — the honest
/// answer for a fragment Moso cannot see inside.
#[test]
fn a_raw_expression_is_never_refused() {
    let raw: Predicate = Expr::value(true).into();
    assert!(Select::<Post>::new().filter(raw).check_scope().is_ok());
}

/// Acceptance criterion 3 of `21-entities-queries.md`: an unfiltered mass write
/// is refused, and `.all_rows()` is how you mean it.
#[test]
fn an_unfiltered_mass_write_is_refused_on_both_builders() {
    assert!(
        Update::<Post>::all()
            .set(Post::PUBLISHED, false)
            .check_guard()
            .is_err()
    );
    assert!(
        Update::<Post>::all()
            .set(Post::PUBLISHED, false)
            .all_rows()
            .check_guard()
            .is_ok()
    );
    assert!(
        Update::<Post>::all()
            .set(Post::PUBLISHED, false)
            .filter(Post::ID.gt(0))
            .check_guard()
            .is_ok()
    );

    assert!(Delete::<Post>::all().check_guard().is_err());
    assert!(Delete::<Post>::all().all_rows().check_guard().is_ok());
    assert!(Delete::<Post>::by_key(1).check_guard().is_ok());
}

/// A soft-deletable entity deletes softly unless told otherwise.
#[test]
fn a_soft_deletable_entity_deletes_softly() {
    assert!(!Delete::<Post>::by_key(1).is_hard());
    assert!(Delete::<Post>::by_key(1).hard().is_hard());
    assert!(
        Delete::<User>::by_key(1).is_hard(),
        "an entity with no soft-delete column always deletes hard"
    );
}

/// The tenant obligation is the one thing `J` encodes, and `.scoped(..)` is
/// what discharges it.
#[test]
fn a_tenant_obligation_is_discharged_by_scoping() {
    let scoped: Select<Post> = Select::<Post, NeedsTenant>::new().scoped(TenantId::of(7_i64));
    assert!(scoped.filters().is_empty());

    let across: Select<Post> = Select::<Post, NeedsTenant>::new().across_tenants();
    assert!(across.filters().is_empty());
}

/// N5 — a tuple projection's output type comes from the columns'.
#[test]
fn n5_a_tuple_projection_is_typed_by_its_columns() {
    let projected = Select::<Post>::new().select((Post::ID, Post::PUBLISHED));
    assert_eq!(projected.items().len(), 2);

    fn output_is<C: moso_orm::ColumnTuple<Output = O>, O>() {}
    output_is::<(Column<Post, i64>, Column<Post, bool>), (i64, bool)>();
}

/// N6 — the descriptor answers everything the migration generator asks.
#[test]
fn n6_the_descriptor_carries_what_migrate_needs() {
    let posts = Post::descriptor();
    assert_eq!(posts.table().name().as_str(), "posts");
    assert!(posts.is_soft_deletable());
    assert_eq!(posts.relations().len(), 1);
    assert!(posts.relation("author").is_some());
    assert_eq!(Post::primary_key_columns(), ["id"]);
}

/// A test that needs the real database, gated so the suite still passes without
/// one. Every ORM behaviour that depends on a server belongs behind this gate.
#[test]
fn postgres_is_reachable_when_configured() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping: DATABASE_URL is not set. Start the test server with \
             `scripts/test-db.sh` and re-run to exercise the PostgreSQL path."
        );
        return;
    };
    assert_eq!(
        moso_orm::Backend::from_url(&url).expect("DATABASE_URL names a supported backend"),
        moso_orm::Backend::Postgres,
        "the test database is PostgreSQL"
    );
}
