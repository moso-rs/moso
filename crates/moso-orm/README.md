# moso-orm

The ORM of [Moso](https://github.com/lowsbarrel/moso): entities, shape-stable
queries, N+1-safe relations, transactions and pooling over PostgreSQL and
SQLite.

Construction goes through [`moso-sql`](../moso-sql), the sealed facade
([ADR-0005](../../docs/adr/0005-sealed-sql-facade.md)); execution goes through
`sqlx`. This crate is the thin layer in between, and it is where the whole
differentiation budget goes.

```rust,ignore
use moso::db::prelude::*;

/// Someone who can sign in.
#[derive(Entity, Debug, Clone)]
#[entity(table = "users", timestamps, soft_delete = "deleted_at")]
pub struct User {
    /// The primary key.
    #[entity(pk, default = "uuid_generate_v7()")]
    pub id: Id<User>,

    /// Login identity.
    #[entity(unique, index)]
    pub email: Email,

    /// Everything this user wrote.
    #[entity(has_many = Post, fk = "author_id")]
    pub posts: Related<Vec<Post>>,
}

// Two statements, whatever the row count.
let admins = User::query()
    .filter(User::IS_ADMIN.eq(true))
    .with(User::POSTS)
    .fetch_all(&db)
    .await?;
```

## The eight non-negotiables

| | Promise |
| --- | --- |
| N1 | `Select<User>` stays `Select<User>` through any chain |
| N2 | `Related::get()` never queries - it returns `NotLoaded` |
| N3 | `.with(..)` is one extra statement, for any number of rows |
| N4 | `filter_opt` / `filter_if` / `when` / `join_if`, no type gymnastics |
| N5 | Typed partial selects, as tuples or a derived `Projection` |
| N6 | Migrations generated from `EntityDescriptor`, never auto-applied |
| N7 | A unique violation is a 409 with a JSON Pointer at the field |
| N8 | `RawQuery`, and `Db::postgres_pool()` for everything sqlx can do |

## Two decisions worth knowing

**The builder is shape-stable** ([ADR-0007](../../docs/adr/0007-shape-stable-query-builder.md)).
Type safety lives at the expression construction site - `Column<E, T>` - not in
the builder's type. `User::AGE.gt("x")` does not compile; no error message ever
prints a forty-line type.

**The joined-entity set is checked when the statement is built, not encoded in
the type.** The reasoning, and the message it produces, are in the
`predicate` module's documentation. The second parameter of `Select<E, J>` is
kept for the one obligation with no good runtime equivalent: a tenant scope,
whose failure mode is a silent cross-tenant read rather than a loud SQL error.

## Status

**Implemented.** Entities, shape-stable queries, N+1-safe relations, transactions and pooling all
ship over PostgreSQL and SQLite; no `todo!()` remains. Round-trip tests pass against real Postgres.
Run `cargo test -p moso-orm` to see what holds today.

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
