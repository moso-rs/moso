# moso-sql

The sealed SQL-construction facade of [Moso](https://github.com/lowsbarrel/moso).

Moso-owned types go in; a `Sql { text, args }` pair comes out, rendered for a
`Dialect` (`Postgres` or `Sqlite`). Every type in the public API belongs to
Moso, so the engine underneath can be replaced in a patch release — that is
[ADR-0005](../../docs/adr/0005-sealed-sql-facade.md), and `xtask check-sealed`
fails the build the moment a foreign path reaches a public signature.

```rust
use moso_sql::{Expr, Ident, Postgres, Select, TableRef};

let users = TableRef::new(Ident::from_static("users"));
let query = Select::from_table(users)
    .select_all()
    .filter(Expr::col(Ident::from_static("is_admin")).eq(Expr::value(true)));

let sql = query.build(&Postgres)?;
assert_eq!(sql.text, r#"SELECT * FROM "users" WHERE "is_admin" = $1"#);
```

Identifiers can only be built through [`Ident`], which validates its input, so
a runtime string can never become a table or column name by accident. Values
can only be bound as parameters. SQL injection is therefore structurally
impossible rather than merely avoided.

This crate is not meant to be used directly: `moso-orm` is the ergonomic layer
on top of it, and `moso::sql!` plus `Db::pool()` are the documented escape
hatches for anything neither covers.

Licensed under MIT — see the root [`LICENSE`](../../LICENSE).
