---
title: Migrations
description: Generate migrations by diffing your entities against a committed snapshot, review the SQL before it runs, and apply it under a lock with a checksum ledger and a destructive-change gate.
order: 18
status: shipped
---

Your entities are the source of truth for the database shape. `moso-migrate` diffs them against a
snapshot committed next to your migration files, writes a reviewable `.sql` file, and applies it
under an advisory lock with a checksum ledger, per-file timeouts, and a gate in front of anything
that destroys data. Generation never touches a database, so the same entities produce the same bytes
on every machine and running the generator twice produces exactly one migration.

This page covers the whole loop: generating, reading and editing what came out, applying and rolling
back, the ledger and how to repair it, the destructive-change gate, the advice the generator prints,
renames, squashing, seeds and drift checking. Every one of them has a `moso db` subcommand; three of
them also need a list only your crate can write, and this page says which and where.

> [!IMPORTANT]
> The library is shipped and tested on both PostgreSQL and SQLite, and every entry point now has a
> command: `moso db status`, `migrate`, `migrate --all-tenants`, `rollback`, `redo`,
> `make-migration`, `check`, `squash` and `seed`. Each one runs your binary with a `--db-*` flag and
> renders the one JSON document it answers with, so what the CLI prints is what
> [`moso_migrate::command`](#the-command-entry-points) returned.
>
> Three of them need something only your crate can supply, and `src/db.rs` is where it goes:
> `make-migration`, `check` and `squash` read the **entity list**, `migrate --all-tenants` reads the
> **tenant list**, and `seed` reads the **seed registry**. There is no link-time registry
> (ADR-0004), so none of the three can be discovered, and the three entity commands refuse while
> the list is empty rather than reporting every table in your database as drift. The edges worth
> knowing are collected under [scope and edges to know](#scope-and-edges-to-know).

## The loop

1. Edit an entity.
2. `moso db make-migration <name>`. It reads `migrations/.schema.json`, builds the desired schema from
   your entity descriptors, diffs the two, and writes `migrations/<version>_<name>.sql` plus an
   updated snapshot. The version is a UTC timestamp, so the file lands as
   `YYYYMMDDTHHMMSS_<name>.sql`.
3. Read the file. Replace any placeholder backfill value, decide whether to uncomment the destructive
   block.
4. Commit the `.sql` and the `.schema.json` together.
5. `moso db migrate`.

Nothing runs at boot. There is no auto-migrate hook anywhere in the framework, by design: a migration
runs when a human or a pipeline runs it.

## Setup

`moso new <name> --with-db` gives you the four pieces:

| Piece | What it is |
| --- | --- |
| `migrations/` | The directory of `*.sql` files, plus `.schema.json` once you generate one. It ships with one hand-written `20260101T000000_init.sql` you can delete. |
| `src/db.rs` | Your composition root for the database. It answers the `--db-*` flags `moso db` sends, and it holds the four things only your crate can supply: the entity list, the tenant list, the seed registry, and `register` for Rust migrations. |
| `moso-migrate` in `Cargo.toml` | A direct dependency of your crate. |
| `DATABASE_URL` in `.env.example` | The one thing the runner needs to reach a server. |

```toml title="Cargo.toml"
[dependencies]
moso-migrate = "0.1"
```

`moso-migrate` is a separate crate and not a feature of the `moso` facade, because it pulls a
database driver (including a bundled SQLite that compiles from C) and the facade's default
dependency resolution is budgeted. Both backends compile in by default:

```toml
[features]
default = ["postgres", "sqlite"]
```

`DATABASE_URL` decides which one a process opens. `Connection::open` accepts `sqlite:`, `postgres:`
and `postgresql:` and refuses anything else by name. There is no MySQL backend. SQLite needs nothing
running; PostgreSQL does.

`moso db` does not link `moso-migrate`. It builds your binary and runs it with one of `--db-status`,
`--db-migrate`, `--db-migrate-tenants`, `--db-rollback <n>`, `--db-redo`,
`--db-make-migration <name>`, `--db-check`, `--db-squash` or `--db-seed [name]`, then reads one JSON
document off standard output. That indirection exists because a migration may need your own code (see
[migrations written in Rust](#migrations-written-in-rust)), and a CLI that ran migrations itself
would silently skip those, and because the entity, tenant and seed lists are statements in your
crate that nothing else can read. If `src/db.rs` is missing, `moso db` says so before it builds
anything.

`moso dev` watches `migrations/` along with `src/`, so editing a migration triggers a rebuild.

## The command entry points

Five subcommands are **one library call each**, in `moso_migrate::command`. Each takes what a command
line carries and returns a `serde::Serialize` report whose field names are the JSON keys, so
`src/db.rs` is a match arm and a `println!`, `moso db --json` is that document passed straight
through, and the CLI never becomes the thing that decides what a migration is.

| Call | What it is | Report |
| --- | --- | --- |
| `command::make_migration(dir, backend, entities, &MakeMigrationOptions)` | diff the entities against the snapshot and write the files | `MakeMigrationReport` |
| `command::check(dir, url, entities).await` | drift in both directions, plus the pending list | `CheckReport` |
| `command::squash(dir, backend, entities, &SquashOptions)` | collapse every file into one baseline | `SquashReport` |
| `command::seed(url, &seeds, name, &SeedOptions).await` | run one registered seed, or all of them | `SeedReport` |
| `command::migrate_tenants(dir, &targets, &options, &register).await` | migrate a set of tenants | `TenantMigrateReport` |

Every report carries a `command` field so the documents are told apart, and the two that gate a
pipeline (`check` and `migrate_tenants`) carry `clean`, which is the boolean `moso db` turns into
exit code 1. Everything they compose (`Generator`, `Squash`, `Seeds`, `check::compare`, `Runner`)
stays public and does more than they do; these exist so the common case is one call with a stable
signature, not to hide the layer underneath.

## Generating a migration

```sh
moso db make-migration add_locale_to_users            # writes the .sql and the snapshot
moso db make-migration add_locale_to_users --dry-run  # prints them and writes nothing
```

The command prints the file it wrote, the SQL inside it and the expand/contract advice, and refuses
to replace a migration that is already on disk. What answers it is `src/db.rs` in your own project.
`moso new --with-db` writes the plumbing, and the entity list is the one thing you fill in:

```rust title="src/db.rs"
use moso::db::{Backend, Entity};
use moso_migrate::command::{self, MakeMigrationOptions, RenamePolicy};

use crate::models::{Customer, Order};

pub fn make_migration(name: Option<&str>, renames: Vec<String>) -> moso_migrate::Result<()> {
    let descriptors = [
        <Customer as Entity>::descriptor(),
        <Order as Entity>::descriptor(),
    ];

    let mut options = MakeMigrationOptions::default()
        .renames(RenamePolicy::Scripted { pairs: renames, strict: true });
    if let Some(name) = name {
        options = options.name(name);
    }

    let report = command::make_migration(MIGRATIONS, Backend::Postgres, &descriptors, &options)?;
    println!("{}", moso::deps::serde_json::to_string_pretty(&report).unwrap_or_default());
    Ok(())
}
```

The descriptor list is yours to assemble. Moso has no link-time registry, so nothing walks your crate
looking for `#[derive(Entity)]` types. An entity you forget to list looks like a table you want
dropped, so keep the list in one place and add to it when you add a model.

`MakeMigrationOptions` is a builder over `Default`, which is no name, `RenamePolicy::Refuse` and no
dry run. `RenamePolicy` is the mapping from flags to [rename oracles](#renames):

| Policy | Oracle | Where it belongs |
| --- | --- | --- |
| `Ask` | `Prompt::stdio()` | interactive development |
| `Scripted { pairs, strict }` | `Scripted::parse(pairs)`, strict or falling back to drop-and-add | scripted generation |
| `Refuse` (the default) | `RefuseToGuess` | CI |
| `DropAndAdd` | `DropAndAdd` | tests, and a first migration against an empty database |

`MakeMigrationReport` answers `has_changes()`, `was_written()`, `is_destructive()`, `version()`,
`name()`, `path()`, `snapshot_path()`, `changes()`, `advice()` and `migration()`. A `false` from
`has_changes()` is the idempotent "nothing to do". `dry_run()` builds everything and writes nothing,
which is what `migration()` is for.

The layer underneath is `Generator`, and it is what you want when the desired schema does not come
from entity descriptors.

```rust
use moso_migrate::generator::Generator;
use moso_migrate::rename::Prompt;

let generator = Generator::new(MIGRATIONS, Backend::Postgres);
if let Some(generated) = generator.make_migration(&descriptors, None, &Prompt::stdio())? {
    generated.write()?;
    eprintln!("wrote {}", generated.path().display());
}
```

`make_migration` returns `Result<Option<Generated>>`. `Ok(None)` means the snapshot and the entities
already agree, which is what makes the generator idempotent.

### What you get back

`Generated` holds everything in memory. Nothing reaches the filesystem until `write()`:

| Method | Returns |
| --- | --- |
| `id()` | The `MigrationId`: version plus slugified name. |
| `path()` | Where `write()` would put the `.sql`. |
| `migration()` | The migration text. |
| `snapshot_path()` / `snapshot()` | Where the new `.schema.json` goes, and its contents. |
| `diff()` | The `Diff` the file was built from. |
| `advice()` | Zero or more `Advice` values for changes that break rolling deploys. |
| `write()` | Writes both files. |

Skipping `write()` and printing `migration()` is the dry run. Add it to your own entry point as a
`--dry-run` flag; the generator does not need one, because it never writes without being asked.

### Inspecting the diff

`Generated::diff()` gives you the change list before you decide anything, which is how you build your
own prompt or fail a job:

```rust
for line in generated.diff().summary() {
    eprintln!("  {line}");
}
```

`Diff` also gives `changes()`, `len()`, `is_empty()`, `is_destructive()`, `destructive()`,
`requires_no_transaction()` and `suggested_name()`. Each `Change` answers `is_destructive()`,
`requires_no_transaction()`, `description()` and `table()`, and is `#[non_exhaustive]`, so match with
a `_` arm.

### Naming and versioning

Pass `Some("add user locale")` and it is slugified to `add_user_locale`. Pass `None` and the
generator suggests one from the diff: `create_users`, or `add_users_locale_and_3_more`.

The version is a UTC timestamp spelled `YYYYMMDDTHHMMSS`, 15 characters. If that version is already
taken, the generator bumps it by a second. Timestamps rather than sequence numbers means two
developers on two branches cannot collide, and the accepted cost is that "the newest migration" is
not always "the last one applied", which is why [out-of-order detection](#the-ledger) exists.

For a test, pin the clock:

```rust
use moso_migrate::Version;

let generator = Generator::new(&directory, backend)
    .at(Version::from_parts(2026, 7, 29, 10, 15, 0));
```

A `Version` is an identifier, not a validated timestamp. `Version::parse` takes 14 digits and hands
back six fields without checking the calendar, and the collision bump can carry a day past the end of
a month. Neither matters for ordering, which is all a version is used for.

### Diffing between two schemas

`make_migration_between(&before, &after, name, oracle)` takes two `Schema` values directly. Use it
when the desired schema comes from somewhere other than entity descriptors: a hand-built `Schema`, or
one read back from a live database with `moso_migrate::introspect::read_schema`.

`Schema::from_entities(descriptors)` is the only bridge from entities to a schema, and it is what
`make_migration` calls. It takes anything that iterates `&EntityDescriptor`:

```rust
let schema = Schema::from_entities([&user])?;
let users = schema.table("users").expect("one table per entity");
assert_eq!(users.primary_key(), ["id"]);
assert!(users.index("users_email_key").is_some(), "a unique column is a unique index");
```

A `#[entity(unique)]` column becomes a unique index named `<table>_<column>_key`, a `belongs_to`
becomes a foreign key, an `Option<T>` becomes a nullable column, `#[entity(schema = "...")]` puts the
table in a named schema, and relations are not columns. Three things are errors rather than
surprises: two entities mapping to the same table, a relation pointing at an entity that is not in
the list, and two entities declaring the same enum type differently. Each error names both sides.

## Reading what it produced

A generated migration is plain SQL with literals inlined and every identifier quoted. You can paste
it into `psql`. Here is the whole file for one new table:

```sql title="migrations/20260729T101500_create_users.sql"
-- 20260729T101500_create_users.sql
-- moso:generated-from .schema.json@c8e7305b
-- moso:reversible
--
-- create the table `users`

-- +migrate up

CREATE TABLE "users" (
    "id" bigserial NOT NULL PRIMARY KEY,
    "email" text NOT NULL,
    CONSTRAINT "users_email_key" UNIQUE ("email")
);

-- +migrate down

-- undo: create the table `users`
DROP TABLE "users";
```

The header comment lists every change. The `down` section is written in reverse operation order.

### A file with everything in it

This is the shape you actually have to read. The diff adds a `NOT NULL` column with no default, adds
an index, adds a check constraint, and drops a column:

```sql title="migrations/20260729T101500_add_users_locale_and_3_more.sql"
-- 20260729T101500_add_users_locale_and_3_more.sql
-- moso:generated-from .schema.json@8e547e87
-- moso:reversible
-- moso:transactional false
-- moso:destructive
--
-- add `users.locale`
-- index `users` as `idx_users_locale`
-- check `users_id_positive` on `users`
-- drop `users.legacy_id`

-- +migrate up

-- REVIEW: `users.locale` is NOT NULL with no default, so it is added nullable, backfilled
-- and then tightened. The backfill value '' is a placeholder, replace it before applying.
ALTER TABLE "users" ADD COLUMN "locale" text;
UPDATE "users" SET "locale" = '' WHERE "locale" IS NULL;
ALTER TABLE "users" ALTER COLUMN "locale" SET NOT NULL;

-- built `CONCURRENTLY`, so writes are not blocked while it builds; that is why this
-- migration runs outside a transaction
CREATE INDEX CONCURRENTLY "idx_users_locale" ON "users" ("locale");

-- added `NOT VALID` and validated separately, so the strong lock is held for the catalogue
-- change only
ALTER TABLE "users" ADD CONSTRAINT "users_id_positive" CHECK (id > 0) NOT VALID;
ALTER TABLE "users" VALIDATE CONSTRAINT "users_id_positive";

-- the data in `users.legacy_id` is not recoverable; the down migration adds the column back
-- empty and nullable
-- ⚠ DESTRUCTIVE: drop `users.legacy_id`.
-- Uncomment the block below to apply it, after confirming that no running version
-- of the application still depends on what it removes.
-- +migrate destructive
-- ALTER TABLE "users" DROP COLUMN "legacy_id"
-- ;
-- +migrate end

-- +migrate down

-- undo: drop `users.legacy_id`
ALTER TABLE "users" ADD COLUMN "legacy_id" integer;

-- undo: check `users_id_positive` on `users`
ALTER TABLE "users" DROP CONSTRAINT IF EXISTS "users_id_positive";

-- undo: index `users` as `idx_users_locale`
DROP INDEX CONCURRENTLY IF EXISTS "idx_users_locale";

-- undo: add `users.locale`
ALTER TABLE "users" DROP COLUMN IF EXISTS "locale";
```

Two edits are expected of you before this is committed. Replace `''` in the `UPDATE` with the value
you actually want, and decide about the destructive block. Everything else is ready.

### What to edit, and what not to

| Safe to edit | Leave alone |
| --- | --- |
| The placeholder backfill value under a `REVIEW:` comment. | The `-- moso:generated-from` line. It records which snapshot the file came from. |
| Uncommenting a destructive block, or deleting it. | The `-- +migrate` markers. Deleting `-- +migrate down` makes the file irreversible. |
| Adding statements, including ones the generator cannot know about. | The file name. The version is the ledger's primary key. |
| Raising a timeout with `-- moso:lock-timeout 30s`. | Anything in a file that has already been applied. That is a checksum mismatch. |

Editing an applied migration is caught, not ignored. If you must (a genuinely cosmetic reformat), see
[repairing the ledger](#repairing-it).

### The directive vocabulary

Header lines starting `-- moso:` are directives. Anything else after `-- moso:` is a parse error that
lists the valid set.

| Directive | Effect |
| --- | --- |
| `-- moso:reversible` | The file has a usable `down` section. |
| `-- moso:irreversible` | It does not. `rollback` will refuse. |
| `-- moso:transactional false` | Run outside a transaction. `false`, `no` and `off` mean false; anything else means true. |
| `-- moso:lock-timeout 30s` | Override this file's lock timeout. Default 5 s. |
| `-- moso:statement-timeout 5min` | Override this file's statement timeout. Default 60 s. |
| `-- moso:destructive` | Informational marker the generator writes when the file contains a destructive block. |
| `-- moso:replaces 20260101T000000,20260201T000000` | This file is a [squash baseline](#squashing) for those versions. |
| `-- moso:generated-from .schema.json@a91f2c` | Which snapshot it came from. |

Durations accept bare digits (seconds), `s`, `sec`, `secs`, `ms`, `min` and `m`.

Four body markers exist: `-- +migrate up`, `-- +migrate down`, `-- +migrate destructive` and
`-- +migrate end`. A `down` marker makes a file reversible whether or not the directive is present, so
a hand-written migration needs no header at all. A destructive block that is never closed with
`-- +migrate end` is a parse error.

Statement splitting understands line comments, nested block comments, single-quoted strings with
`''`, double-quoted identifiers and PostgreSQL dollar quoting, so a semicolon inside a `$$ ... $$`
function body does not split the definition in half.

### The snapshot

`migrations/.schema.json` is the schema as of the last generated migration. Commit it. It is
pretty-printed with ordered keys, and a column's type is one string rather than a nested object,
because the point is that a schema change reads as a two-line diff in a pull request.

```json title="migrations/.schema.json"
{
  "format": 1,
  "tables": {
    "users": {
      "name": "users",
      "entity": "User",
      "columns": [
        { "name": "id", "type": "bigserial", "field": "id" },
        { "name": "email", "type": "text", "field": "email" }
      ],
      "primary_key": [ "id" ],
      "indexes": {
        "users_email_key": {
          "name": "users_email_key",
          "columns": [ { "expr": "email" } ],
          "unique": true,
          "constraint": true
        }
      }
    }
  }
}
```

The snapshot is why generation works offline and why it is deterministic: the generator never has to
introspect a live database to know what it last intended. It is parsed with `deny_unknown_fields` and
it refuses a `format` newer than it understands, because a field this build ignores is a schema
element it would then propose to drop.

Types are spelled the PostgreSQL way on both backends. SQLite derives column affinity from substrings
of whatever type name you declare, so keeping the declared names identical is what makes drift
detection work on SQLite at all.

## What the generator writes for you

The zero-downtime idioms are the default, not an option. The operation table does not say "add an
index", it says `CREATE INDEX CONCURRENTLY`.

| Change | What is emitted |
| --- | --- |
| New index | `CREATE INDEX CONCURRENTLY` on PostgreSQL, which forces `-- moso:transactional false` on the whole file. |
| New unique constraint | `CREATE UNIQUE INDEX CONCURRENTLY`, then `ADD CONSTRAINT ... UNIQUE USING INDEX`. |
| New foreign key or check | `ADD CONSTRAINT ... NOT VALID`, then `VALIDATE CONSTRAINT`, so the strong lock covers the catalogue change only. |
| `NOT NULL` column with no default | Add it nullable, `UPDATE` with a placeholder, then `SET NOT NULL`, with a `REVIEW:` comment on top. |
| Type change the server will not cast implicitly | An explicit `USING` expression. |
| New enum label | `ALTER TYPE ... ADD VALUE`, which is irreversible (PostgreSQL has no `DROP VALUE`), so the file is marked `-- moso:irreversible`. |
| Removed or reordered enum label | A commented manual template, because there is no correct SQL to generate. |

Backfill values are always placeholders, chosen by type family: `''`, `0`, `false`,
`CURRENT_TIMESTAMP` or `'{}'`, always with a `REVIEW:` note. They are never guessed silently and they
are also never right. Replace them.

A consequence worth planning for: creating an index, dropping an index and adding an enum label all
force the migration outside a transaction on PostgreSQL, because `CONCURRENTLY` and `ADD VALUE`
cannot run inside one. Many ordinary migrations are therefore non-transactional, which means a
failure leaves a dirty ledger row rather than nothing. The `down` for a concurrent index is
`DROP INDEX CONCURRENTLY`, so rollback is likewise non-transactional.

### Operation coverage

The differ and the planner handle the following, and each row has a test against both a real
PostgreSQL server and SQLite. This list is an acceptance criterion, not an aspiration: an
auto-generator that covers a small subset is a generator you stop trusting.

| Area | Covered |
| --- | --- |
| Tables | Create, drop, rename. |
| Columns | Add, drop, rename, type change (with lossiness classified), nullability in both directions, default set, changed and dropped. |
| Keys | Primary-key change; foreign keys added and dropped with `ON DELETE`, `ON UPDATE` and deferrability. |
| Indexes | New, changed, renamed, dropped. Partial (`WHERE`), expression, sorted, `INCLUDE`, operator class, `NULLS NOT DISTINCT` and method (`gin`, `gist`, `brin`, and the rest). |
| Constraints | Unique constraints built the zero-downtime way; check constraints added and dropped, compared through an expression normaliser. |
| Enums | Created, dropped, extended with new labels. |
| Relations | Many-to-many join tables, with two foreign keys, a composite primary key and a reverse index. |
| Structure | Named schemas, PostgreSQL extensions, partitioned tables (`PARTITION BY RANGE`, `LIST`, `HASH`). |
| Documentation | Table and column comments on PostgreSQL. SQLite emits nothing, and says so. |

A changed index is a drop and a create, never an alter. Same for a changed foreign key and a changed
check constraint.

### On SQLite

SQLite is a full backend, not a toy: the point is that your test suite runs with no external service.
Where SQLite cannot alter a table in place, the planner generates the 12-step rebuild from the SQLite
manual, and several changes to one table collapse into one rebuild. The same diff as the PostgreSQL
example above produces this:

```sql title="migrations/20260729T101500_add_users_locale_and_3_more.sql"
-- 20260729T101500_add_users_locale_and_3_more.sql
-- moso:generated-from .schema.json@abc3790c
-- moso:reversible
-- moso:destructive
--
-- rebuild `users` (SQLite cannot alter it in place)

-- +migrate up

-- SQLite has no `ALTER COLUMN`, so the table is recreated, copied, dropped and renamed:
-- the 12-step recipe from the SQLite manual. Steps 1 and 12 (`PRAGMA foreign_keys`) are the
-- runner's job: a pragma inside a transaction is ignored.
-- the rebuild copies `users` into its new definition, so it destroys data: drop
-- `users.legacy_id`. The data is not recoverable, and the down migration recreates the lost
-- columns empty.
-- ⚠ DESTRUCTIVE: rebuild `users` (SQLite cannot alter it in place).
-- Uncomment the block below to apply it, after confirming that no running version
-- of the application still depends on what it removes.
-- +migrate destructive
-- CREATE TABLE "users__moso_new" (
--     "id" integer PRIMARY KEY AUTOINCREMENT,
--     "email" text NOT NULL,
--     "locale" text NOT NULL,
--     CONSTRAINT "users_id_positive" CHECK (id > 0)
-- )
-- ;
-- INSERT INTO "users__moso_new" ("id", "email", "locale") SELECT "id", "email", '' FROM "users"
-- ;
-- DROP TABLE "users"
-- ;
-- ALTER TABLE "users__moso_new" RENAME TO "users"
-- ;
-- CREATE INDEX "idx_users_locale" ON "users" ("locale")
-- ;
-- CREATE UNIQUE INDEX "users_email_key" ON "users" ("email")
-- ;
-- PRAGMA foreign_key_check
-- ;
-- +migrate end
```

The closing `PRAGMA foreign_key_check` is treated as a failure by the runner when it returns rows.
Type spellings are translated: `bytea` renders as `blob`, serial types as `integer`, arrays and enums
as `text`, and `UNLOGGED` is dropped.

### The rebuild does not launder the destructive gate

Notice the `-- moso:destructive` header and the commented block above. A rebuild copies the table
into its *new* definition, so a column the new definition does not have is simply not copied, which
destroys exactly as much data as `ALTER TABLE … DROP COLUMN` would. It is therefore gated exactly as
a standalone drop is: `moso db migrate` refuses the file until the block is uncommented, or
`allow_destructive` is set.

It is the whole rebuild that is commented out, not a line inside it, because the rebuild is one
operation and there is no half of it worth running. The `down` section is left uncommented, as it is
for any destructive operation, so a rollback still works.

A rebuild that destroys nothing is not gated. Widening `integer` to `bigint`, adding a check
constraint, changing a default: those force a rebuild too, and it applies without ceremony. The two
things that make a rebuild destructive are a column the new table does not have, and a type change
the differ cannot prove is lossless. Those are the same two answers `Change::is_destructive()` gives
everywhere else.

Earlier builds folded a column drop into the rebuild and lost the gate on the way, so a drop applied
without acknowledgement whenever some other change to the same table forced a rebuild. That was a
bug, and it is fixed.

A `gin`, `gist`, `brin` or `hash` index on SQLite is a hard error (`Error::Unsupported`) naming the
alternative, never a silent downgrade. Partitioning on SQLite is likewise an error.

## Renames

A diff cannot tell a rename from a drop-and-add, and the difference is whether the data survives.
That is not a question a machine can answer, so the generator asks an `Oracle`. You pass one to
`make_migration`.

| Oracle | Behaviour | Where it belongs |
| --- | --- | --- |
| `Prompt::stdio()` | Asks on the terminal, reading standard input and writing standard **error**, so it cannot corrupt the one JSON document `moso db` reads. Accepts `r`, `rename`, `renamed`, `y`, `yes` or `d`, `drop`, `dropped`, `n`, `no`. Re-asks twice, then errors with the flag to use. A closed input is an error, never a guess. | Interactive development. |
| `Scripted::parse(["users.name:full_name"])` | Answers from `--rename old:new` style arguments, and refuses anything they do not cover unless `otherwise(..)` says otherwise. | Scripted or repeatable generation. |
| `RefuseToGuess` | Always `Error::NeedsAnswer`, naming the flag that would answer it. | CI. |
| `DropAndAdd` | Everything is a drop and an add. | Tests, and a first migration against an empty database. |

```rust
use moso_migrate::rename::{Oracle, RenameAnswer, RenameQuestion, Scripted};

let oracle = Scripted::parse(["users.name:full_name"])?;
let question = RenameQuestion::column("users", "name", "full_name");
assert_eq!(oracle.answer(&question)?, RenameAnswer::Rename);
```

`Scripted::parse` is **strict by default**: a question its pairs do not cover is an error naming the
flag that would answer it. Ask it to fall back instead when you want that:

```rust
let strict = Scripted::parse(["a:b"])?;
assert!(strict.answer(&RenameQuestion::table("c", "d")).is_err());

let loose = Scripted::parse(["a:b"])?.otherwise(Some(RenameAnswer::DropAndAdd));
assert_eq!(loose.answer(&RenameQuestion::table("c", "d"))?, RenameAnswer::DropAndAdd);
```

`RenamePolicy::Scripted { pairs, strict }` is the same choice, spelled as a flag.

Being strict in CI is the point. A generator that guesses in CI produces a migration nobody reviewed
that either drops a column or does not, and the failure mode is discovered in production.

Implement `Oracle` yourself if you want a different interface. It is one method,
`answer(&RenameQuestion) -> Result<RenameAnswer>`, and `RenameQuestion` gives you `prompt()` and
`flag()` for rendering, plus `kind()`, `from()`, `to()` and `key()`.

Two limits. Renames are only offered for plausible candidates: tables must share an entity name or
half their column names, columns must match on everything but the name or share a Rust field name.
Anything outside those heuristics is reported as a drop and an add, which the destructive gate then
catches. And a `Scripted` pair whose left-hand side matches with a different right-hand side is
treated as a deliberate "no", answering `DropAndAdd` rather than erroring.

## Applying

```rust
use moso_migrate::{Runner, RunnerOptions};

let mut runner = Runner::open("migrations", "postgres://moso:moso@localhost/app").await?;
let report = runner.migrate(&RunnerOptions::default()).await?;
println!("{} applied", report.applied().len());
runner.close().await;
```

`Runner::open` takes one dedicated connection, not a pool. That is structural, not an optimisation:
`pg_advisory_lock` is per session, so a pool that hands out a different connection per statement
protects nothing; `SET lock_timeout` is session state that would otherwise leak to the next borrower,
which is a request handler; and `CREATE INDEX CONCURRENTLY` needs a connection the runner controls
entirely. See [Transactions and pooling](./transactions.md) for the pool the rest of your application
uses.

### What one run does, in order

1. `status()`: create `moso_migrations` if absent, read every row, classify the directory against it.
2. Turn the first problem into an error, in severity order: dirty, then checksum-changed, then
   missing. A checksum mismatch stops the run before anything is applied.
3. Refuse if anything pending sorts before something already applied, unless `allow_out_of_order`.
4. Trim the pending list to `up_to(target)` if one was given.
5. Ask every pending file for its statements. A file with an unacknowledged destructive block fails
   **here**, before the lock is taken and before any statement runs.
6. If `dry_run`, report the list and stop.
7. Take the lock. On PostgreSQL, `pg_try_advisory_lock(4355294045437474)` in a 250 ms poll loop up to
   `lock_wait`. On SQLite, reap any expired lock row and then `INSERT ... ON CONFLICT DO NOTHING`
   into `moso_migrations_lock`. See [the migration lock](#the-migration-lock).
8. Re-read the ledger. Another process may have applied some of these while this one waited; drop
   those from the pending list. This is what makes twenty pods starting at once safe, and there is a
   test with ten concurrent migrators proving one migration is applied exactly once.
9. Apply each remaining version in order.
10. Release the lock and return a `MigrateReport`.

Step 9 for one file: if it is a [squash baseline](#squashing) whose replaced versions are all already
applied, record it without running it. Otherwise set `lock_timeout` and `statement_timeout` from the
file (PostgreSQL only; SQLite's `busy_timeout` is set at connect time), then either `BEGIN`, ledger
row, statements, ledger finish, `COMMIT`, or, for a non-transactional file, write the ledger row
first as dirty, run the statements, then finish it.

`MigrateReport` answers `applied()` (version, name and duration for each), `skipped()`, `waited()`
(how long the lock took), `is_up_to_date()` and `was_dry_run()`.

### Runner options

`RunnerOptions` is a builder over `Default`, which is `allow_destructive: false`,
`allow_out_of_order: false`, `lock_wait: 60s`, `lock_lease: 15min`, `target: None`,
`profile: "dev"`, `dry_run: false`.

| Method | Effect |
| --- | --- |
| `allow_destructive()` | Run the statements inside `-- +migrate destructive` blocks without uncommenting them. Refused in production. |
| `allow_out_of_order()` | Apply a pending migration whose version sorts before one already applied. |
| `lock_wait(Duration)` | How long to wait for the migration lock. Default 60 s, then `Error::LockTimeout`. |
| `lock_lease(Duration)` | How long a SQLite lock row is good for. Default 15 min. Ignored on PostgreSQL. |
| `up_to(Version)` | Apply only versions at or below this one. Forward only. |
| `profile(impl Into<String>)` | The deployment profile string. |
| `dry_run()` | Report what would be applied and touch nothing. |

### What the profile decides

`Runner::migrate` and `Runner::apply_one` read the profile, and it decides exactly one thing:

> **In a production profile, `allow_destructive` is refused.** The same files apply, and a
> destructive block somebody uncommented and committed still runs. What production will not accept
> is the flag standing in for the diff.

`is_production()` is `"production"`, `"prod"` or `"live"`. Setting `allow_destructive` under one of
them fails with `Error::RefusedInProduction` before the directory is even classified, so nothing at
all runs, not even the migrations that destroy nothing. The reason is the same one the whole
destructive gate exists for: the mechanism should leave a diff in version control showing who
acknowledged what and when, and a flag typed at a shell leaves nothing. The escape hatch is to
uncomment the block, commit it, and deploy that.

Nothing else changes with the profile. In particular, "never auto-apply in production" is not
enforced here and does not need to be: there is no auto-apply anywhere in the framework, so a
migration runs when a human or a pipeline runs it, in every profile.

The generated `src/db.rs` builds `RunnerOptions::default().profile(profile)` from
`moso::config::Profile::detect()`, so this is live in a generated project. To pass
`allow_destructive` or a target today you edit that file; `moso db migrate` has no flags for them.

### The commands

| Command | What it does |
| --- | --- |
| `moso db status` | Applied rows with duration and `applied_by`, pending, dirty, checksum-changed, missing, out-of-order. Exits 1 when the ledger and the files disagree, so it gates CI. |
| `moso db migrate` (alias `up`) | Applies everything pending. |
| `moso db rollback --steps N` (alias `down`) | Reverts the last `N` applied. Default 1. |
| `moso db redo` | Reverts one and applies it again. The edit loop for a migration you are still writing. |

`--json` is a global flag and works after any of them; it passes your application's own document
through unchanged, so a CI job can act on it without the CLI becoming the thing that defines the
schema. Every subcommand also takes `--manifest-path`, `--bin`, `--release` and `--features`, which
are about how to build your binary, not about the database.

### The rest of the runner

`Runner::with_connection(dir, connection)` builds a runner over a `Connection` you opened yourself,
for example an in-memory SQLite one in a test. `apply_one(version, options)` applies exactly one
version. `register` adds a [Rust migration](#migrations-written-in-rust). `status()`, `files()` and
`versions()` inspect without applying, `set_timeouts(lock, statement)` sets the session timeouts by
hand, `connection()` hands you the underlying connection for a repair you have to do yourself, and
`close()` shuts it down cleanly.

`moso_migrate::runner::read_directory(dir)` parses a directory into `MigrationFile`s without opening
a database, which is how you lint migrations in CI with no server running.

## The ledger

The runner creates and owns one table:

```sql
CREATE TABLE IF NOT EXISTS moso_migrations (
  version           text PRIMARY KEY,
  name              text NOT NULL,
  checksum          text NOT NULL,
  applied_at        timestamptz NOT NULL DEFAULT now(),
  duration_ms       bigint NOT NULL DEFAULT 0,
  dirty             boolean NOT NULL DEFAULT false,
  applied_by        text,
  failed_statement  integer,
  total_statements  integer,
  failure           text
)
```

The SQLite form uses `text` for `applied_at` and `integer` for `dirty`. `applied_by` is `user@host`,
read from `USER`/`USERNAME` and `HOSTNAME`/`COMPUTERNAME`, and is `unknown@unknown` when those are
unset, which is common in containers.

`Status` classifies the directory against those rows:

| Category | Meaning |
| --- | --- |
| `applied` | A ledger row. Carries duration, `applied_by` and any recorded failure. |
| `pending` | A file with no row. Not a problem; it is the normal state before a deploy. |
| `dirty` | A non-transactional migration that failed part way. The row names the failing statement's index, the total, and the database's own message. |
| `changed` | The file's checksum no longer matches what was applied. Somebody edited a migration after it ran. |
| `missing` | A row whose file is gone. |
| `out_of_order` | A pending version that sorts before an applied one, usually a branch merged late. |

`is_clean()` is false for everything except pending. That is the boolean `moso db status` turns into
an exit code. `into_result()` turns the first problem into an `Error`, so only one is reported per
run.

Checksums are SHA-256 over the file body, normalising line endings, trailing whitespace and blank
lines. Reformatting a migration does not trip the check; changing a character does. That tolerance is
deliberate: a checksum that fires because someone re-indented a file is a checksum people learn to
bypass, and a bypassed check is worse than none. Comments are hashed, because a comment is exactly
where a destructive statement hides.

The checksum defends against accident, not against an adversary who can write to `migrations/`, since
such an adversary can write a new migration instead.

### Repairing it

Four operations exist on `Ledger` for when the ledger and reality have to be reconciled by hand. They
take a `Connection` and nothing stops you calling them, so read the row first:

| Call | Use it when |
| --- | --- |
| `Ledger::resolve(conn, version)` | A dirty migration was finished by hand and you want the flag cleared. |
| `Ledger::rewrite_checksum(conn, version, checksum)` | A migration was legitimately reformatted after it ran. |
| `Ledger::forget(conn, version)` | A row must be removed, for example after a manual rollback. |
| `Ledger::applied(conn)` | Read every row. Returns an empty vector when the table does not exist, so a fresh database is not an error. |

A squash baseline is deliberately exempt from the checksum check: any file with a non-empty
`replaces` has a legitimately different body from the migration whose version it took over.

## Destructive changes

A change is destructive when applying it loses data that the `down` cannot restore: `DropTable`,
`DropColumn`, `DropEnum`, `RewriteEnum`, and a type change the differ cannot prove is lossless.
Lossiness is decided by a heuristic with a conservative fallback, so anything it cannot prove safe is
treated as destructive. Widening within a type family, and serial to base integer in either
direction, are the proven-safe cases.

The generator emits destructive statements commented out, inside a delimited block:

```sql
-- the data in `users.legacy_id` is not recoverable; the down migration adds the column back
-- empty and nullable
-- ⚠ DESTRUCTIVE: drop `users.legacy_id`.
-- Uncomment the block below to apply it, after confirming that no running version
-- of the application still depends on what it removes.
-- +migrate destructive
-- ALTER TABLE "users" DROP COLUMN "legacy_id"
-- ;
-- +migrate end
```

Uncommenting the block is the acknowledgement. This is parsed, not a convention: the runner asks each
file for its statements and a file with a block that is still commented fails with
`Error::Destructive` before the lock is taken and before any statement runs.

Note that the statement terminator sits on its own line as `-- ;`. Uncomment both lines, or uncomment
the statement and add your own semicolon.

Why commented-out text rather than a flag alone: the flag exists (`allow_destructive`), but it is the
escape hatch, not the mechanism. The mechanism should leave a diff in version control showing who
acknowledged what and when.

### `allow_destructive` is not a universal override

Two things it will not do.

**It cannot apply a template.** Removing or reordering an enum label has no correct SQL to generate:
PostgreSQL has no `ALTER TYPE … DROP VALUE`, and every row holding the removed label needs a
replacement value only you can choose. The generator emits the plan as a block whose every line is a
comment, `PendingDestructive::is_manual()` is true for it, and `statements_to_apply` refuses it
**whether or not the flag is set**, with `Error::ManualMigrationRequired`. Honouring the flag would
mean running nothing and recording the migration as applied, which is worse than refusing.

```sql
-- ⚠ DESTRUCTIVE: rewrite the type `user_role`. A label was removed or reordered.
-- +migrate destructive
-- -- Removing or reordering an enum label needs a new type and a swap, and Moso
-- -- cannot write it: only you know which value the rows holding a removed label
-- -- should get instead.
-- --
-- -- Write the statements below, filled in, WITHOUT the leading `--`, between the
-- -- `-- +migrate destructive` and `-- +migrate end` markers. Deleting the `--` from
-- -- these comment lines is not enough: a comment runs nothing, and this migration
-- -- would be recorded as applied having changed nothing.
-- --
-- -- CREATE TYPE "user_role_new" AS ENUM ('admin', 'member');
-- -- UPDATE <table> SET <column> = <replacement label>
-- --   WHERE <column> IN ('auditor');
-- -- ALTER TABLE <table> ALTER COLUMN <column> DROP DEFAULT;
-- -- ALTER TABLE <table> ALTER COLUMN <column> TYPE "user_role_new"
-- --   USING <column>::text::"user_role_new";
-- -- DROP TYPE "user_role";
-- -- ALTER TYPE "user_role_new" RENAME TO "user_role";
-- --
-- -- Repeat the three `ALTER TABLE` lines for every table with a "user_role" column,
-- -- and restore any default you dropped afterwards.
-- +migrate end
```

Write your statements between the two markers with no leading `--`, and the block stops being
pending. That is the same acknowledgement mechanism as everywhere else. It just needs SQL you wrote
rather than SQL you uncommented.

**It is refused in production.** See [what the profile decides](#what-the-profile-decides).

> [!NOTE]
> One ordering caveat, which predates the gate and applies to every destructive block: when
> `allow_destructive` is set, the acknowledged statements run **after** the file's other statements
> rather than where they sit in the file. It matters only when a later, non-destructive operation on
> a *different* table depends on a destructive one, which the database then rejects loudly. Uncomment
> the block instead and the file runs in its written order.

## The advice it prints

When a change breaks a rolling deploy, the generator attaches `Advice` to the `Generated`. Four cases
produce it: dropping a column, adding a required column, renaming anything, and narrowing a type.
Each has a one-line `summary()` and a multi-line `plan()`.

```text
adding `users.locale` as NOT NULL with no default breaks any running version that inserts without it
expand/contract, either give it a DEFAULT, or split it in two:
  1. now:   add `users.locale` nullable, and start writing it.
     Deploy that. Every pod is then filling it in.
  2. later: backfill the old rows and add NOT NULL, in a migration of its own.
A DEFAULT does the same job in one step and is usually the right answer; this generator has
already written the three-step form for you.
```

The advice is data on `Generated`, not something printed for you. Print it in your entry point, as in
the example near the top of this page, or nobody sees it. `Advice::for_diff(&diff)` computes the same
list from a `Diff` you already have.

## Rolling back

```bash
moso db rollback --steps 2
moso db redo
```

`rollback(n)` takes the lock, then for each of the last `n` applied versions (by ledger order, not
file order) runs the file's `down` statements and deletes the ledger row. A file with no `down`
refuses, with a message telling you to write a new forward migration instead. A version whose file is
gone errors with `MissingFile` rather than being skipped.

`redo` is rollback one plus migrate, for the loop where you are still writing the migration.

Rolling back is for your machine and your staging database. In production, a forward migration that
undoes the change is almost always the right move, because a `down` that is wrong is discovered
during an incident, which is the moment you least want to find out.

## Migrations written in Rust

When a migration needs application logic, implement `RustMigration` and register it on the runner. It
then runs in the same ordering, under the same lock, and is recorded in the same ledger as the SQL
files.

```rust title="src/migrations/backfill_slugs.rs"
use futures_util::future::BoxFuture;
use moso_migrate::rust_migration::{Migrator, RustMigration};
use moso_migrate::{Result, Version};

pub struct BackfillSlugs;

impl RustMigration for BackfillSlugs {
    fn version(&self) -> Version {
        Version::from_parts(2026, 7, 30, 9, 0, 0)
    }

    fn name(&self) -> &str {
        "backfill_slugs"
    }

    /// Runs outside a transaction so it can batch without holding locks.
    fn is_transactional(&self) -> bool {
        false
    }

    fn up<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            migrator
                .batched("SELECT id, title FROM posts WHERE slug IS NULL", 1000, |rows| async move {
                    Ok(rows
                        .iter()
                        .filter_map(|row| Some((row.first()?.as_ref()?, row.get(1)?.as_ref()?)))
                        .map(|(id, title)| {
                            format!(
                                "UPDATE posts SET slug = {} WHERE id = {id}",
                                moso_migrate::emit::quote_literal(&title.to_lowercase()),
                            )
                        })
                        .collect())
                })
                .await?;
            Ok(())
        })
    }
}
```

Register it where the runner is built:

```rust title="src/db.rs"
runner.register(BackfillSlugs);
```

That is the only way one reaches the runner. `read_directory` reads `*.sql` and nothing else, so a
`.rs` file in `migrations/` is invisible. `#[migration]` does not help here either: the attribute adds
four constants (`VERSION`, `NAME`, `DESCRIPTION`, `SOURCE`) to the type it annotates and implements
nothing. It is a place to keep the metadata, not a registration.

`Migrator` deliberately gives you raw SQL, rows as text, and batching, and deliberately not the ORM.
An entity's Rust type is the *current* one; a migration written six months ago has to keep working
against the schema of six months ago. The `batched` closure receives rows and returns statements
rather than receiving the connection, which is a borrow-checker consequence that also makes the
closure testable with no database. `execute`, `fetch`, `backend()` and `statements_run()` round out
the type.

Defaults on the trait: `is_reversible()` is false, `is_transactional()` is true, `down` is a no-op,
and `fingerprint()` hashes `"version:name"`.

## Squashing

Once a project has a hundred migrations, a fresh database spends minutes replaying history.

```sh
moso db squash        # a report: every file it would collapse, every file it would delete
moso db squash --yes  # …and then do it
```

`--yes` is not a formality: without it nothing is written and nothing is deleted. The command is also
refused outright while anything on disk is unapplied or the ledger is unsound, because a baseline
that claims to replace a migration a database never ran would run in full over a schema that already
has half of it. `command::squash` is the call it makes, and it collapses **every** file into one
baseline carrying `-- moso:replaces`.

```rust
use moso_migrate::command::{self, SquashOptions};

// A look first: nothing is written and nothing is deleted.
let planned = command::squash(MIGRATIONS, Backend::Postgres, &descriptors, &SquashOptions::default())?;
eprintln!("{} files collapse into {}", planned.replaced().len(), planned.path());

// Then, deliberately:
let done = command::squash(
    MIGRATIONS,
    Backend::Postgres,
    &descriptors,
    &SquashOptions::default().apply(),
)?;
assert!(done.was_written());
```

`SquashReport` answers `version()`, `name()`, `path()`, `replaced()`, `removable()`, `migration()`
and `was_written()`. `apply()` is off by default: a squash rewrites version-controlled history, and
deleting files during what might have been a look would be unforgivable.

It squashes everything, deliberately. A **partial** squash needs the schema as of the cut-off rather
than today's (a baseline built from today's entities followed by migrations that add today's columns
fails on a fresh database), and nothing here can know that schema. If you do know it, the layer
underneath takes it:

```rust
use moso_migrate::squash::Squash;
use moso_migrate::{MigrationId, Version};

let id = MigrationId::new(Version::from_parts(2026, 7, 29, 10, 15, 0), "baseline");
let squash = Squash::build(
    &schema_as_of_that_point,
    &[Version::from_parts(2026, 1, 1, 0, 0, 0)],
    &id,
    Backend::Postgres,
)?;
assert!(squash.migration().contains("-- moso:replaces 20260101T000000"));
```

`Squash::over_directory(directory, before, schema, backend, at)` does the same for every file older
than `before`.

The baseline takes the *oldest* replaced version, so a fresh database applies it first, and so a
second `git pull` does not produce two baselines. When the runner reaches a baseline whose replaced
versions are all already applied, it records it as applied without running it and deletes the
replaced rows. That is the only mechanism that works for a team where some databases are old and
some are new.

`removable()` returns the paths that are now redundant; `apply(directory)` writes the baseline and
deletes them.

## Seeds

A seed is not a migration: not versioned, not recorded, meant to be run again.

```rust
use futures_util::future::BoxFuture;
use moso_migrate::rust_migration::Migrator;
use moso_migrate::seed::{Seed, SeedOptions, Seeds};
use moso_migrate::Result;

struct Dev;

impl Seed for Dev {
    fn name(&self) -> &str {
        "dev"
    }

    fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            migrator
                .execute("INSERT INTO users (email) VALUES ('admin@local') ON CONFLICT DO NOTHING")
                .await?;
            Ok(())
        })
    }
}

let mut seeds = Seeds::default();
seeds.add(Dev);
let ran = seeds
    .run(&mut connection, Some("dev"), &SeedOptions::default().profile("dev"))
    .await?;
```

Pass `None` as the name to run every registered seed; `names()` lists them. `SeedOptions` defaults to
profile `dev`, not forced, transactional; `force()` and `without_a_transaction()` change that.

A seed refuses to run when the profile is production unless `is_safe_in_production()` returns true on
the seed or the caller passes `force()`. It fails with `Error::RefusedInProduction`. A seed that
creates an `admin@local` account is a security incident in production.

Idempotence is your problem, and the module says so: "idempotent by convention" is a convention it
cannot enforce and does not pretend to. Write `ON CONFLICT DO NOTHING`.

```sh
moso db seed          # every registered seed
moso db seed dev      # one, by name
moso db seed --force  # …under a production profile, deliberately
```

`command::seed(url, &seeds, name, &options)` is the call behind it: it opens the database, runs the
seeds and closes, and answers a `SeedReport` with `ran()`, `available()`, `profile()` and
`was_forced()`. `available()` is there so a wrong name is answerable without a second round trip.
The registry it runs is `seeds()` in your `src/db.rs`; `moso new --with-db` puts one `dev` seed
there to show the shape.

```rust
let report = moso_migrate::command::seed(url, &seeds, Some("dev"), &SeedOptions::default()).await?;
```

## Checking for drift

Introspection reads a live database back into a `Schema`, and `compare` diffs it against what you
expect, in both directions.

```rust
use moso_migrate::check::compare;
use moso_migrate::schema::{Schema, Table};

let mut live = Schema::empty();
live.add_table(Table::new("legacy"));

let drift = compare(&live, &Schema::empty())?;
assert_eq!(drift.extra_in_database().len(), 1);
assert!(drift.missing_in_database().is_empty());
```

Against a real database, `moso db check` does the whole job (open, list what is pending, introspect,
compare) and exits 1 on drift in either direction, which is what makes it usable as a CI gate:

```sh
moso db check          # names what diverged, and how
moso db check --json   # the CheckReport itself, for a pipeline
```

`command::check` is the call behind it:

```rust
let report = moso_migrate::command::check(MIGRATIONS, url, &descriptors).await?;
if !report.is_clean() {
    eprintln!("{}", report.report());
}
```

`CheckReport` answers `is_clean()`, `missing_in_database()`, `extra_in_database()`, `mismatched()`,
`pending()` and `report()`. `is_clean()` is the exit code, and pending migrations do not make it
false. One level down, `check::check(connection, expected, pending)` takes a schema from anywhere
(the committed snapshot, a hand-built one) rather than from the entity graph:

```rust
let drift = moso_migrate::check::check(connection, expected, Vec::new()).await?;
drift.into_result()?;
```

`Drift` reports `missing_in_database()`, `extra_in_database()`, `mismatched()` and `pending()`, and
its `Display` renders a printable report. Pending migrations are listed but do
not by themselves count as drift, because they are the fix for it. One direction alone would be half
a check: missing-in-database catches an unapplied migration, extra-in-database catches somebody's
`psql` session.

Two things shape what this can tell you. Introspection targets fidelity for the schemas Moso
generates, not completeness for any schema: a hand-written `EXCLUDE` constraint is read back
approximately and reported as drift. The rejected alternative, silently ignoring what it cannot read,
would make a drift check say "no drift" for a database missing all of your indexes. And check
expressions are compared through a normaliser, because PostgreSQL stores a parse tree and re-prints
it, so `length(title) > 0` on a `varchar` column comes back as `length((title)::text) > 0`. The
normaliser does not try to understand the expression, so two genuinely different predicates always
compare different.

The proven CI shape is: apply everything to a fresh database, `read_schema`, `compare` against the
snapshot, fail the job on any drift. That is what the crate's own mandatory test suite does. Note
that suite resets the `public` schema between tests, so never point `DATABASE_URL` at a database you
care about while running it.

## Migrating every tenant

Under `TenancyModel::SchemaPerTenant` or `DatabasePerTenant` the migrations have to run once per
tenant, and a tenant whose schema is behind is a tenant whose requests fail.

```sh
moso db migrate --all-tenants
```

reports every tenant as it goes and exits non-zero naming the ones that failed. `tenants()` in your
`src/db.rs` is the list it migrates, and `command::migrate_tenants` is the loop underneath it.

```rust
use moso_migrate::command::{migrate_tenants, TenantTarget};

let tenants = [
    TenantTarget::schema("acme", url, "tenant_7"),
    TenantTarget::schema("globex", url, "tenant_8"),
];
let report = migrate_tenants(MIGRATIONS, &tenants, &options, &|runner| {
    runner.register(BackfillSlugs);
})
.await?;
```

| Piece | What it does |
| --- | --- |
| `TenantTarget::database(tenant, url)` | one whole database per tenant |
| `TenantTarget::schema(tenant, url, schema)` | one named schema per tenant, inside one database |
| `register` | is handed each tenant's `Runner` before it migrates. This is where `runner.register(..)` goes |
| `TenantMigrateReport` | `is_clean()`, `tenants()`, `failures()` |

The tenant list is yours: Moso does not know where you keep it. Under schema-per-tenant the runner
creates the schema if it is missing and sets `search_path` to it, so the ledger, the tables and the
lock all land in the tenant's own schema. Each tenant gets its own `moso_migrations`, which is what
lets tenants be at different versions. The schema name reaches SQL only as a validated, quoted
identifier, and a qualified name is refused outright, so a tenant key chosen by somebody else cannot
become SQL. Schema-per-tenant on SQLite is `Error::Unsupported` naming `TenantTarget::database`.

`register` is not optional ceremony. A run that silently skipped your Rust migrations would leave a
schema that is *almost* migrated, so pass `&|_| {}` when there are none and mean it.

A tenant that fails is recorded in its `TenantOutcome` and the run continues to the next one, because
tenants are independent and a deploy needs to know about all of them rather than the first.
`is_clean()` is the exit code. The one thing that stops the whole run before it starts is
`allow_destructive` against a production profile, which is refused before any tenant is opened.

An empty tenant list is refused rather than reported as a clean run of nothing: `--all-tenants` in a
deploy script that migrated no tenant and exited 0 is the failure worth preventing. See
[multi-tenancy](./multi-tenancy.md#operating-a-routed-deployment).

## The safety rules, and why

Seven rules are enforced by code rather than by convention. They are worth knowing because each one
exists because of a specific way people lose data.

1. **Nothing applies automatically.** There is no boot hook and no `database.auto_migrate` key. A
   migration runs when a human or a pipeline runs it, in every profile.
2. **Destructive statements ship commented out.** Uncommenting is the acknowledgement, and it leaves
   a diff in version control naming who acknowledged what. The `--allow-destructive` equivalent is
   the escape hatch, not the mechanism, and in production it is not even that, because the profile
   refuses it.
3. **An applied migration cannot change.** The checksum is recorded and compared, tolerant of
   reformatting and not of content. A mismatch stops the run before anything is applied.
4. **One migrator at a time.** A lock is held for the whole run, and the ledger is re-read after it
   is taken, so a pod that waited does not re-apply what the pod ahead of it just did. A killed
   migrator releases it: on PostgreSQL when the session closes, on SQLite when the row's lease
   expires. See [the migration lock](#the-migration-lock).
5. **Every migration has a `lock_timeout`,** defaulting to 5 seconds. A migration queued behind a
   long-running transaction queues every query behind itself, and moments later the site is down.
   This is the one default that prevents a shocking number of outages. Raise it per file with
   `-- moso:lock-timeout 30s` when you know the table is quiet. Do not remove it.
6. **A partial failure is recorded, not forgotten.** A non-transactional migration that fails leaves
   a dirty row with the failing statement's index, the total, and the database's own message, and the
   next run refuses until it is resolved.
7. **Out-of-order application is detected.** Timestamps mean two branches cannot collide, and the
   cost is that a late merge can produce a pending version older than an applied one. That is
   reported and refused rather than quietly applied.

Rule 1 is a consequence of there being no auto-apply at all, not of the profile check. What the
profile *does* decide is the second half of rule 2. See
[what the profile decides](#what-the-profile-decides).

## Failure modes

| Error | What happened | What to do |
| --- | --- | --- |
| `ChecksumMismatch` | A migration file changed after it was applied. | Revert the edit, or `rewrite_checksum` if the change was cosmetic and you are certain. |
| `Dirty` | A non-transactional migration failed part way. The error names the statement index, the total and the driver's message. | Finish or undo the remaining statements by hand, then `Ledger::resolve`. |
| `MissingFile` | A ledger row has no file. | Restore the file, or `Ledger::forget` if the migration is genuinely gone. |
| `OutOfOrder` | A pending version sorts before an applied one. | Review whether the late branch is safe to apply now, then `allow_out_of_order`. |
| `Destructive` | A file has an unacknowledged destructive block. | Uncomment the block and commit that, or set `allow_destructive`. |
| `ManualMigrationRequired` | A destructive block is a template, so no flag can apply it. | Write the statements inside the block, without the leading `--`. |
| `RefusedInProduction` | `allow_destructive` was set against a production profile, or a seed ran there. | Uncomment the block and commit it; for a seed, mark it safe or force it deliberately. |
| `NeedsAnswer` | Generation hit a rename question with no oracle answer. The error carries the exact `--rename` flag. | Add the pair, or answer interactively. |
| `LockTimeout` | Another migrator held the lock past `lock_wait`. | Wait; on SQLite a dead holder's row is reaped once its lease expires. |
| `LockLost` | A SQLite run outlived its lock lease and the row was reaped. | Check `moso db status` (another migrator may have applied some of these), then raise `lock_lease`. |
| `Unsupported` | The backend cannot do the operation, for example a `gin` index or partitioning on SQLite. | The message names the alternative. This is a hard error, never a silent downgrade. |
| `DuplicateVersion` | Two files claim the same version. | Rename one. The runner refuses to start. |
| `MalformedMigration` | A bad directive, or a destructive block with no `-- +migrate end`. | The error carries the file, the reason and a help line. |
| `Snapshot` | `.schema.json` is unparseable, has an unknown field, or has a `format` newer than this build. | Regenerate it, or upgrade `moso-migrate`. |
| `Drift` | `check` found a difference. | Read the report; it lists each side. |
| `MigrationFailed` | The database rejected a statement. | The message is the driver's. |

`Error` is `#[non_exhaustive]` and carries `is_operational()`, which separates "the database said no"
from "your files are wrong".

## The migration lock

One migrator at a time, and the two backends get there differently because only one of them has a
server that can notice a process died.

On **PostgreSQL** it is `pg_advisory_lock(4355294045437474)`, held by the session. A migrator that is
killed leaves nothing behind: the server drops the lock when the connection goes, and the next run
takes it immediately.

On **SQLite** there is no server, so the lock is a row in `moso_migrations_lock`, and a row outlives
its writer. It is therefore a **leased** row: an `owner` token and an `expires_at`, and a runner that
finds an expired row reaps it before trying to take the lock. The lease is renewed between
migrations, so a healthy long run keeps its lock and a dead one gives it up.

```sql
CREATE TABLE IF NOT EXISTS moso_migrations_lock (
  id         integer PRIMARY KEY CHECK (id = 1),
  holder     text NOT NULL,      -- user@host, for the human reading it
  owner      text,               -- the token that scopes DELETE and UPDATE to one run
  taken_at   text NOT NULL DEFAULT (datetime('now')),
  expires_at text
)
```

A lock table written by an older Moso has no `owner` or `expires_at`; both are added with
`ALTER TABLE … ADD COLUMN` rather than by dropping the table, and a row with no `expires_at` is
treated as expiring `taken_at` plus the lease, so an old row is reaped on the same schedule instead
of being immortal.

Two differences from PostgreSQL remain, and both follow from there being no server:

1. **Recovery is not instant.** Between the kill and the lease running out, a later run waits exactly
   as it would behind a live migrator, because nothing can tell the two apart. The default lease is
   15 minutes; `lock_lease(Duration)` changes it.
2. **A single statement can outlive the lease.** Renewal happens between migrations, not inside one.
   A migration slower than its whole lease has its row reaped, finds out at the next renewal, and
   fails with `Error::LockLost` rather than carrying on beside another migrator. Raise the lease if
   you genuinely have a migration that slow.

`Ledger`'s neighbours are public for the cases you have to do by hand: `MigrationLock::acquire`,
`acquire_with_lease`, `refresh`, `reap_expired` and `release`. `reap_expired` is a no-op on
PostgreSQL rather than an error, so a caller does not have to branch.

## Scope and edges to know

An honest map of the edges, so you know where each one is.

- **`moso db reset`, `shell` and `explain` are out of scope by decision.** The shipped `moso db`
  subcommands are listed in the `IMPORTANT` note at the top of this page; the CLI's own table is the
  authoritative record of what exists.
- **The entity list, the tenant list and the seed registry are hand-written.** `moso new --with-db`
  writes an empty entity list, an empty `tenants()` and one `dev` seed into `src/db.rs`, and
  `make-migration`, `check` and `squash` refuse until you have listed your entities. That is the
  ADR-0004 consequence, not an oversight: nothing may discover them.
- **No CLI flags for `--allow-destructive`, `--allow-out-of-order`, `--to`, `--lock-timeout` or
  `--lock-lease`.** All five exist on `RunnerOptions`; `moso db migrate` does not expose them. Edit
  your `src/db.rs`. (`--dry-run` *is* wired, on `make-migration`.)
- **`moso db squash` squashes everything or nothing.** There is no `--before <version>`: a partial
  squash needs the schema as of the cut-off rather than today's, and nothing on this path knows it.
  [`Squash::over_directory`](#squashing) is the entry point for a caller who does.
- **The rename prompt is not reachable from the CLI.** `moso db` runs your binary with standard
  input closed, so `RenamePolicy::Ask` would fail rather than ask. Answer with `--rename old:new`,
  or `--drop-and-add` when the data does not matter.
- **`up_to` is forward only.** The target filters the pending list; there is no downward path.
- **The SQLite lock lease does not renew inside one migration.** A migration slower than the whole
  lease loses its lock. See [the migration lock](#the-migration-lock).
- **Destructive statements applied with `allow_destructive` run at the end of the file** rather than
  where they sit in it. Uncommenting the block preserves the order.
- **`moso db prune-test` does not exist,** and it is the one `moso db` subcommand that cannot be
  built the way the others were. The pruner is `moso_test::db::prune_test_databases`, and `src/db.rs`
  is compiled into your production binary, so answering a `--db-prune-test` flag would put the test
  harness, and its dependency-override surface, in every deployment. Wiring it needs `moso-test` as
  an *optional* dependency of the generated project behind a cargo feature the CLI turns on for that
  one command, which changes what `moso new` generates and wants an RFC first. See
  [testing](./testing.md#environment-and-cleanup).
- **`partition_by` and extensions are not reachable from `#[derive(Entity)]`.**
  `Table::partitioned_by` and `Schema::add_extension` work and are tested against real PostgreSQL,
  but nothing on the entity path calls them, and there is no `#[entity(partition_by = ..)]`
  attribute. Identity columns, collations and virtual generated columns are in the same position.
  Named schemas *are* reachable, with `#[entity(schema = "...")]`.
- **`#[migration]` registers nothing.** It adds constants. `runner.register(...)` is the only
  registration, and Rust migrations are never discovered from files.
- **Migration is an explicit step, not a boot hook.** There is deliberately no `database.auto_migrate`
  configuration key and no boot hook that runs migrations for you: you run `moso db migrate` (or call
  the runner) when you decide to, so a deploy never silently reshapes the database.

## See also

- [Transactions and pooling](./transactions.md) for the connection pool the rest of your application
  uses, which the runner deliberately does not share.
- [Testing](./testing.md) for building test databases, which has its own `Migrator` trait unrelated to
  the one on this page.
- [Configuration](./configuration.md) for `DATABASE_URL` and the profile string.
- [Raw SQL](./raw-sql.md) for why every operation is a typed DDL value before it is text.
