//! Every entity attribute, query shape and write builder the ORM documentation
//! prints, written the way the documentation prints it, in one fixture.
//!
//! `derives.rs` next door proves the *derive* — that the tokens resolve, that
//! the descriptor is right, that a statement renders on both dialects — over a
//! deliberately small pair of entities. This file proves something narrower and
//! more perishable: that the surface `docs/02-data/21-entities-queries.md` and
//! `22-relations.md` tell a reader to write still exists and still composes.
//! Those two are the pages a newcomer copies from, so an example that stopped
//! compiling is a bug with an audience.
//!
//! It is one binary rather than three because the three properties share one
//! fixture, and three fixtures that drift apart would be exactly the redundancy
//! the workspace is written to avoid.
//!
//! # What each test holds
//!
//! | Test | The property |
//! | --- | --- |
//! | `every_documented_entity_attribute_reaches_the_descriptor` | the declarations mean what they say |
//! | `every_documented_query_shape_composes_and_keeps_its_type` | ADR-0007: `Select<E>` stays `Select<E>` |
//! | `every_documented_statement_renders_on_postgres` | the builders produce SQL, not just types |
//! | `a_factory_default_is_the_expression_the_struct_declares` | `#[factory(..)]` is read, and read from the struct |
//!
//! # No database
//!
//! Nothing here connects. `round_trip.rs` owns "a real server accepts this",
//! and duplicating it would only buy a second suite to keep green. The tables
//! still carry the `ds_` prefix the other files use, so a test that later does
//! want a connection cannot collide with theirs.

use chrono::{DateTime, Utc};
use moso::db::prelude::*;
use moso::db::{DbEnum as DbEnumTrait, descriptor::EntityDescriptor, expr};
use moso::schema::{Email, Id};
use moso::sql::{Postgres, Statement};
use moso::{DbEnum, Embedded, Entity, Factory, Projection};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// The fixture
// ---------------------------------------------------------------------------

/// Where an article is in its lifecycle.
#[derive(DbEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[db_enum(as = "text", rename_all = "snake_case")]
pub enum Status {
    /// Not visible yet.
    Draft,
    /// Visible to everyone.
    Published,
    /// Withdrawn.
    #[db_enum(rename = "n/a")]
    Retracted,
}

/// Anything the editor wanted to record, stored as one `jsonb` column.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Meta {
    /// The canonical URL, if the article was syndicated.
    pub canonical: Option<String>,
}

/// A postal address, flattened into its owner's table.
#[derive(Embedded, Clone, Debug)]
#[embedded(prefix = "billing_")]
pub struct Address {
    /// The street.
    pub line1: String,
    /// The city.
    pub city: String,
    /// The postcode.
    #[embedded(len = 16)]
    pub postcode: String,
}

/// The address every factory-built user gets.
///
/// A factory default is an ordinary Rust expression, so the readable way to
/// give one for a type that is not `Default` — which an embedded value object
/// deliberately is not — is to name a function that builds it.
fn sample_address() -> Address {
    Address {
        line1: String::from("1 Main St"),
        city: String::from("Lyon"),
        postcode: String::from("F-69001"),
    }
}

/// Someone who can sign in.
#[derive(Entity, Factory, Debug, Clone)]
#[entity(
    table = "ds_users",
    timestamps,
    soft_delete = "deleted_at",
    versioned = "version",
    index(name = "ds_users_email_lower", columns("email"), unique),
    check(name = "ds_users_email_not_blank", expr = "length(email) > 0"),
    comment = "Everyone who can sign in.",
    new_derives(Debug)
)]
#[factory(
    id = "Id::new()",
    email = "Email::new(format!(\"user{n}@example.com\")).expect(\"valid\")",
    billing = "sample_address()"
)]
pub struct User {
    /// The primary key.
    #[entity(pk)]
    pub id: Id<User>,

    /// Login identity; one row per address.
    #[entity(unique)]
    pub email: Email,

    /// Display name.
    #[entity(len = 64, comment = "Shown on the byline.")]
    pub name: String,

    /// Where to send the invoice.
    #[entity(embedded)]
    pub billing: Address,

    /// Everything this user wrote.
    #[entity(has_many = Article, fk = "author_id")]
    pub articles: Related<Vec<Article>>,

    /// How many articles there are, without loading them.
    #[entity(count_of = "articles")]
    pub articles_count: Option<i64>,

    /// When the row was written.
    #[entity(default = "now()")]
    pub created_at: DateTime<Utc>,
    /// When the row last changed.
    #[entity(default = "now()")]
    pub updated_at: DateTime<Utc>,
    /// When the row was retired, if it was.
    pub deleted_at: Option<DateTime<Utc>>,
    /// The optimistic-locking counter.
    #[entity(default = "0")]
    pub version: i32,
}

/// One article, by one user.
#[derive(Entity, Debug, Clone)]
#[entity(table = "ds_articles", timestamps)]
pub struct Article {
    /// The primary key.
    #[entity(pk)]
    pub id: Id<Article>,

    /// The headline.
    pub title: String,

    /// The body, absent while the article is an outline.
    pub body: Option<String>,

    /// How many people read it.
    #[entity(default = "0")]
    pub views: i64,

    /// Whether readers can see it.
    #[entity(enum_as = "text")]
    pub status: Status,

    /// Anything the editor wanted to record.
    #[entity(jsonb)]
    pub meta: Meta,

    /// A column the database computes.
    #[entity(generated = "lower(title)")]
    pub title_lower: String,

    /// Whose article this is.
    #[entity(index)]
    pub author_id: Id<User>,

    /// Who wrote it.
    #[entity(belongs_to = User, fk = "author_id")]
    pub author: Related<User>,

    /// When the row was written.
    #[entity(default = "now()")]
    pub created_at: DateTime<Utc>,
    /// When the row last changed.
    #[entity(default = "now()")]
    pub updated_at: DateTime<Utc>,
}

/// The two columns a listing page needs.
#[derive(Projection, Debug, Clone, PartialEq, Eq)]
#[projection(entity = Article)]
pub struct ArticleListing {
    /// The primary key.
    pub id: Id<Article>,
    /// The headline.
    pub title: String,
}

/// A user and how much they wrote, across a join.
#[derive(Projection, Debug, Clone)]
#[projection(entity = User, join = Article)]
pub struct UserSummary {
    /// The user's key.
    pub id: Id<User>,
    /// Their address.
    pub email: Email,
    /// How many articles they have.
    #[projection(expr = "count(ds_articles.id)")]
    pub article_count: i64,
    /// When they last wrote.
    #[projection(column = Article::CREATED_AT, agg = "max")]
    pub last_article_at: Option<DateTime<Utc>>,
}

/// Money, in minor units — a column type an application defines itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cents(pub i64);

impl SqlType for Cents {
    const KIND: moso::sql::ValueKind = moso::sql::ValueKind::I64;
    const TYPE_NAME: &'static str = "Cents";

    fn data_type() -> moso::sql::DataType {
        moso::sql::DataType::BigInt
    }

    fn to_value(&self) -> moso::sql::Value {
        moso::sql::Value::I64(self.0)
    }

    fn decode(row: &Row, index: usize) -> core::result::Result<Self, DecodeError> {
        row.get_i64(index).map(Self)
    }
}

/// A named, reusable query fragment — the form an inherent method cannot take,
/// because this one is a value that can be stored, cloned and chosen at runtime.
fn published() -> Scope<Article> {
    Scope::new("published", |query: Select<Article>| {
        query.filter(Article::STATUS.eq(Status::Published))
    })
}

/// One unsaved article, so the write builders have something to insert.
fn new_article() -> NewArticle {
    NewArticle {
        id: Id::new(),
        title: String::from("Hello"),
        body: None,
        views: None,
        status: Status::Draft,
        meta: Meta::default(),
        author_id: Id::new(),
    }
}

/// The PostgreSQL text a builder's statement renders to.
fn rendered(statement: moso::db::Result<Statement>) -> String {
    statement
        .expect("the fixture is neither unjoined nor tenant-scoped")
        .build(&Postgres)
        .expect("the statement renders on PostgreSQL")
        .text
}

// ---------------------------------------------------------------------------
// The declarations
// ---------------------------------------------------------------------------

#[test]
fn every_documented_entity_attribute_reaches_the_descriptor() {
    let user: &EntityDescriptor = <User as Entity>::descriptor();
    assert_eq!(user.table().name().as_str(), "ds_users");
    assert!(user.is_soft_deletable(), "`soft_delete` is on the entity");

    let columns: Vec<&str> = <User as Entity>::COLUMNS
        .iter()
        .map(ColumnDef::name)
        .collect();
    assert!(
        columns.contains(&"billing_line1") && columns.contains(&"billing_postcode"),
        "`#[entity(embedded)]` splices the embedded columns in, prefixed: {columns:?}"
    );
    assert!(
        !columns.contains(&"articles"),
        "a relation is not a column: {columns:?}"
    );
    assert!(
        !columns.contains(&"articles_count"),
        "`count_of` is loaded, not stored: {columns:?}"
    );

    assert_eq!(
        <Status as DbEnumTrait>::VARIANTS,
        ["draft", "published", "n/a"],
        "`rename_all` spells the variants, and `rename` overrides one"
    );

    assert_eq!(
        <Article as Entity>::descriptor().table().name().as_str(),
        "ds_articles"
    );

    // A column constant is typed by the field's Rust type, which is what makes
    // `Article::VIEWS.eq("x")` a compile error rather than a query with no rows.
    let _: Column<Article, i64> = Article::VIEWS;
    let _: Column<User, Email> = User::EMAIL;

    // A custom `SqlType` is an ordinary application type.
    assert_eq!(Cents(500).to_value(), moso::sql::Value::I64(500));
}

// ---------------------------------------------------------------------------
// The query builder
// ---------------------------------------------------------------------------

#[test]
fn every_documented_query_shape_composes_and_keeps_its_type() {
    // ADR-0007, spelled out: the annotations are the assertion. Every one of
    // these calls returns the type it was given, so an error message about this
    // query names `Select<Article>` and not a forty-character nest.
    let filtered: Select<Article> = Article::query()
        .filter(Article::VIEWS.gt(10) & Article::TITLE.contains("rust"))
        .filter(!Article::BODY.is_null())
        .filter_opt(Some(Article::STATUS.eq(Status::Published)))
        .filter_if(true, || Article::VIEWS.between(1, 1_000))
        .when(true, |query| query.limit(20))
        .apply(|query| query.offset(10))
        .order_by(Article::CREATED_AT.desc())
        .order_by_nulls_last(Article::BODY.asc())
        .with_scope(&published())
        .distinct();
    assert!(filtered.filters().len() > 1, "every filter is kept");

    let _ = all([Article::VIEWS.gt(1), Article::VIEWS.lt(10)]);
    let _ = any([Article::VIEWS.gt(1), Article::VIEWS.lt(10)]);
    let _ = not(Article::VIEWS.gt(1));

    // Aggregates and grouping.
    let _ = User::query()
        .left_join(User::ARTICLES)
        .group_by(User::ID.expr())
        .having(Article::ID.count().gt(moso::sql::Expr::value(3_i64)));

    // Subqueries, in both shapes.
    let prolific = User::query().filter(User::NAME.ne(String::new()));
    let _ = Article::query()
        .filter(expr::in_query(Article::AUTHOR_ID, User::ID, &prolific).expect("renderable"));
    let _ = Article::query().filter(expr::exists(&prolific).expect("renderable"));

    // Projections: a tuple, a struct, and a struct across a join.
    let _ = Article::query().select((Article::ID, Article::TITLE));
    let _ = Article::query().project::<ArticleListing>();
    let _ = User::query()
        .left_join(User::ARTICLES)
        .group_by(User::ID.expr())
        .project::<UserSummary>();

    // Pagination, keyset and offset.
    let _ = Article::query()
        .order_by(Article::CREATED_AT.desc())
        .paginate(None, 25)
        .with_total()
        .with_scope("inbox");
    let _ = Article::query()
        .order_by(Article::CREATED_AT.desc())
        .paginate_offset(3, 25);

    // Locks.
    let locked: Select<Article> = Article::query()
        .filter(Article::ID.eq(Id::new()))
        .lock(LockMode::ForUpdate);
    let _ = locked.lock_with(LockMode::ForUpdate, moso::db::LockBehavior::SkipLocked);

    // Soft-delete visibility, both directions.
    let _ = User::query().with_deleted();
    let _ = User::query().only_deleted();
}

// ---------------------------------------------------------------------------
// The statements
// ---------------------------------------------------------------------------

#[test]
fn every_documented_statement_renders_on_postgres() {
    let select = rendered(
        Article::query()
            .filter(Article::TITLE.icontains("rust"))
            .to_statement(),
    );
    assert!(select.starts_with("SELECT"), "{select}");

    let joined = rendered(
        User::query()
            .left_join(User::ARTICLES)
            .group_by(User::ID.expr())
            .project::<UserSummary>()
            .to_statement(),
    );
    assert!(joined.contains("count(ds_articles.id)"), "{joined}");
    assert!(joined.contains("LEFT JOIN"), "{joined}");

    let insert = rendered(Article::insert(new_article()).to_statement());
    assert!(insert.starts_with("INSERT INTO"), "{insert}");
    assert!(
        !insert.contains("title_lower"),
        "a generated column is written by the database: {insert}"
    );

    let upsert = rendered(
        Article::insert(new_article())
            .on_conflict(Article::ID)
            .do_update([Article::TITLE.ident(), Article::BODY.ident()])
            .returning_entity()
            .to_statement(),
    );
    assert!(upsert.contains("ON CONFLICT"), "{upsert}");

    let ignored = rendered(
        User::insert(NewUser {
            id: Id::new(),
            email: Email::new("ada@example.com").expect("valid"),
            name: String::from("Ada"),
            billing: sample_address(),
        })
        .on_conflict(User::EMAIL)
        .do_nothing()
        .to_statement(),
    );
    assert!(ignored.contains("DO NOTHING"), "{ignored}");

    let update = rendered(
        Article::update_all()
            .filter(Article::ID.eq(Id::new()))
            .set(Article::TITLE, String::from("New"))
            .set_opt(Article::BODY, Some(String::from("Body")))
            .set_with(Article::VIEWS, |current| {
                current + moso::sql::Expr::value(1_i64)
            })
            .to_statement(),
    );
    assert!(update.starts_with("UPDATE"), "{update}");

    let optimistic = rendered(
        User::update_all()
            .filter(User::ID.eq(Id::new()))
            .set(User::NAME, String::from("Ada"))
            .expecting_version(1_i32)
            .to_statement(),
    );
    assert!(optimistic.contains("version"), "{optimistic}");

    let hard = rendered(
        Article::delete_all()
            .filter(Article::ID.eq(Id::new()))
            .hard()
            .to_statement(),
    );
    assert!(hard.starts_with("DELETE FROM"), "{hard}");

    // A hand-written statement's interpolations are bind parameters, never text.
    let threshold = 40_i64;
    let raw = moso::sql!("select id, title from ds_articles where views >= {threshold}");
    assert!(raw.text().contains("$1"), "{}", raw.text());
}

// ---------------------------------------------------------------------------
// The factory
// ---------------------------------------------------------------------------

#[test]
fn a_factory_default_is_the_expression_the_struct_declares() {
    let first = User::factory().build();
    assert_eq!(first.email.as_str(), "user0@example.com");
    assert_eq!(
        first.billing.city, "Lyon",
        "an embedded value object takes its default from the same attribute"
    );

    // `n` is the row's index, which is what makes twenty rows twenty different
    // rows without a `sequence(..)` closure.
    let twenty = User::factory().count(20).build_many();
    assert_eq!(twenty.len(), 20);
    assert_eq!(twenty[19].email.as_str(), "user19@example.com");
    assert_ne!(
        twenty[0].id, twenty[1].id,
        "`Id::default()` is `Id::NIL`, so a primary key with no database default \
         needs a factory default or every row collides"
    );

    // A setter fixes one value for every row and leaves the rest defaulted.
    let named = User::factory()
        .name(String::from("Ada"))
        .count(2)
        .build_many();
    assert_eq!(named[0].name, "Ada");
    assert_eq!(named[1].name, "Ada");
    assert_eq!(named[1].email.as_str(), "user1@example.com");
}
