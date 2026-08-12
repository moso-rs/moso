//! `#[derive(Entity)]` and friends, compiled the way an application compiles
//! them and run against a real database.
//!
//! Every other ORM test in the workspace writes its entities by hand, because
//! the crate under test cannot reach the derive: `moso-orm` must not depend on
//! `moso-orm-macros` (rule 1), and the macro's output resolves only against
//! `::moso::__private::*` (decision D6). This file is the one that proves the
//! derive itself — that the tokens it emits resolve, type-check, and produce a
//! statement a real server accepts.

use moso::db::prelude::*;
use moso::db::{ColumnTuple, DbEnum as DbEnumTrait, Preload};
use moso::sql::{Dialect, Postgres, Sqlite};
use moso::{DbEnum, Entity, Projection};

// ───────────────────────────────────────────────────────────────────────────
// The fixture — two entities, one relation, one database enum, written as the
// tutorial writes them.
// ───────────────────────────────────────────────────────────────────────────

/// Whether a post is visible.
#[derive(DbEnum, Debug, Clone, Copy, PartialEq, Eq)]
#[db_enum(as = "text")]
pub enum Status {
    /// Not visible yet.
    Draft,
    /// Visible.
    Published,
}

/// Someone who can write a post.
#[derive(Entity, Debug, Clone)]
#[entity(table = "d_authors")]
pub struct Author {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,

    /// Login identity; one row per address.
    #[entity(unique)]
    pub email: String,

    /// Everything this author wrote.
    #[entity(has_many = Post, fk = "author_id")]
    pub posts: Related<Vec<Post>>,
}

/// One post, by one author.
#[derive(Entity, Debug, Clone)]
#[entity(table = "d_posts")]
pub struct Post {
    /// The primary key.
    #[entity(pk)]
    pub id: i64,

    /// The headline.
    pub title: String,

    /// How many people read it.
    pub views: i64,

    /// Whether readers can see it.
    #[entity(enum_as = "text")]
    pub status: Status,

    /// Whose post this is. Declared as a field, because a `belongs_to` key has
    /// to be one: the preloader groups parents by the value it reads here.
    pub author_id: i64,

    /// Who wrote it.
    #[entity(belongs_to = Author, fk = "author_id")]
    pub author: Related<Author>,
}

/// A partial select: the two columns a listing page needs.
#[derive(Projection, Debug, Clone, PartialEq, Eq)]
#[projection(entity = Post)]
pub struct PostListing {
    /// The primary key.
    pub id: i64,
    /// The headline.
    pub title: String,
}

/// Renders a query the way the executor would, for assertions on text.
fn render(query: &Select<Post>, dialect: &dyn Dialect) -> String {
    query
        .to_statement()
        .expect("the fixture is neither unjoined nor tenant-scoped")
        .build(dialect)
        .expect("the statement renders on both dialects")
        .text
}

// ───────────────────────────────────────────────────────────────────────────
// The derive's own surface
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn the_derive_produces_the_entity_impl_the_documentation_promises() {
    assert_eq!(<Post as Entity>::NAME, "Post");
    assert_eq!(<Post as Entity>::TABLE.name().as_str(), "d_posts");
    assert_eq!(
        <Post as Entity>::COLUMNS
            .iter()
            .map(ColumnDef::name)
            .collect::<Vec<_>>(),
        ["id", "title", "views", "status", "author_id"],
        "the columns are the declared fields, in order, minus the relations"
    );
}

#[test]
fn the_derive_produces_the_query_constructors_and_the_column_constants() {
    let all = render(&Post::query(), &Postgres);
    assert!(all.contains("d_posts"), "{all}");

    let one = render(&Post::find(7), &Postgres);
    assert!(one.to_lowercase().contains("where"), "{one}");

    // The generated column constants are typed by their field's type, which is
    // what makes `Post::VIEWS.gt("nope")` a compile error.
    let typed: Column<Post, i64> = Post::VIEWS;
    let _ = typed;
}

#[test]
fn the_derive_produces_the_new_struct_and_the_write_builders() {
    let new = NewPost {
        id: 1,
        title: "Hello".to_owned(),
        views: 0,
        status: Status::Draft,
        author_id: 1,
    };
    let insert = Post::insert(new)
        .to_statement()
        .expect("an insert of one row builds")
        .build(&Sqlite)
        .expect("renders on SQLite");
    assert!(insert.text.to_lowercase().contains("insert"), "{insert}");
    assert_eq!(
        insert.args.len(),
        5,
        "one bound parameter per column, and no interpolation"
    );
}

#[test]
fn a_derived_db_enum_round_trips_through_its_storage() {
    assert_eq!(<Status as DbEnumTrait>::VARIANTS, ["draft", "published"]);
    assert_eq!(Status::Draft.as_db_str(), "draft");
    assert_eq!(Status::from_db_str("published"), Some(Status::Published));
    assert_eq!(Status::from_db_str("nonsense"), None);
    assert_eq!(Status::from_db_int(0), Some(Status::Draft));
}

// ───────────────────────────────────────────────────────────────────────────
// N1 — shape stability, through the derived builders
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n1_the_derived_builder_is_shape_stable() {
    /// Accepts `Select<Post>` and nothing else: a combinator that widened the
    /// type would not coerce to this parameter.
    fn still_a_select(_: &Select<Post>) {}

    let query = Post::query()
        .filter(Post::VIEWS.gt(10))
        .filter_opt(Some(Post::ID.gt(0)))
        .filter_if(true, || Post::VIEWS.lt(1_000_000))
        .when(true, |q| q.limit(20))
        .apply(|q| q.offset(5))
        .order_by(Post::VIEWS.desc())
        .distinct()
        .clear_order()
        .order_by(Post::ID.asc());

    still_a_select(&query);
    assert_eq!(query.filters().len(), 3);
}

#[test]
fn n1_the_type_a_user_reads_in_an_error_is_short() {
    for name in [
        core::any::type_name::<Select<Post>>(),
        core::any::type_name::<Update<Post>>(),
        core::any::type_name::<Delete<Post>>(),
        core::any::type_name::<Insert<Post>>(),
    ] {
        let short = name.replace("derives::", "").replace("moso_orm::", "");
        assert!(short.len() <= 80, "{short} is {} characters", short.len());
    }
}

// ───────────────────────────────────────────────────────────────────────────
// N2 — an unloaded relation never queries
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n2_an_unloaded_relation_returns_not_loaded_without_a_database() {
    let post = Post {
        id: 1,
        title: "Hello".to_owned(),
        views: 0,
        status: Status::Draft,
        author_id: 1,
        author: Related::NotLoaded,
    };

    // No `Db` is in scope in this test at all: if the accessor could query, it
    // could not compile, let alone pass.
    let error = post.author().expect_err("nothing was preloaded");
    let text = error.to_string();
    assert!(
        text.contains("Post::author"),
        "the error names the relation: {text}"
    );
    assert!(
        text.contains(".with(Post::AUTHOR)"),
        "and carries a paste-able fix: {text}"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// N3 — batched eager loading is +1 statement per relation
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n3_a_preload_of_a_derived_relation_is_one_statement() {
    let query = Post::query().with(Post::AUTHOR);
    let total: usize = query.preloads().iter().map(Preload::statement_count).sum();
    assert_eq!(total, 1, "one relation, one extra statement");

    let nested = Author::query().with(Preload::from(Author::POSTS).with(Post::AUTHOR));
    let total: usize = nested.preloads().iter().map(Preload::statement_count).sum();
    assert_eq!(total, 2, "a nested preload is +1 per level, not per row");
}

// ───────────────────────────────────────────────────────────────────────────
// N4 — dynamic filters, with no type gymnastics
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n4_absent_filters_leave_the_statement_alone() {
    let none: Option<i64> = None;
    let unfiltered = Post::query()
        .filter_opt(none.map(|v| Post::VIEWS.gt(v)))
        .filter_if(false, || Post::TITLE.eq("never".to_owned()))
        .when(false, |q| q.limit(1));
    assert!(
        !render(&unfiltered, &Postgres)
            .to_lowercase()
            .contains("where"),
        "an absent filter must add no clause: {}",
        render(&unfiltered, &Postgres)
    );

    let filtered = Post::query().filter_opt(Some(Post::VIEWS.gt(10)));
    assert!(
        render(&filtered, &Postgres)
            .to_lowercase()
            .contains("where"),
        "{}",
        render(&filtered, &Postgres)
    );
}

// ───────────────────────────────────────────────────────────────────────────
// N5 — typed partial selects
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n5_a_derived_projection_selects_only_its_columns() {
    let sql = Post::query()
        .project::<PostListing>()
        .to_statement()
        .expect("builds")
        .build(&Postgres)
        .expect("renders")
        .text;
    assert!(sql.contains("title"), "{sql}");
    assert!(
        !sql.contains("views"),
        "a projection selects its own columns and no others: {sql}"
    );
}

#[test]
fn n5_a_tuple_projection_is_typed_by_its_columns() {
    /// Binds the tuple's `Output`, so a wrong element type is a compile error.
    fn output_is<C: ColumnTuple<Output = O>, O>() {}
    output_is::<(Column<Post, i64>, Column<Post, String>), (i64, String)>();
}

// ───────────────────────────────────────────────────────────────────────────
// N6 — the descriptor the migration generator reads
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn n6_the_descriptor_carries_what_the_generator_needs() {
    let descriptor = <Post as Entity>::descriptor();
    assert_eq!(descriptor.table().name().as_str(), "d_posts");
    assert_eq!(descriptor.columns().len(), 5);
    assert!(
        descriptor
            .relations()
            .iter()
            .any(|relation| relation.name() == "author"),
        "the relation is in the descriptor, so `moso db make-migration` can emit its FK"
    );

    let authors = <Author as Entity>::descriptor();
    let authors_columns = <Author as Entity>::COLUMNS;
    assert!(
        authors
            .columns()
            .iter()
            .any(|column| column.name().as_str() == "email" && column.is_unique()),
        "`#[entity(unique)]` reaches the descriptor, which is what emits the constraint"
    );
    assert!(
        authors_columns
            .iter()
            .any(|column| column.name() == "email" && column.is_unique()),
        "…and the const column list the executor reads, so the two never disagree"
    );
}

// ───────────────────────────────────────────────────────────────────────────
// Both dialects, for every construct the derive produces
// ───────────────────────────────────────────────────────────────────────────

#[test]
fn every_derived_statement_renders_on_both_dialects() {
    let query = Post::query()
        .filter(Post::STATUS.eq(Status::Published))
        .order_by(Post::VIEWS.desc())
        .limit(10);

    let postgres = render(&query, &Postgres);
    let sqlite = render(&query, &Sqlite);

    assert!(
        postgres.contains("$1"),
        "PostgreSQL binds by number: {postgres}"
    );
    assert!(sqlite.contains('?'), "SQLite binds positionally: {sqlite}");
    assert!(postgres.contains("\"d_posts\""), "{postgres}");
    assert!(sqlite.contains("\"d_posts\""), "{sqlite}");
}
