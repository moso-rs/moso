---
title: Raw SQL
description: Run SQL the query builder cannot express, with every interpolated value bound as a parameter, decoded back into your entities and projections.
order: 19
status: shipped
---

Every ORM eventually meets a query it cannot express. Moso's answer is a set of escape hatches that
all keep the one property that matters: an interpolated value is always a bind parameter, never text
spliced into the statement. `moso::sql!` hands you a whole statement you wrote yourself. `expr::raw`
hands you a fragment inside a query the builder still owns. Neither has a syntax that concatenates a
runtime string into SQL, so neither can produce an injection even when handed a request body.

This page covers both, plus the hatch underneath them (sqlx's own pool), how rows are decoded back
into your types, the checks the raw path does not run for you, and what the sealed SQL facade means
for which crates you can name in your own code.

> [!NOTE]
> Everything here needs the `orm` cargo feature on the `moso` facade, which is off by default
> because it pulls in a database driver.

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1", features = ["orm"] }
```

## The smallest working example

```rust title="src/reports.rs"
use moso::db::prelude::*;
use moso::Projection;

/// The two columns a listing page needs.
#[derive(Projection, Debug, Clone, PartialEq, Eq)]
#[projection(entity = Post)]
pub struct PostListing {
    /// The primary key.
    pub id: i64,
    /// The headline.
    pub title: String,
}

let threshold = 40_i64;
let popular: Vec<PostListing> =
    moso::sql!("select id, title from posts where views >= {threshold} order by id")
        .project_all::<PostListing>(db)
        .await?;
```

`{threshold}` is not string interpolation. The macro splits the literal at compile time, writes `$1`
into the statement text, and appends `.bind(threshold)`. The value never touches the text.

`moso::sql` is also a module: the whole SQL construction layer, re-exported. A macro and a module
live in different namespaces, so `moso::sql!("…")` and `use moso::sql::Expr;` coexist without a
conflict.

## What `sql!` compiles to

```text
moso::sql!("select id, email from users where created_at > {since}")
// becomes
RawQuery::new("select id, email from users where created_at > $1").bind(since)
```

The macro takes one string literal and nothing else. There are no trailing arguments, because
everything a statement needs is spelled inside the literal, and a literal is what makes the statement
visible at compile time.

| Written | Means |
| --- | --- |
| `{name}` | bind the variable `name` |
| `{a.b().c}` | bind the value of any Rust expression |
| `{value as Cents}` | bind it as a `Cents`, when inference needs telling |
| `{{` / `}}` | a literal `{` / `}` |

Placeholders are numbered in the order they appear in the text, starting at `$1`. An expression
written twice is bound twice and gets two numbers: `sql!("select {a}, {b} where c = {a}")` renders
`select $1, $2 where c = $3` and binds three values.

`{table}` binds a *value* named `table`. It does not name a table. There is no interpolation that
produces SQL structure, so the identifier half of a statement is always something you typed.

The macro's own errors name the fix:

| Mistake | What you get |
| --- | --- |
| `sql!("select {}")` | `an empty {} binds nothing` / `help: name the value: {user_id}` |
| `sql!("select {a")` | `this { is never closed` |
| `sql!("100} off")` | `this } closes nothing` / `help: write }} for a literal brace` |
| `sql!("select 1", id)` | `sql! takes one string literal and no arguments` |

### Placeholders are PostgreSQL's spelling

`sql!` writes `$1`, `$2` and the text reaches the driver verbatim. Nothing renumbers it, because a
hand-written statement is written against one backend by definition. Both drivers Moso ships accept
that spelling: the round-trip suite runs the same `sql!` on SQLite everywhere, and on PostgreSQL
wherever `DATABASE_URL` is set. The SQL around the placeholders is yours to keep portable.

The `?` and `??` convention further down this page belongs to the builder's fragment types, not to
`sql!` or `RawQuery`. In those two, a `?` is a question mark on its way to the server, which is a
syntax error on PostgreSQL.

## Getting typed rows back

`RawQuery`'s terminals are generic over the row type, so you need either a turbofish or an annotated
binding.

| Method | Returns | Decodes with |
| --- | --- | --- |
| `fetch_all::<E>(ex)` | `Vec<E>` | `E::from_row`, positionally |
| `fetch_one::<E>(ex)` | `E`, or `Error::NotFound { entity }` | `E::from_row` |
| `fetch_optional::<E>(ex)` | `Option<E>` | `E::from_row` |
| `project_all::<P>(ex)` | `Vec<P>` | `P::from_row` |
| `execute(ex)` | `u64` rows affected | nothing |

`ex` is anything that implements `Executor`: `&Db`, `&Tx`, `&mut Tx` or `&RequestTx`. The trait is
sealed, so those four are the whole list, and a raw statement inside a transaction is just
`sql!("…").execute(&mut tx).await?`.

```rust
// A turbofish, or an annotated binding: `let users: Vec<User> = …fetch_all(db)`.
let users = moso::sql!("select id, email, is_admin from users where id = {id}")
    .fetch_all::<User>(db)
    .await?;
```

### The select list must match, in order

Decoding is positional. `E::from_row` reads column 0, then column 1, with literal indices, in the
order the fields are declared on the entity. That is what makes the built query fast, and it is what
makes a raw query fragile: your select list has to line up with the target type's field order, not
with what reads nicely. `select *` is the usual way to get this wrong, because the server returns the
table's own column order.

A drifted list is a `DecodeError`, and it says so:

```text
User::email reads column 1, and the row has fewer columns
  help: the query's select list does not match the entity; rebuild it with `Entity::query()`, or fix the projection
```

Two ways to avoid the problem entirely:

- **Project instead.** A `#[derive(Projection)]` struct is usually the right target for a raw query,
  because you write its fields in the order your select list already has. Only `from_row` is used
  when decoding a raw result: `select_items` is never called, so the projection's attributes do not
  have to describe the statement you wrote.
- **Read the order from the entity.** `<User as Entity>::COLUMNS` is a `const` slice of `ColumnDef`,
  and `ColumnDef::name()` gives the column name. Generating the list from it keeps the two in step.

## Building a `RawQuery` by hand

The macro is a convenience over a type you can use directly, which is what you want when the
statement text is a constant or is assembled from a fixed set of pieces.

```rust
use moso::db::prelude::*;

let query = RawQuery::new("select id, email, is_admin from users where created_at > $1")
    .bind(cutoff);

let recent: Vec<User> = query.fetch_all(db).await?;
```

| Method | What it does |
| --- | --- |
| `RawQuery::new(text)` | a statement with no parameters yet |
| `bind(value)` | binds one `impl SqlType`, in order |
| `bind_text(&str)` | binds a borrowed string without an intermediate `String` |
| `bind_value(Value)` | binds an already-built `moso::sql::Value` |
| `read_only()` | declares the statement a read (see the gaps at the end of this page) |
| `text()` / `args()` / `is_read_only()` | inspection |
| `into_sql()` | the `Sql { text, args }` pair the executor runs |

`&str` is deliberately not a `SqlType`, because decoding one would have to hand back a borrow of the
row's buffer. That is why `bind_text` exists. It also means `sql!("… = {name}")` where `name` is a
`&str` does not compile; write `{name.to_owned()}`.

`RawQuery`'s `Debug` prints the text and the *count* of the arguments, never their values, so a
statement that lands in a log cannot take somebody's password with it. `Sql`'s `Display` behaves the
same way.

### Raw statements are traced and counted like any other

A raw query goes through the same executor path as a built one, so it is timed, traced, exported to
metrics, and counted:

```rust
let mark = db.statements().mark();
let rows: Vec<User> = moso::sql!("select id, email, is_admin from users").fetch_all(db).await?;
assert_eq!(db.statements().since(mark), 1);
```

Driver errors are translated too. A raw `insert` that trips a unique index still comes back as
`Error::UniqueViolation`, carrying the constraint, the columns and the SQLSTATE, exactly as a built
insert would. The one difference: the violation names the entity as `row`, because a raw statement
does not know one. When you want the real name, or the rows undecoded, go through the handle:

```rust
let rows = db
    .handle()
    .for_entity("User")
    .fetch_all_sql(query.into_sql())
    .await?;
```

That returns `Vec<Row>`, which you decode yourself: `Row::get::<T>(index)`, `get_opt::<T>(index)`,
the typed getters (`get_i64`, `get_string`, `get_uuid`, `get_timestamp`, `get_decimal`,
`get_json_text` and the rest), plus `len()`, `is_null(index)` and `column_name(index)`.

## Raw fragments inside a typed query

Dropping a whole query to raw SQL because one operator has no wrapper is a bad trade. Two functions
let you keep the builder and escape only the fragment.

```rust
use moso::db::expr::{raw, raw_with};
use moso::sql::Value;

// No parameters.
let overlapping = Event::query().filter(raw("age_range @> 30"));

// With parameters: `?` is a placeholder, `??` is a literal question mark.
let overlapping = Event::query().filter(raw_with("age_range @> ?", [Value::I32(30)]));
```

The `?` convention comes from `moso::sql::RawExpr`, the type both functions wrap. The dialect
renumbers those placeholders into its own spelling, so the same fragment renders on PostgreSQL and
SQLite, and it is numbered *after* whatever the builder already bound:

```rust
use moso::sql::{Expr, Ident, RawExpr, Select, TableRef};

Select::from_table(TableRef::from_static("t"))
    .select_all()
    .filter(Expr::col(Ident::from_static("id")).eq(Expr::value(7)))
    .filter(Expr::raw(
        RawExpr::new("created_at > now() - ?::interval").bind("1 day"),
    ));
// SELECT * FROM "t" WHERE "id" = $1 AND (created_at > now() - $2::interval)
// args: [I32(7), Text("1 day")]
```

A placeholder count that does not match the bound values is caught at build time, before anything is
sent:

```text
the raw SQL fragment has 1 placeholder(s) and 0 bound value(s)
fragment: a = ?
help: bind one value per `?`; write `??` for a literal question mark
```

> [!WARNING]
> Because `?` is the placeholder, PostgreSQL's `jsonb` existence operators have to be doubled inside
> a fragment: write `data ?? 'key'` to get `data ? 'key'`. This rule applies to `RawExpr` and
> `RawStatement`. In a `sql!` literal or a `RawQuery` text, only braces are special.

### A raw fragment in a projection

`#[derive(Projection)]` takes an expression per field:

```rust
#[derive(Projection)]
#[projection(entity = User, join = Post)]
pub struct UserSummary {
    /// The user's key.
    pub id: Id<User>,
    /// How many posts they have.
    #[projection(expr = "count(posts.id)")]
    pub post_count: i64,
}
```

The fragment is emitted verbatim into the select list, so the join it names has to be one the query
actually made (`User::query().left_join(User::POSTS).group_by(User::ID.expr())` here). The
hand-written equivalents are `moso::db::projection::raw_expr(fragment)` and
`raw_expr_as(fragment, alias)`, neither of which is scope-checked. Both build a `RawExpr` with no
bound values, so the fragment must contain no `?`: a placeholder with nothing bound is the arity
error above. A projected expression that needs a parameter has to be built with
`moso::sql::RawExpr::with_args(..)` and placed with `moso::sql::SelectItem::aliased(..)`.

### Raw fragments skip the joined-set check

A `Predicate` normally records which entities' columns went into it, and building the statement
refuses a filter that mentions an entity the query never joined. A raw fragment records nothing:
`expr::raw` returns `Predicate::unchecked`, whose entity set is empty. The same is true of
`raw_expr` in a projection, and of any bare `Expr` converted into a `Predicate`.

That is the honest answer (Moso cannot parse your fragment), but it means the fragment is the one
part of a query where an unjoined column reaches the server and comes back as a database error
rather than an `Error::Unjoined` naming your file and line.

### One layer down: `RawStatement`

`moso::sql::RawStatement` is the whole-statement hatch at the construction layer, for code that
produces a `moso::sql::Statement` rather than running a query: a migration, a schema tool, a custom
dialect. It shares `RawExpr`'s `?` and `??` convention and is renumbered per dialect.

Two sharp edges live there. `RawStatement::read_only()` marks the statement, but
`Statement::Raw(..).is_read_only()` is always `false`, deliberately, because guessing the other way
sends a write to a replica. And `Ddl::Raw(..).is_destructive()` is always `true`, because nothing
parses the text and guessing the other way is how data disappears, so raw DDL in a migration always
needs the destructive acknowledgement. See [migrations](./migrations.md).

## Dynamic identifiers

None of these hatches lets you interpolate a table or column name, on purpose. A runtime string
becomes an identifier only through `moso::sql::Ident`, which validates it and which every dialect
emits quoted.

For the common case, a sort column chosen by the client, match over the entity's generated constants
and never touch a string:

```rust
let term = match sort {
    "title" => Post::TITLE.asc(),
    "views" => Post::VIEWS.desc(),
    _ => Post::ID.asc(),
};
let page = Post::query().order_by(term).limit(50).fetch_all(db).await?;
```

`Ident::new(name)` exists for the genuinely dynamic case and returns `Result<Ident, IdentError>`. It
rejects an empty name, anything over 63 bytes, and any ASCII control character, double quote,
backtick or backslash. It accepts a great deal else (spaces, semicolons, `--`, quotes, non-ASCII)
because output is always quoted, so a legitimate column called `order` or `full name` stays usable.

> [!IMPORTANT]
> `Ident` closes the injection door, not the authorization door. A validated identifier cannot break
> out of its quotes, but it can still name `password_hash`. Check the name against a list of columns
> you are willing to expose.

## What the raw path does not do for you

This is the part worth reading twice. Everything the builder adds to a query, it adds while building
the statement. A raw statement never goes through that.

| The builder does this | A raw statement does not |
| --- | --- |
| Adds `deleted_at IS NULL` for a soft-deletable entity | you write it, or you read deleted rows |
| Adds the tenant predicate and refuses to render without one | no `Error::TenantMissing`, no filter, and cross-tenant rows if you forget |
| Caps an unbounded `fetch_all` at 10,000 rows with a `warn` | no cap at all: a raw `select *` buffers the whole table |
| Refuses an `update` or `delete` with no `WHERE` | no `Error::UnfilteredWrite` guard |
| Checks that filtered entities are joined | no scope check |
| Sets `updated_at` on an update of a `#[entity(timestamps)]` entity | the column keeps its old value |
| Bumps `version = version + 1` for `#[entity(versioned)]`, and matches on it after `.expecting_version(v)` | no bump, no predicate, no `Error::StaleWrite` |
| Turns a delete into a timestamp write for a soft-deletable entity | the row is gone |
| Refuses a write in a read-only transaction, using `Statement::is_read_only` | not classified; see the gaps below |

If your entity is tenant-scoped, treat a raw query over its table as a security review item. See
[multi-tenancy](./multi-tenancy.md).

## When to reach for it

| Situation | Reach for |
| --- | --- |
| One operator or function has no wrapper | `expr::raw_with` inside the built query |
| A computed column in a listing | `#[projection(expr = "…")]` |
| A recursive CTE, a window over a set operation, a query you already tuned by hand | `moso::sql!` |
| A bulk backfill inside a migration | `Migrator::execute` and `Migrator::batched`, see [migrations](./migrations.md) |
| `LISTEN`, `COPY`, a driver feature Moso does not model | `Db::postgres_pool()` and sqlx directly |
| Filtering, sorting, paginating, joining, upserting | the query builder |

The builder covers more than it looks like it does: CTEs (recursive and data-modifying ones
included), window functions with frames, `DISTINCT ON`, row locks with `SKIP LOCKED`, full-text
search, the `jsonb` operators, and `ON CONFLICT` with a partial-index target. Check what the query
builder covers before you drop out of it.

## The last hatch: sqlx's own pool

Everything sqlx can do that Moso does not model is one method away, at the price of adding sqlx to
your own manifest at the version Moso compiled against:

```toml title="Cargo.toml"
[dependencies]
sqlx = { version = "0.9", default-features = false, features = ["runtime-tokio", "postgres"] }
```

```rust
if let Some(pool) = db.postgres_pool() {
    let count: i64 = sqlx::query_scalar("select count(*) from users")
        .fetch_one(pool)
        .await?;
}
```

`Db::postgres_pool()` returns `Option<&sqlx::PgPool>` and `Db::sqlite_pool()` returns
`Option<&sqlx::SqlitePool>`. `Db` dispatches on a runtime `Backend`, so each returns `None` when the
handle is connected to the other one. There is no `Db::pool()`, whatever the older design documents
say.

Two costs:

- **The version is your problem.** The facade does not re-export sqlx, and `moso::deps` covers only
  the HTTP-side crates (`axum`, `bytes`, `http`, `serde`, `serde_json`, `tokio`, `tower`,
  `tower_http`, `tracing`). A mismatched major version means the pool type does not line up and
  nothing compiles.
- **Moso cannot see what you did.** A write through the pool does not move the read-your-writes
  window, so a later `db.read()` may go to a replica that has not caught up. Call `db.mark_write()`
  after such a write; the method is public for exactly this reason. The same applies to a raw
  statement that starts with `with`, because the executor classifies a statement by its first keyword
  and treats `with` as a read, so a data-modifying CTE writes without moving the window.

## What the sealed facade means for your imports

`moso-sql` is a sealed construction facade: its public API is entirely Moso-owned types, and
`cargo xt check-sealed` fails the build on any foreign path that escapes it. Here is what that
sealing changes for you.

**You can import the whole SQL layer.** `moso::sql` is `moso-sql` re-exported behind the `orm`
feature. `moso::sql::{Expr, Ident, RawExpr, RawStatement, Value, TableRef, Postgres, Sqlite, Sql}`
are all yours, and so is `moso::sql::ddl` for schema statements. The crate exports no macros and no
attributes: unlike every neighbouring Moso crate, its whole surface is types and methods.

**You cannot import the engine underneath it.** No `sea-query` type appears in any Moso signature,
anywhere. That is enforced by a CI gate whose allowlist for `moso-sql` is deliberately empty. The
payoff is that the engine can be replaced in a patch release. The cost is a handful of Moso-owned
scalars where you might have expected a well-known crate's: `moso::sql::Uuid` is sixteen bytes rather
than `uuid::Uuid`, `Decimal` is a mantissa and a scale, and there are five hand-rolled date and time
types. The ORM converts at its own boundary, so entity fields still use `chrono` and `uuid`.

**There is no `Serialize` bound in `moso-sql`.** `Json` has no `from_serialize`. You serialise and
hand over text:

```rust
use moso::sql::Json;

let text = serde_json::to_string(&preferences)?;
let document = Json::from_json_string(text)?;
```

At the ORM level this is smoother: `Json<T>` in an entity field does the serialising for you, and
`Column::contains_json` takes JSON text.

**sqlx is deliberately not sealed.** Construction is sealed; execution is not. `Db::postgres_pool()`
is a documented escape hatch, so sqlx's major version is part of Moso's semver contract, and the
allowlist names `sqlx::pool::Pool`, `sqlx_postgres::PgPool` and `sqlx_sqlite::SqlitePool` one by one.

**There are exactly two doors from a runtime string into a built statement.** An identifier goes
through `Ident`, which validates it and which every dialect emits quoted. A value goes through
`Value`, which is bound. There is no third door: no `raw_column`, no unchecked identifier
constructor. The escape hatches on this page do not open one either, because the fragment is a
literal you wrote and its parameters are bound like any other. What a raw fragment does skip is the
joined-set check, which is a correctness check rather than a safety one.

## Failure modes

| What you did | What happens |
| --- | --- |
| Interpolated a table name with `{table}` | it binds a value; the statement is a syntax error at the server |
| Bound a `&str` with `sql!("… {name}")` | compile error: `&str` is not a `SqlType`. Write `{name.to_owned()}` |
| Selected columns in the wrong order for an entity | `Error::Decode` naming the field and the index |
| Selected fewer columns than the type reads | a `MissingColumn` decode error with the select-list help line |
| Read a `NULL` into a non-`Option` field | an `UnexpectedNull` decode error, help: make it `Option<T>` |
| `fetch_one` on a raw query that matched nothing | `Error::NotFound { entity }` |
| Bound the wrong number of values to a `RawExpr` | `Error::Build` wrapping `RawArity`, at build time |
| Used `?` in a fragment meaning the `jsonb` operator | it became a placeholder; write `??` |
| Wrote `?` placeholders in a `RawQuery` against PostgreSQL | a syntax error at the server; write `$1` |
| Ran an unbounded raw `select *` | the whole table is buffered; the 10,000-row cap does not apply |
| Ran a raw query over a tenant-scoped table | no tenant filter and no error; other tenants' rows come back |
| Wrote with a raw `with … as (delete …)` and then read | the read-your-writes window did not move; call `db.mark_write()` |
| Wrote through `Db::postgres_pool()` and then read | the read may hit a stale replica; call `db.mark_write()` |

## Reserved surface and edges to know

A few pieces of this surface are reserved for forward compatibility or resolve differently than you
might first assume. Reach for the alternatives noted here.

- **`RawQuery::read_only()` is reserved, not yet acted on.** The flag is stored and readable through
  `is_read_only()`, but `into_sql()` drops it and the executor does not consult it, so treat it as a
  forward-compatibility marker rather than an enforcement point. To route a read to a replica, choose
  the handle with `db.read()`. The read-only transaction guard reads `Statement::is_read_only`, and a
  raw query bypasses the `Statement` path entirely, so a raw write inside a
  `TxOptions::new().read_only()` transaction is refused by PostgreSQL rather than by Moso, and on
  SQLite is not refused at all.
- **`Typed<T>` is unreachable from the macro.** It is documented as the `sql!` macro's turbofish-free
  form, but nothing constructs one: the macro expands to a bare `RawQuery`. Build one by hand with
  `Typed::new(query)`, or use the turbofish.
- **`Projection::COLUMNS` is not checked.** The constant is emitted by the derive and defaults to the
  `usize::MAX` sentinel for hand-written impls, but no code compares it against the row's width. A
  select list that is too short fails per column instead, which is a worse message than the arity
  error the constant exists to enable.

## See also

- [Migrations](./migrations.md) for raw SQL in a schema change or a backfill.
- [Transactions and pooling](./transactions.md) for `Tx`, replicas and read-your-writes.
- [Multi-tenancy](./multi-tenancy.md), which raw SQL does not enforce.
- [Security](./security.md) for the injection argument in full.
- [Testing](./testing.md) for counting the statements a raw query issues.
