# 20 — The Data Layer: Strategy & Risk Containment

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

> This is the highest-risk part of Moso. Read this document before writing any ORM code.

## The evidence

| Prior attempt | Outcome | Lesson |
| --- | --- | --- |
| **Diesel** | Mature, compile-time-safe, widely used. Complaints: async bolted on, "40 lines of generic type vomit," dynamic queries fight you. | Type-level query encoding buys safety and costs ergonomics. The cost is what stops adoption. |
| **SQLx** | Excellent, widely adopted — but **not an ORM**. No relations, no entity mapping, minimal migrations, compile-time checking needs a live DB or an offline cache. | Great execution layer. Build on it, don't compete with it. |
| **SeaORM** | Ergonomic, async-first, real relations, real adoption. Complaints: verbose, "magic," no query pipelining. | Proof that the ActiveRecord shape sells in Rust. Verbosity is the beatable weakness. |
| **Prisma Client Rust** | **Abandoned** (archived Sept 2024). Prisma itself removed its Rust query engine. | Generating a client from a foreign schema language, maintained by one person, does not survive. |
| **Toasty** | Early, from a credible author, explicitly prioritising ease of use. Not ready. | The gap is real enough that Tokio's lead is working on it. |
| **Cot's ORM** | Self-described "very lacking"; auto-migrations cover a small subset of operations. | Building a framework *and* an ORM at once starves both. |

**The synthesis:** the unmet need is not a new query engine. It is **ergonomics, relations,
migrations, and error messages** on top of an execution layer that already works.

## Strategy: build the thin layer, seal the thick one

```
   moso-orm       ← WE BUILD THIS. Entity, Column, Select<E>, relations, preload,
                     pagination, transactions, projections, migrations-facing schema descriptor.
        │            This is where the entire differentiation budget goes.
        ▼
   moso-sql       ← SEALED FACADE. Our types in, SQL out. Initially delegates to `sea-query`.
        │            No foreign type appears in any public signature. Replaceable.
        ▼
   sqlx           ← EXECUTION. Pool, drivers, type mapping, prepared statements, streaming.
                     We use it directly and expose it (`Db::pool()`).
```

### Why a sealed facade rather than depending on `sea-query` openly

Because the reversal cost is otherwise catastrophic. If `sea-query` proves limiting (dialect gaps,
CTEs, window functions, JSON operators, `RETURNING` edge cases), we want to replace it in a patch
release. If its types are in our public API — even in one `impl Trait` return — we cannot.

`xtask check-sealed` parses `cargo rustdoc --output-format json` for `moso-orm` and `moso-sql` and
fails the build if any path outside `moso_*`, `std`, `core`, or an explicit allowlist appears in a
public signature. This runs on every PR.

### Why sqlx and not something else

- Async-native, Tokio-first, mature pooling, `PgListener` for LISTEN/NOTIFY, streaming, real
  transaction support, `Encode`/`Decode` covering the types people actually use.
- We use its runtime API (`query_as_with`, `Executor`), **not** its compile-time macros
  (`query!`), so no live database is required at build time. This is important: needing a database
  to compile is a Loop-1 killer for evaluators and a CI nuisance.
- Its escape hatch is our escape hatch: `Db::pool() -> &PgPool` gives users every sqlx feature we
  did not wrap.

## The non-negotiables (the reason to build this at all)

These are the acceptance bar. An ORM that misses any of them is not worth shipping.

### N1 — Shape-stable query builders
`Select<User>` is `Select<User>` after any number of `.filter()`, `.join()`, `.order_by()` calls.
Clauses accumulate in runtime `Vec`s. Type safety lives at the *expression construction site*
(`User::AGE.gt(18)` is checked; `User::AGE.gt("x")` does not compile), not in the builder's type.

Consequence: dynamic queries are trivial, and no user ever sees a 40-line type. This is the single
biggest ergonomic win over Diesel and it is achieved by *giving up* type-level query encoding —
a deliberate, documented trade (ADR-0007).

### N2 — No implicit lazy loading, ever
Accessing an unloaded relation returns `Err(NotLoaded)` — it never silently issues a query.
Implicit lazy loading is how ORMs produce N+1 in production. Preloading is explicit:
`.with(User::POSTS)`. `moso check` flags a `Result<_, NotLoaded>` that is `unwrap`ed in a loop.

### N3 — Batched eager loading
`.with(User::POSTS)` executes **2** queries for any number of users, not N+1. Nested preloads
(`.with(User::POSTS.with(Post::COMMENTS))`) execute 3. Verified by a test that counts statements.

### N4 — Ergonomic dynamic queries
```rust
// example — this must be this easy
let mut q = Product::query();
if let Some(c) = f.category { q = q.filter(Product::CATEGORY_ID.eq(c)); }
if let Some(s) = f.search   { q = q.filter(Product::NAME.ilike(format!("%{s}%"))); }
if f.in_stock               { q = q.filter(Product::STOCK.gt(0)); }
q = match f.sort {
    Sort::Price => q.order_by(Product::PRICE.asc()),
    Sort::New   => q.order_by(Product::CREATED_AT.desc()),
};
let page = q.paginate(f.cursor, f.limit).fetch(&db).await?;
```

### N5 — Typed partial selects
`User::query().select((User::ID, User::EMAIL))` returns `(Id<User>, Email)` tuples, or a
`#[derive(Projection)]` struct. Selecting a column you did not project is a compile error.

### N6 — Migrations generated from entities, always reviewable
`moso db make-migration` diffs the entity graph against `migrations/.schema.json` and writes a SQL
file you read before running. Never auto-applied in production. Destructive operations require an
explicit acknowledgement.

### N7 — Errors that name the problem
A unique-violation becomes a 409 with the offending field's JSON Pointer. A missing column at
runtime is impossible (columns are constants). A type mismatch is a compile error naming the column
and both types. Query errors include the SQL and the parameter shapes in dev.

### N8 — Full raw-SQL escape hatch
```rust
let rows = moso::sql!("select * from users where email = {email}").fetch_all::<User>(&db).await?;
let pool = db.pool();   // and everything sqlx can do
```
`sql!` is a macro that produces bound parameters from interpolated identifiers — never string
concatenation, so it cannot produce an injection.

## What we do NOT do

| Not doing | Why |
| --- | --- |
| Compile-time verified SQL against a live DB | Requires a database to build. sqlx already offers it; users who want it can use `sqlx::query!` alongside. |
| Type-level query encoding (Diesel-style) | The type vomit is the thing we exist to remove. |
| Dirty tracking / `save()` on arbitrary mutation | Hidden writes. Explicit `update()` builders instead. |
| An identity map / session cache | Aliasing bugs, surprising staleness, hard to reason about in async. |
| NoSQL in the same abstraction (Toasty's bet) | Doubles the surface for a fraction of our users. A separate `moso-kv` covers the KV case honestly. |
| A schema DSL file (Prisma-style) | Rust structs are the source of truth. One language. |

## Supported databases

| DB | Status | Notes |
| --- | --- | --- |
| PostgreSQL 14+ | **First class** | Every feature. The reference dialect. |
| SQLite 3.40+ | **Full support** | Dev, test, embedded, edge. Documented divergences (no `ILIKE` → `LIKE` with `NOCASE`, limited `ALTER TABLE` → table-rebuild migrations). |
| MySQL 8 / MariaDB | Best effort | Community-maintained. Feature matrix published; gaps are documented, not hidden. |

The `Dialect` trait is public so a third party can add a backend, but we make no promise to keep it
stable before 1.0.

## Benchmarks we commit to publishing

Against SeaORM and hand-written sqlx, on the same schema and machine, published in-repo and rerun in
CI:

| Scenario | Metric |
| --- | --- |
| Single-row lookup by PK | queries/s, allocations/query |
| 100-row select with 3 filters | queries/s, µs of query-construction overhead |
| Parent + children preload (100 + 1000 rows) | total statements, wall time |
| Insert of 1000 rows | wall time (batched vs looped) |
| Dynamic query with 5 optional filters | construction overhead |

The honest target: **within 15% of hand-written sqlx** for construction overhead, and **identical
statement counts** for relation loading. We publish where we lose, too. If we cannot beat SeaORM on
ergonomics *and* match it on performance, the strategy is wrong and we should know early.

## Kill criteria (from `05-delivery/50-roadmap.md`)

At the end of M2, if **any** of these is true, we abandon the bespoke ORM and ship a thin,
excellent *integration* with SeaORM instead (keeping the migration generator, the `Entity`-derived
OpenAPI/admin metadata, and the preload API as a layer on top):

- N1 or N3 is not demonstrably achieved on the reference app.
- The reference app's ORM-attributable compile time exceeds 20 s.
- Two consecutive milestones slip on data-layer work.
- Construction overhead is > 40% above hand-written sqlx.

Writing the kill criteria down before starting is the discipline that separates this from the
attempts that stalled.
