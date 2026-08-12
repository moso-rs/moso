# 21 — Entities, Columns & the Query Builder

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## Defining an entity

```rust
// example — src/models/user.rs
use moso::db::prelude::*;

#[derive(Entity, Debug, Clone)]
#[entity(table = "users")]
pub struct User {
    #[entity(pk, default = "uuid_v7()")]
    pub id: Id<User>,

    #[entity(unique, index)]
    pub email: Email,

    #[entity(len = 255)]
    pub name: String,

    #[entity(column = "password_hash")]
    pub password: PasswordHash,

    #[entity(default = "false")]
    pub is_admin: bool,

    #[entity(json)]
    pub preferences: Preferences,          // serde_json-backed jsonb column

    pub deleted_at: Option<DateTime<Utc>>,

    #[entity(created_at)]
    pub created_at: DateTime<Utc>,
    #[entity(updated_at)]
    pub updated_at: DateTime<Utc>,

    // relations are not columns
    #[entity(has_many = Post, fk = "author_id")]
    pub posts: Related<Vec<Post>>,
}
```

### Container attributes
| Attribute | Effect |
| --- | --- |
| `table = "users"` | default: `snake_case` pluralised struct name |
| `schema = "billing"` | Postgres schema |
| `soft_delete = "deleted_at"` | all queries add `WHERE deleted_at IS NULL`; `.with_deleted()` opts out |
| `timestamps` | shorthand for `created_at` + `updated_at` |
| `expose` | allows the entity to also derive `Schema` (opt-out of the DTO guard, see `00-foundations/04`) |
| `index(name, columns, unique, where, method)` | composite / partial / GIN indexes |
| `check(name, expr)` | table check constraint |
| `versioned = "version"` | optimistic locking column |
| `audit` | records changes into `moso_audit` (see `03-batteries/33-admin.md`) |

### Field attributes
| Attribute | Effect |
| --- | --- |
| `pk` | primary key; composite keys use `pk` on multiple fields |
| `column = "..."` | override column name |
| `unique`, `index` | constraint/index generated in the migration |
| `default = "sql expr"` | database default, emitted into the migration |
| `len = 255` | `VARCHAR(255)` instead of `TEXT` |
| `precision(10, 2)` | `NUMERIC(10,2)` |
| `json` / `jsonb` | serde-serialised column; the inner type needs `Serialize + DeserializeOwned` |
| `enum_as = "text" \| "int" \| "pg_enum"` | enum storage strategy |
| `created_at`, `updated_at` | auto-managed timestamps |
| `readonly` | never included in INSERT/UPDATE (e.g. generated columns) |
| `encrypted` | transparent column encryption via the app's key (AES-GCM-SIV) |
| `belongs_to`, `has_many`, `has_one`, `many_to_many` | relations (`22-relations.md`) |

## What the derive generates

```rust
// generated (abridged) — see 06-reference/62-macro-reference.md for the full expansion
impl Entity for User {
    type Pk = Id<User>;
    const TABLE: TableRef = TableRef { schema: None, name: "users" };
    const COLUMNS: &'static [ColumnDef] = &[ /* … */ ];
    fn pk(&self) -> Self::Pk { self.id }
    fn from_row(row: &Row) -> Result<Self, DecodeError> { /* positional, no by-name lookup */ }
    fn descriptor() -> &'static EntityDescriptor { /* consumed by moso-migrate & moso-admin */ }
}

impl User {
    pub const ID:         Column<User, Id<User>>          = Column::new("id");
    pub const EMAIL:      Column<User, Email>             = Column::new("email");
    pub const NAME:       Column<User, String>            = Column::new("name");
    pub const IS_ADMIN:   Column<User, bool>              = Column::new("is_admin");
    pub const CREATED_AT: Column<User, DateTime<Utc>>     = Column::new("created_at");
    pub const POSTS:      HasMany<User, Post>             = HasMany::new("author_id");

    pub fn query() -> Select<User>;
    pub fn insert(v: NewUser) -> Insert<User>;
    pub fn update(&self) -> Update<User>;     // scoped to this row's PK
    pub fn update_all() -> Update<User>;
    pub fn delete(&self) -> Delete<User>;
    pub fn find(pk: Id<User>) -> Select<User>;
}
```

`Column<E, T>` carries the entity and the Rust type, which is where type safety lives. It is a
zero-sized-ish const (a `&'static str` plus two `PhantomData`), so there is no runtime cost and no
monomorphisation blowup.

## The query builder

### Shape stability (the core design rule)

```rust
// spec
pub struct Select<E: Entity> {
    // NO type-level clause encoding. All of these are runtime Vecs.
    filters: Vec<Expr>,
    joins: Vec<Join>,
    order: Vec<OrderTerm>,
    preloads: Vec<PreloadNode>,
    projection: Projection,
    limit: Option<u64>,
    offset: Option<u64>,
    lock: Option<LockMode>,
    _marker: PhantomData<E>,
}
```

Every combinator returns `Select<E>`. The type in an error message is at worst
`moso::db::Select<shop::models::User>`.

### Reading

```rust
// example
let user  = User::find(id).fetch_one(&db).await?;         // Error::NotFound if absent
let maybe = User::find(id).fetch_optional(&db).await?;    // Option<User>
let users = User::query().filter(User::IS_ADMIN.eq(true)).fetch_all(&db).await?;
let count = User::query().filter(User::IS_ADMIN.eq(true)).count(&db).await?;
let exists= User::query().filter(User::EMAIL.eq(&email)).exists(&db).await?;
let stream= User::query().fetch_stream(&db);              // Stream<Item = Result<User>>
let first = User::query().order_by(User::CREATED_AT.desc()).fetch_first(&db).await?;
```

Naming is chosen to match sqlx (`fetch_one`, `fetch_optional`, `fetch_all`) so knowledge transfers.

### Expressions

```rust
// spec — Column<E, T> methods, all type-checked against T
impl<E: Entity, T: SqlType> Column<E, T> {
    pub fn eq(self, v: impl Into<Value<T>>) -> Expr;
    pub fn ne(self, v: impl Into<Value<T>>) -> Expr;
    pub fn lt/le/gt/ge(self, v: impl Into<Value<T>>) -> Expr;
    pub fn between(self, a: impl Into<Value<T>>, b: impl Into<Value<T>>) -> Expr;
    pub fn is_in(self, vs: impl IntoIterator<Item = impl Into<Value<T>>>) -> Expr;
    pub fn is_null(self) -> Expr;                       // only when T: IsOption
    pub fn eq_col(self, other: Column<E2, T>) -> Expr;  // same T enforced
    pub fn asc(self) -> OrderTerm;
    pub fn desc(self) -> OrderTerm;
    pub fn nulls_last(self) -> OrderTerm;
}

// string-specific, only where T: TextLike
impl<E> Column<E, String> {
    pub fn like/ilike/starts_with/ends_with/contains(self, pat: impl AsRef<str>) -> Expr;
    pub fn matches(self, q: TextQuery) -> Expr;         // full-text search (tsquery)
}

// json-specific, only where T: JsonLike
impl<E, J> Column<E, J> {
    pub fn path(self, p: &'static str) -> JsonExpr;     // -> #>> '{a,b}'
    pub fn has_key(self, k: &str) -> Expr;
    pub fn contains_json(self, v: impl Serialize) -> Expr;  // @>
}

// combinators
pub fn and(exprs: impl IntoIterator<Item = Expr>) -> Expr;
pub fn or(exprs: impl IntoIterator<Item = Expr>) -> Expr;
pub fn not(e: Expr) -> Expr;
impl BitAnd for Expr { /* a & b */ }
impl BitOr  for Expr { /* a | b */ }
```

```rust
// example
User::query()
    .filter(User::IS_ADMIN.eq(true) & (User::NAME.ilike("a%") | User::EMAIL.ends_with("@acme.com")))
```

**Type errors are good by construction:**

```
error[E0277]: the trait bound `&str: Into<Value<bool>>` is not satisfied
  --> src/routes/users.rs:22:41
   |
22 |     .filter(User::IS_ADMIN.eq("yes"))
   |                            -- ^^^^^ expected a value comparable to column `users.is_admin`
   |                            |
   |                            column `is_admin` has type `bool`
   |
   = help: pass a `bool`, or use `User::IS_ADMIN.eq(value.parse::<bool>()?)`
```

(Achieved with `#[diagnostic::on_unimplemented]` on a `ColumnValue<T>` trait that `Into<Value<T>>`
goes through.)

### Dynamic queries — first-class

```rust
// spec
impl<E: Entity> Select<E> {
    pub fn filter(self, e: Expr) -> Self;
    /// Apply only if Some. The single most-used ergonomic helper.
    pub fn filter_opt(self, e: Option<Expr>) -> Self;
    /// Apply only if the condition holds.
    pub fn filter_if(self, cond: bool, f: impl FnOnce() -> Expr) -> Self;
    /// Arbitrary conditional transformation.
    pub fn when(self, cond: bool, f: impl FnOnce(Self) -> Self) -> Self;
    pub fn apply(self, f: impl FnOnce(Self) -> Self) -> Self;
}
```

```rust
// example — the full dynamic-filter case, with no type gymnastics
fn search(f: &ProductFilter) -> Select<Product> {
    Product::query()
        .filter_opt(f.category.map(|c| Product::CATEGORY_ID.eq(c)))
        .filter_opt(f.q.as_ref().map(|s| Product::NAME.matches(TextQuery::web(s))))
        .filter_if(f.in_stock, || Product::STOCK.gt(0))
        .when(f.include_archived, |q| q.with_deleted())
}
```

### Reusable scopes

```rust
// example
impl User {
    pub fn active() -> Select<User> { User::query().filter(User::DELETED_AT.is_null()) }
}
impl Select<User> {
    pub fn admins(self) -> Self { self.filter(User::IS_ADMIN.eq(true)) }
}
// User::active().admins().fetch_all(&db)
```

Because the builder is shape-stable, scopes compose without any trait gymnastics. This is where
Diesel's design makes people give up.

### Projections

```rust
// example — tuple projection
let rows: Vec<(Id<User>, Email)> =
    User::query().select((User::ID, User::EMAIL)).fetch_all(&db).await?;

// struct projection, including joined columns and aggregates
#[derive(Projection)]
#[projection(entity = User)]
struct UserSummary {
    id: Id<User>,
    email: Email,
    #[projection(expr = "count(posts.id)")]
    post_count: i64,
    #[projection(column = "Post::CREATED_AT", agg = "max")]
    last_post_at: Option<DateTime<Utc>>,
}

let rows = User::query()
    .left_join(User::POSTS)
    .group_by(User::ID)
    .project::<UserSummary>()
    .fetch_all(&db).await?;
```

`Projection` derives a `from_row` that decodes positionally and a compile-time check that every
referenced column belongs to the joined entity set.

### Writing

```rust
// example — insert
let user = User::insert(NewUser {
        email: body.email,
        name: body.name,
        password: PasswordHash::new(&body.password)?,
        ..Default::default()
    })
    .returning_entity()
    .fetch_one(&db).await?;

// bulk insert — one statement
User::insert_many(rows).execute(&db).await?;

// upsert
User::insert(new)
    .on_conflict(User::EMAIL)
    .do_update([User::NAME, User::UPDATED_AT])
    .fetch_one(&db).await?;

// update by pk
user.update()
    .set(User::NAME, "New name")
    .set_with(User::LOGIN_COUNT, |c| c + 1)          // atomic, becomes `login_count = login_count + 1`
    .fetch_one(&db).await?;

// bulk update
User::update_all()
    .filter(User::LAST_SEEN.lt(cutoff))
    .set(User::IS_ACTIVE, false)
    .execute(&db).await?;                             // returns rows affected

// delete
user.delete().execute(&db).await?;                    // soft delete if configured
User::query().filter(...).delete().hard().execute(&db).await?;
```

`NewUser` is generated by the derive: the entity's fields minus `pk`-with-default, minus
`created_at`/`updated_at`, minus relations, with `Option` for anything having a DB default. This
removes the "construct a struct with 4 dummy fields to insert a row" complaint about SeaORM's
`ActiveModel`.

`update_all()` **requires** a `.filter()` or an explicit `.all_rows()`; an unfiltered mass update is
a compile-time-impossible/runtime-refused operation. Same for `delete`. (This has cost real
companies real data.)

### Pagination

```rust
// spec
impl<E: Entity> Select<E> {
    /// Keyset pagination. Requires a deterministic order; adds pk as a tiebreaker automatically.
    pub fn paginate(self, cursor: Option<Cursor>, limit: u32) -> Paginated<E>;
    /// Offset pagination, for admin UIs that need page numbers.
    pub fn paginate_offset(self, page: u32, per_page: u32) -> OffsetPaginated<E>;
}
impl<E: Entity> Paginated<E> {
    pub async fn fetch(self, db: &Db) -> Result<Page<E>>;
    pub fn with_total(self) -> Self;     // adds a count query; opt-in because it costs
}
```

Cursors are opaque, signed (HMAC with the app secret), and encode the ordering key tuple — so a
tampered cursor is rejected rather than producing a weird page, and a cursor from a differently
ordered query is rejected with a clear error.

### Locking and concurrency

```rust
// example
let row = Account::find(id).lock(LockMode::ForUpdate).fetch_one(&tx).await?;

// optimistic locking via #[entity(versioned = "version")]
order.update().set(Order::STATUS, Status::Paid).fetch_one(&db).await?;
// → WHERE id = $1 AND version = $2 ; 0 rows ⇒ Error::Conflict("stale write")
```

## Raw SQL

```rust
// example
let rows: Vec<UserSummary> = moso::sql!(
    r#"select u.id, u.email, count(p.id) as post_count
         from users u left join posts p on p.author_id = u.id
        where u.created_at > {since}
        group by u.id"#
).fetch_all(&db).await?;
```

`sql!` binds `{ident}` as a parameter (never interpolates text), supports `{expr as ty}` for
disambiguation, and returns a `RawQuery` that decodes into any `Projection` or `Entity`. For
anything more exotic, `db.pool()` gives raw sqlx.

## Query logging & the dev experience

In `dev`, every statement is logged at `debug` with duration, row count, and the call-site
`file:line` (captured with `#[track_caller]`). Statements over `db.slow_query_ms` (default 200 ms)
log at `warn` in every profile with the query plan attached when `db.explain_slow = true`.

The dev error page shows the last 20 statements of the failing request. `moso db explain` runs a
built query through `EXPLAIN ANALYZE` from the CLI.

## Acceptance criteria (WP-11, WP-12)

1. `Select<User>` remains `Select<User>` through 10 chained combinators (compile-time assertion via
   a type-equality test).
2. `User::IS_ADMIN.eq("yes")` produces the hand-written diagnostic (UI test).
3. `update_all()` without a filter fails; with `.all_rows()` it succeeds (test).
4. Every combinator in this document has a test asserting the generated SQL against a snapshot,
   for Postgres and SQLite.
5. `NewUser` generation covers defaults, optional columns, and `#[entity(readonly)]` exclusion.
6. Cursor tampering and cross-query cursor reuse produce `Error::BadRequest` with a clear detail.
7. Construction overhead benchmark: building a 5-filter query ≤ 2 µs; within 15% of hand-written
   sqlx end to end.
8. `from_row` decodes positionally; a benchmark shows no per-column string hashing.
