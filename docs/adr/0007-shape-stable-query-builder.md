# ADR-0007 - Shape-stable query builder; no type-level query encoding

Status: Accepted
Date: 2026-07-29

## Context

There are two philosophies for a typed query builder in Rust:

1. **Type-level encoding** (Diesel). Each clause changes the builder's type, so the compiler proves
   the query is well-formed. The documented cost: "40 lines of generic type vomit" in error
   messages, and dynamic queries that "fight you" because the builder wants a static query shape.
2. **Runtime accumulation** (SeaORM, and most ORMs in other languages). Clauses go into `Vec`s; the
   builder's type is stable. Type errors are caught at expression construction, not at composition.

This is the single most consequential ergonomic decision in the data layer.

## Decision

**Shape-stable builders.** `Select<E>` remains `Select<E>` after any number of `.filter()`,
`.order_by()`, `.join()` calls. Type safety lives at the expression construction site:
`User::AGE.gt(18)` type-checks against `Column<User, i32>`; `User::AGE.gt("x")` does not compile,
with a hand-written diagnostic naming the column and both types.

One deliberate exception: `Select<E, J>` carries a joined-entity set so that filtering on an
unjoined entity's column is a compile error rather than a runtime SQL failure. `J` defaults to `()`
and is invisible in the common case. **This exception is itself under review** - see the pending
decision in `adr/README.md`; if it degrades error messages in practice, we fall back to a runtime
check with an equally good message.

## Alternatives considered

- **Diesel-style type-level encoding.** Rejected: the type vomit is precisely the barrier we exist
  to remove, and dynamic queries - which every real API needs for filtering and sorting - are where
  it hurts most.
- **Fully untyped builder (strings).** Rejected: loses the compile-time column/type checking that
  makes an ORM worth using in Rust at all.

## Consequences

- We give up compile-time proof that a query is well-formed. Malformed queries - comparing columns
  from unjoined tables, aggregating without grouping - are caught by tests and by the database, not
  by the compiler. We accept this and compensate with snapshot tests of generated SQL for every
  construct, on every dialect.
- Dynamic queries are trivial: `filter_opt`, `filter_if`, `when` compose without any trait
  gymnastics, and scopes are ordinary methods returning `Self`.
- Error messages stay short. No user ever sees a type longer than
  `moso::db::Select<shop::models::User>`.
- Monomorphisation is bounded, which materially helps compile times (rule A1 in `04-devex/42`).

## Reversal criteria

Reversing this would be a rewrite of the query layer and all user code - treat it as effectively
irreversible after M2. The evidence that would have changed the decision, gathered before M2 exit:

- If the class of bugs caught by type-level encoding turns out to be common in practice (measured on
  the reference apps and early users), we would add targeted compile-time checks for *specific*
  constructs rather than adopting full type-level encoding.
