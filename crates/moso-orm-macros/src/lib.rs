#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![doc = "Procedural macros for Moso's ORM."]
//!
//! # What is here
//!
//! | Macro | Generates |
//! | --- | --- |
//! | [`macro@Entity`] | `impl Entity`, the column and relation constants, `New…`, and the query helpers |
//! | [`macro@Projection`] | a positional `from_row` and the `SELECT` list |
//! | [`macro@Embedded`] | flattens a struct into its owner's columns |
//! | [`macro@DbEnum`] | `impl DbEnum` and `impl SqlType` for an enum column |
//! | [`macro@Factory`] | a typed builder for test fixtures and seeds |
//! | [`macro@migration`] | the version, name and description of one migration |
//! | [`sql!`](macro@sql) | a hand-written statement whose interpolations are bind parameters |
//!
//! # Three rules the whole crate obeys
//!
//! **It depends on no runtime Moso crate** (dependency rule 1). Generated code
//! resolves against `::moso::__private::*` and nothing else (decision D6), so
//! the runtime layout can move without touching a macro and a user who renames
//! the dependency still compiles.
//!
//! **One mistake produces one error.** Where a problem is detectable here, the
//! macro emits a single `compile_error!` with a `help:` line and nothing else,
//! so the user does not also get twenty resolution failures from the code that
//! was not generated (`docs/04-devex/41-diagnostics.md`, rule 4).
//!
//! **Nothing is guessed.** A migration with no version, a projection field that
//! names no column, an enum variant that collides with another: each is an
//! error naming both fixes rather than a default that happens to be wrong on
//! the day it matters.

extern crate proc_macro;

use proc_macro::TokenStream;

mod db_enum;
mod embedded;
mod entity;
mod factory;
mod migration;
mod projection;
mod shared;
mod sql;

/// Derives `Entity` — one struct, one table.
///
/// # Container attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `table = "users"` | default: the `snake_case` pluralised type name |
/// | `schema = "billing"` | a PostgreSQL schema |
/// | `soft_delete = "deleted_at"` | every query adds `WHERE deleted_at IS NULL` |
/// | `timestamps` | shorthand for `created_at` + `updated_at` |
/// | `expose` | opts out of the "entities are not schemas" rule (ADR-0008) |
/// | `index(columns("a", "b"), unique, method = "gin", where = "…", include("c"))` | a composite, partial or method-qualified index |
/// | `check(name = "…", expr = "…")` | a table `CHECK` |
/// | `versioned = "version"` | optimistic locking |
/// | `tenant = "tenant_id"` | tenant scoping, enforced at compile time |
/// | `audit` | records changes into the audit table |
/// | `rls` | emits a row-level-security policy |
/// | `comment = "…"` | default: the type's documentation comment |
/// | `new_derives(Debug, Default)` | extra derives on the generated `New…` struct |
///
/// # Field attributes
///
/// `pk`, `column = "…"`, `unique`, `index` (or `index(..)`), `default = "…"`,
/// `len = 255`, `precision(10, 2)`, `json` / `jsonb`, `enum_as = "text" |
/// "int" | "pg_enum"`, `created_at`, `updated_at`, `readonly`, `generated =
/// "…"`, `encrypted`, `comment = "…"`, `embedded`, and the relation attributes
/// `belongs_to`, `has_many`, `has_one`, `many_to_many` and `belongs_to_any`,
/// with `fk`, `through`, `left`, `right`, `on_delete`, `on_update` and
/// `self_ref`.
///
/// # What it generates
///
/// ```text
/// impl ::moso::__private::Entity for User { … }   // TABLE, COLUMNS, NAME, pk, from_row, descriptor
/// impl User {
///     pub const ID: Column<User, Id<User>> = Column::new("id");
///     pub const POSTS: HasMany<User, Post> = HasMany::new("posts", "author_id");
///     pub fn posts(&self) -> Result<&Vec<Post>, NotLoaded>;
///     pub fn query() -> Select<User>;
///     pub fn find(pk: Id<User>) -> Select<User>;
///     pub fn insert(row: NewUser) -> Insert<User>;
///     pub fn insert_many(rows: impl IntoIterator<Item = NewUser>) -> Insert<User>;
///     pub fn update(&self) -> Update<User>;
///     pub fn update_all() -> Update<User>;
///     pub fn delete(&self) -> Delete<User>;
///     pub fn delete_all() -> Delete<User>;
/// }
/// pub struct NewUser { … }                        // minus defaults, timestamps, relations
/// ```
///
/// A `belongs_to` **must** name a key that is a declared field: writing
/// `pub author_id: i64,` next to the relation is what gives the preloader a
/// value to group parents by, and what gives a query a
/// `Post::AUTHOR_ID: Column<Post, i64>` to filter on without a join. Leaving it
/// out is an error rather than a synthesised column, because the only fallback
/// available to the preloader — the row's own primary key — returns the wrong
/// rows silently.
///
/// # Example
///
/// ```ignore
/// use moso::db::prelude::*;
///
/// /// Someone who can sign in.
/// #[derive(Entity, Debug, Clone)]
/// #[entity(table = "users", timestamps, soft_delete = "deleted_at")]
/// pub struct User {
///     /// The primary key.
///     #[entity(pk, default = "uuid_generate_v7()")]
///     pub id: Id<User>,
///
///     /// Login identity.
///     #[entity(unique, index)]
///     pub email: Email,
///
///     /// Everything this user wrote.
///     #[entity(has_many = Post, fk = "author_id")]
///     pub posts: Related<Vec<Post>>,
///
///     /// When the row was written.
///     pub created_at: DateTime<Utc>,
///     /// When the row last changed.
///     pub updated_at: DateTime<Utc>,
///     /// When the row was retired, if it was.
///     pub deleted_at: Option<DateTime<Utc>>,
/// }
/// ```
#[proc_macro_derive(Entity, attributes(entity))]
pub fn derive_entity(input: TokenStream) -> TokenStream {
    entity::expand(input.into()).into()
}

/// Derives `Projection` — a struct a partial select decodes into.
///
/// # Container attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `entity = User` | the entity whose columns are in scope, and the source of a bare field's constant |
/// | `join = Post` (or `join(Post, Tag)`) | another entity whose columns may be referenced |
///
/// # Field attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `column = Post::TITLE` | the column this field reads |
/// | `expr = "count(posts.id)"` | a raw expression |
/// | `agg = "max"` | wraps the column in an aggregate |
/// | `alias = "…"` | the output name; default: the field's |
/// | `skip` | not selected; filled with `Default::default()` |
///
/// A field with no attribute reads the entity constant of the same name:
/// `email` becomes `User::EMAIL`.
///
/// # The compile-time check
///
/// The derive writes `impl ProjectionScope<E>` once per named entity and routes
/// every column through `checked_column`, whose bound is satisfied only by
/// those entities. Reading a column of an entity the projection does not join
/// is therefore an error at the field, not a query that returns no rows.
///
/// # Example
///
/// ```ignore
/// use moso::db::prelude::*;
///
/// /// A user and how much they wrote.
/// #[derive(Projection)]
/// #[projection(entity = User, join = Post)]
/// pub struct UserSummary {
///     /// The user's key.
///     pub id: Id<User>,
///     /// How many posts they have.
///     #[projection(expr = "count(posts.id)")]
///     pub post_count: i64,
///     /// When they last wrote.
///     #[projection(column = Post::CREATED_AT, agg = "max")]
///     pub last_post_at: Option<DateTime<Utc>>,
/// }
/// ```
#[proc_macro_derive(Projection, attributes(projection))]
pub fn derive_projection(input: TokenStream) -> TokenStream {
    projection::expand(input.into()).into()
}

/// Derives `Embedded` — a struct whose fields become columns of its owner.
///
/// A value object such as `Address { line1, city, postcode }` becomes
/// `address_line1`, `address_city`, `address_postcode` on the owning table,
/// with one prefix and no join. The owner declares it with
/// `#[entity(embedded)]`, and the columns are spliced into its
/// `Entity::COLUMNS` at compile time.
///
/// # Container attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `prefix = "address_"` | prepended to every generated column name |
///
/// # Field attributes
///
/// `column = "…"`, `len = 255`, `precision(10, 2)`, `json`, `default = "…"`,
/// `comment = "…"`.
///
/// # Example
///
/// ```ignore
/// use moso::db::prelude::*;
///
/// /// Where something is.
/// #[derive(Embedded, Clone, Debug)]
/// #[embedded(prefix = "address_")]
/// pub struct Address {
///     /// The street.
///     pub line1: String,
///     /// The city.
///     pub city: String,
/// }
/// ```
#[proc_macro_derive(Embedded, attributes(embedded))]
pub fn derive_embedded(input: TokenStream) -> TokenStream {
    embedded::expand(input.into()).into()
}

/// Derives `DbEnum` and `SqlType` for an enum column.
///
/// # Container attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `as = "text"` | store the variant's name. The default |
/// | `as = "int"` | store the discriminant |
/// | `as = "pg_enum"` | a PostgreSQL `CREATE TYPE … AS ENUM` |
/// | `type_name = "order_status"` | the enum type's name, for `pg_enum` |
/// | `rename_all = "snake_case"` | how variant names are spelled in the column |
///
/// # Variant attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `rename = "n/a"` | this variant's stored spelling |
///
/// Reading a value the enum does not know is a decode error naming the value
/// and listing the variants — never a silent fallback.
///
/// # Example
///
/// ```ignore
/// use moso::db::prelude::*;
///
/// /// Where an order is in its lifecycle.
/// #[derive(DbEnum, Clone, Copy, Debug, PartialEq, Eq)]
/// #[db_enum(as = "pg_enum", type_name = "order_status")]
/// pub enum Status {
///     /// Awaiting payment.
///     Pending,
///     /// Paid for.
///     Paid,
/// }
/// ```
#[proc_macro_derive(DbEnum, attributes(db_enum))]
pub fn derive_db_enum(input: TokenStream) -> TokenStream {
    db_enum::expand(input.into()).into()
}

/// Derives a typed factory for test fixtures and seed data.
///
/// Requires `#[derive(Entity)]` on the same struct: the factory's setters are
/// the generated `New…` struct's fields, so there is no second list to keep in
/// step.
///
/// # Container attributes
///
/// `#[factory(field = "expression")]` gives a field its default. The expression
/// is ordinary Rust, evaluated once per row, with the row's index in scope as
/// `n`. A field with no default falls back to `Default::default()`.
///
/// # There are no field attributes
///
/// Every other derive here reads a field form, so `#[factory(default = "…")]`
/// above the field is the natural thing to write. It is a compile error naming
/// the container line to write instead — never a silent no-op, which is what a
/// declared-but-unread helper attribute would otherwise be.
///
/// # Example
///
/// ```ignore
/// use moso::db::prelude::*;
///
/// #[derive(Entity, Factory)]
/// #[factory(email = "format!(\"user{n}@example.com\")", name = "String::from(\"Ada\")")]
/// pub struct User { /* … */ }
///
/// # async fn example(db: &Db) -> Result<()> {
/// let admin = User::factory().is_admin(true).create(db).await?;
/// let many = User::factory()
///     .count(20)
///     .sequence(|i, mut row| { row.name = format!("User {i}"); row })
///     .create_many(db)
///     .await?;
/// let unsaved = User::factory().build();
/// # Ok(())
/// # }
/// ```
#[proc_macro_derive(Factory, attributes(factory, entity))]
pub fn derive_factory(input: TokenStream) -> TokenStream {
    factory::expand(input.into()).into()
}

/// Records one migration's version, name and description.
///
/// # Attributes
///
/// | Attribute | Effect |
/// | --- | --- |
/// | `version = "20260730T090000"` | default: the leading timestamp of the file's name |
/// | `name = "backfill_slugs"` | default: the type's name in `snake_case` |
/// | `description = "…"` | default: the type's documentation comment |
///
/// It adds four inherent constants — `VERSION`, `NAME`, `DESCRIPTION` and
/// `SOURCE` — and leaves the `impl Migration` block to the author. It registers
/// nothing: ADR-0004 rules out link-time registries, so the list of migrations
/// is a written-down list.
///
/// # Example
///
/// ```ignore
/// // migrations/20260730T090000_backfill_slugs.rs
/// use moso::migrate::prelude::*;
///
/// /// Fills in the slugs the old importer left null.
/// #[migration]
/// pub struct BackfillSlugs;
///
/// impl Migration for BackfillSlugs {
///     const REVERSIBLE: bool = false;
///     async fn up(m: &mut Migrator) -> Result<()> { /* … */ Ok(()) }
/// }
/// ```
#[proc_macro_attribute]
pub fn migration(attributes: TokenStream, item: TokenStream) -> TokenStream {
    let file = proc_macro::Span::call_site().file();
    migration::expand(attributes.into(), item.into(), Some(file.as_str())).into()
}

/// A hand-written statement whose interpolations are **bind parameters**.
///
/// ```text
/// sql!("select id, email from users where created_at > {since}")
/// // →
/// RawQuery::new("select id, email from users where created_at > $1").bind(since)
/// ```
///
/// There is no syntax that concatenates a runtime string into the statement
/// text, so this cannot produce an injection even when handed a request body.
///
/// | Written | Means |
/// | --- | --- |
/// | `{name}` | bind the variable `name` |
/// | `{a.b().c}` | bind the value of any Rust expression |
/// | `{value as Cents}` | bind it as a `Cents`, when inference needs telling |
/// | `{{` / `}}` | a literal `{` / `}` |
///
/// The placeholders are `$1`-style, which is PostgreSQL's spelling; a raw
/// statement is written against one backend by definition.
///
/// # Example
///
/// ```ignore
/// use moso::db::prelude::*;
///
/// # async fn example(db: &Db, since: i64) -> Result<Vec<UserSummary>> {
/// moso::sql!(
///     "select u.id, u.email, count(p.id) as post_count
///        from users u left join posts p on p.author_id = u.id
///       where u.created_at > {since}
///       group by u.id"
/// )
/// .project_all::<UserSummary>(db)
/// .await
/// # }
/// ```
#[proc_macro]
pub fn sql(input: TokenStream) -> TokenStream {
    sql::expand(input.into()).into()
}
