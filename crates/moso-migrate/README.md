# moso-migrate

Moso's migration system: **entities in, a reviewable migration out, applied safely.**

Rust entities are the source of truth. A committed snapshot (`migrations/.schema.json`) records the
schema as of the last generated migration; `moso db make-migration` diffs the entities against it
and writes a SQL file you read before applying.

```
   src/models/*.rs  (#[derive(Entity)])
            │  EntityDescriptor
            ▼
   ┌─────────────────┐   diff    ┌────────────────────────────┐
   │ desired schema  │──────────▶│ migrations/00042_add_x.sql │
   └─────────────────┘           └────────────────────────────┘
            ▲                                  │ apply
   ┌─────────────────┐                         ▼
   │ migrations/     │◀──────── update ──── database
   │  .schema.json   │                    (moso_migrations table)
   └─────────────────┘
```

## What you get

- **A differ that handles every row** of the operation-coverage table in
  `docs/02-data/23-migrations.md` — new/removed entities and fields, renames, type changes,
  nullability, defaults, indexes, unique constraints, foreign keys, checks, enum variants,
  many-to-many join tables, schemas, extensions and partitioning.
- **The zero-downtime idioms, by default.** `CREATE INDEX CONCURRENTLY`;
  `ADD CONSTRAINT … NOT VALID` then `VALIDATE CONSTRAINT`; `CREATE UNIQUE INDEX CONCURRENTLY` then
  `ADD CONSTRAINT … USING INDEX`. Not decorations — the difference between a deploy and an outage.
- **The SQLite 12-step table rebuild**, generated automatically for every change SQLite cannot make
  in place, with several changes to one table collapsing into one rebuild — and gated like any other
  destructive change when the rebuild drops a column or narrows a type.
- **A runner that treats production as production**: lock guarded, transactional unless the file says
  otherwise, dirty state with the exact failing statement, checksums, `lock_timeout` and
  `statement_timeout` on every migration, out-of-order detection, and `--allow-destructive` refused
  under a production profile because the acknowledgement should be a committed diff.
- **A lock that survives a killed migrator** on both backends: `pg_advisory_lock` dies with the
  session, and the SQLite lock row is leased, renewed and reaped.
- **`moso db check`** — drift in both directions, against a live database.
- **Squash** (with Django-style `replaces`, so old databases do not re-run the baseline) and
  **seeds** (refused in production without `--force`).
- **One entry point per `moso db` subcommand** in [`command`](src/command.rs) —
  `make_migration`, `check`, `squash`, `seed`, `migrate_tenants` — each returning a
  `serde::Serialize` report, so the CLI can drive them over its `--db-*` protocol without linking
  this crate.

## The three guarantees

| Guarantee | What it asserts |
| --- | --- |
| Round trip | apply every migration to a fresh database, read the schema back, compare with the snapshot |
| Reversibility | apply → rollback → apply produces the same schema each time |
| Idempotence | `make-migration` twice in a row produces exactly one migration |

All three run on **PostgreSQL and SQLite**. The PostgreSQL half gates on `DATABASE_URL` and skips
with a message when it is not set:

```console
$ cargo test -p moso-migrate                                    # SQLite only
$ DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test \
  cargo test -p moso-migrate                                    # both
```

## Using it

```rust,ignore
use moso_migrate::command::{self, MakeMigrationOptions};
use moso_migrate::prelude::*;
use moso_orm::Backend;

// moso db make-migration — one call, one JSON-serialisable report
let report = command::make_migration(
    "migrations",
    Backend::Postgres,
    &entities,
    &MakeMigrationOptions::default(),
)?;
for advice in report.advice() {
    eprintln!("warning: {}\n{}", advice.summary(), advice.plan());
}

// moso db migrate
let mut runner = Runner::open("migrations", &database_url).await?;
let report = runner.migrate(&RunnerOptions::default().profile(&profile)).await?;
```

## Two decisions worth knowing

**This crate renders its own SQL rather than calling `Dialect::build`.** A migration file is text a
human reviews and pastes into `psql`; `Sql { text, args }` with `$1` placeholders is exactly wrong
for that. `moso_sql::ddl::Ddl` is still the intermediate representation — the operation table is
expressed in types, not strings — and `moso_migrate::emit` turns it into standalone text with
literals inlined and every identifier quoted.

**The runner opens its own connection rather than borrowing a pool.** `pg_advisory_lock` is per
session, `SET lock_timeout` is session state that must not leak to a request handler, and
`CREATE INDEX CONCURRENTLY` needs a connection the runner controls entirely.

## Licence

MIT, like the rest of Moso — see the root [`LICENSE`](../../LICENSE).
