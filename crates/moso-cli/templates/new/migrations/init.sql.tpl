-- The first migration.
--
-- A migration file is one `up` section and one `down` section, separated by the
-- markers below. Both are plain SQL for your database; nothing here is
-- rewritten, so what you read is what runs.
--
-- The file name is `<version>_<name>.sql`, where the version is a UTC timestamp
-- spelled `YYYYMMDDTHHMMSS`. Migrations run in version order, and the ledger
-- (`moso_migrations`) records the checksum of each one — so editing a file that
-- has already run is reported by `moso db status` rather than silently ignored.
--
-- This one is hand-written so that `moso db migrate` has something real to do on
-- a fresh project. Once you have `#[derive(Entity)]` models, you will not write
-- another by hand: list them in `src/db.rs` and run
--
--     moso db make-migration add_locale_to_users
--
-- which diffs your entities against `migrations/.schema.json` and writes the
-- file plus an updated snapshot, both of which you commit together. There is no
-- snapshot yet, and this table is in none — so delete this file and its table
-- before you generate your first migration, or the generator will offer to
-- create `greetings` a second time.
--
-- `src/db.rs` also registers a `dev` seed that inserts one row into the table
-- below; `moso db seed` runs it.

-- +migrate up
CREATE TABLE greetings (
    id          BIGINT PRIMARY KEY,
    name        TEXT NOT NULL,
    message     TEXT NOT NULL,
    created_at  TIMESTAMP NOT NULL
);

CREATE INDEX greetings_name_idx ON greetings (name);

-- +migrate down
-- Every `down` must undo exactly what its `up` did, in reverse order. A `down`
-- that is wrong is worse than one that is absent: it is discovered during an
-- incident, which is the moment you least want to find out.
DROP INDEX greetings_name_idx;

DROP TABLE greetings;
