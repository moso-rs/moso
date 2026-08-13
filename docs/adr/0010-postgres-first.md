# ADR-0010 - Postgres first-class, SQLite full, MySQL best-effort

Status: Accepted
Date: 2026-07-29

## Context

Dialect breadth is where ORMs go to die: every feature must be designed against the least capable
supported database, or it must be conditional, and conditional behaviour multiplies the test matrix.

## Decision

| Database | Status | Meaning |
| --- | --- | --- |
| PostgreSQL 14+ | **First class** | Every feature. The reference dialect. All design decisions are made against it. |
| SQLite 3.40+ | **Full support** | Every feature works or has a documented, tested equivalent (table-rebuild migrations, `LIKE` + `NOCASE` for `ILIKE`, correlated subquery instead of window functions where needed). Dev, test, embedded, edge. |
| MySQL 8 / MariaDB | Best effort | Community-maintained. A published feature matrix names the gaps rather than hiding them. |

The `Dialect` trait is public so a third party can add a backend, but it carries no stability
promise before 1.0.

## Alternatives considered

- **Postgres only.** Cleaner, but it forfeits the "run the test suite with no external service"
  property that SQLite gives us, which is a first-five-minutes requirement.
- **Equal support for four databases.** Rejected: this produces a uniformly mediocre data layer and
  a test matrix that dominates CI time. Features like `RETURNING`, `SKIP LOCKED`, partial indexes,
  `jsonb`, and `LISTEN/NOTIFY` are load-bearing for Moso's jobs, KV, and ORM design.

## Consequences

- Postgres-specific features (partial indexes, `jsonb` operators, full-text search, `SKIP LOCKED`
  job queues, advisory locks, `LISTEN/NOTIFY`) are used freely and have documented SQLite
  equivalents or documented absences.
- The CI matrix runs Postgres and SQLite on every PR; MySQL runs nightly and its failures do not
  block a merge (they open an issue).
- Users on MySQL are told honestly what they are getting. That is better than a matrix of untested
  claims.

## Reversal criteria

- If MySQL demand is substantial among real users (measured, not assumed) and a maintainer steps up
  to own it, promote it to full support with its own gates.
