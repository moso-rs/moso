# ADR-0005 — Sealed query-construction facade over `sea-query`

Status: Accepted
Date: 2026-07-29

## Context

The ORM is the highest-risk component (see `02-data/20`). Every prior Rust attempt has stalled,
stayed niche, or is unfinished; Prisma removed its own Rust query engine. We must not write a query
engine, and we must not be trapped by the one we borrow.

## Decision

Introduce `moso-sql`: a **sealed facade** whose public API is entirely Moso-owned types
(`Select`, `Insert`, `Expr`, `Sql`, `Dialect`) and whose implementation initially delegates to
`sea-query`.

**No `sea-query` type may appear in any public signature anywhere in Moso.** This is enforced by
`xtask check-sealed`, which parses `cargo rustdoc --output-format json` and fails the build on any
foreign path in a public API, on every PR.

Execution is `sqlx`, used through its runtime API (not its compile-time `query!` macros, so no
database is needed to build). `Db::pool()` exposes the raw pool as a full escape hatch.

## Alternatives considered

- **Depend on `sea-query` openly.** Simpler, but if it proves limiting (CTEs, window functions,
  JSON operators, `RETURNING` edge cases, dialect gaps), replacing it becomes a breaking change.
  The whole point of the facade is that this reversal costs a patch release instead of a major one.
- **Write our own query AST from scratch.** More control, but it is exactly the "build a query
  engine" trap, and dialect coverage is where ORMs go to die.
- **Build on Diesel's query builder.** Type-level encoding conflicts directly with ADR-0007.

## Consequences

- A thin translation layer to write and maintain, and some `sea-query` capability we do not expose
  until someone needs it.
- Users cannot reach `sea-query` directly. This is deliberate and is stated in the docs; the
  documented escape hatches are `sql!` and `Db::pool()`.
- We can replace the internals without breaking anyone — which is the entire value of this ADR.

## Reversal criteria

This ADR is *designed* to be reversed cheaply. Trigger a replacement if:
- A required construct cannot be expressed and upstream will not take a patch within one release
  cycle.
- Query-construction overhead exceeds our budget (2 µs for a 5-filter query) because of the
  translation layer.
- `sea-query` maintenance stalls.

Replacement means rewriting `moso-sql`'s internals only. No user-visible change.
