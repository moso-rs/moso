//! The three mandatory tests from `docs/02-data/23-migrations.md`, plus the
//! operation-coverage table, on both dialects.
//!
//! 1. **Round trip.** Fresh database → apply every migration → read the schema
//!    back → compare with the schema the migrations were generated from.
//! 2. **Reversibility.** Apply → roll back → apply, comparing the schema each
//!    time.
//! 3. **Generator idempotence.** `make-migration` twice produces exactly one
//!    migration.
//!
//! PostgreSQL tests gate on `DATABASE_URL` and skip with a message when it is
//! not set, so the suite still passes on a machine without Docker. SQLite tests
//! always run: the driver is bundled.

#![allow(
    clippy::await_holding_lock,
    reason = "`#[tokio::test]` runs on a current-thread runtime, so the guard               never crosses a thread; it is held across awaits deliberately, to               serialise the tests that share one PostgreSQL database"
)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use moso_migrate::check::compare;
use moso_migrate::generator::Generator;
use moso_migrate::introspect::{read_schema, read_schema_including};
use moso_migrate::rename::{DropAndAdd, Scripted};
use moso_migrate::runner::{Runner, RunnerOptions};
use moso_migrate::schema::{
    Check, Column, EnumType, ForeignKey, Index, IndexPart, Schema, Sort, Table,
};
use moso_migrate::{Diff, Version};
use moso_orm::Backend;
use moso_sql::DataType;

// ── fixtures ────────────────────────────────────────────────────────────────

/// A schema that exercises every row of the operation-coverage table that a
/// `CREATE TABLE` can carry.
fn full_schema(backend: Backend) -> Schema {
    let mut schema = Schema::empty();

    let mut users = Table::new("users").for_entity("User");
    users.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
    users.add_column(Column::new("email", DataType::Text).for_field("email"));
    users.add_column(
        Column::new("bio", DataType::Text)
            .nullable()
            .for_field("bio"),
    );
    users.add_column(
        Column::new("locale", DataType::VarChar(Some(8)))
            .with_default("'en'")
            .for_field("locale"),
    );
    users.add_column(
        Column::new("is_admin", DataType::Boolean)
            .with_default(if backend == Backend::Sqlite {
                "0"
            } else {
                "false"
            })
            .for_field("is_admin"),
    );
    users.set_primary_key(["id"]);
    users.add_index(
        Index::new("users_email_key", ["email"])
            .unique()
            .backing_a_constraint(),
    );
    users.add_index(Index::over(
        "idx_users_locale",
        [IndexPart::column("locale").sorted(Sort::Desc)],
    ));
    users.add_check(Check::new("users_id_positive", "id > 0"));
    schema.add_table(users);

    let mut posts = Table::new("posts").for_entity("Post");
    posts.add_column(Column::new("id", DataType::BigSerial));
    posts.add_column(Column::new("author_id", DataType::BigInt));
    posts.add_column(Column::new("title", DataType::Text));
    posts.set_primary_key(["id"]);
    posts.add_foreign_key(
        ForeignKey::new("posts_author_id_fkey", ["author_id"], "users", ["id"])
            .on_delete(moso_migrate::schema::Action::Cascade),
    );
    posts.add_index(Index::new("idx_posts_author", ["author_id"]));
    schema.add_table(posts);

    let mut tags = Table::new("tags").for_entity("Tag");
    tags.add_column(Column::new("id", DataType::BigSerial));
    tags.add_column(Column::new("name", DataType::Text));
    tags.set_primary_key(["id"]);
    schema.add_table(tags);

    // The many-to-many join table the operation table calls for.
    let mut post_tags = Table::new("post_tags");
    post_tags.add_column(Column::new("post_id", DataType::BigInt));
    post_tags.add_column(Column::new("tag_id", DataType::BigInt));
    post_tags.set_primary_key(["post_id", "tag_id"]);
    post_tags.add_foreign_key(
        ForeignKey::new("post_tags_post_id_fkey", ["post_id"], "posts", ["id"])
            .on_delete(moso_migrate::schema::Action::Cascade),
    );
    post_tags.add_foreign_key(
        ForeignKey::new("post_tags_tag_id_fkey", ["tag_id"], "tags", ["id"])
            .on_delete(moso_migrate::schema::Action::Cascade),
    );
    post_tags.add_index(Index::new("post_tags_tag_id_idx", ["tag_id"]));
    schema.add_table(post_tags);

    // A PostgreSQL enum type. SQLite has none, so the SQLite fixture stores the
    // variant as text and needs no type at all.
    if backend == Backend::Postgres {
        schema.add_enum(EnumType::new("user_role", ["admin", "member"]));
    }
    schema
}

/// The schema after a second round of changes: every ALTER the operation table
/// names.
fn evolved_schema(backend: Backend) -> Schema {
    let mut schema = full_schema(backend);

    {
        let users = schema.table_mut("users").expect("users");
        // New field, nullable so the migration is one statement.
        users.add_column(Column::new("timezone", DataType::Text).nullable());
        // Nullability change.
        users.add_column(Column::new("bio", DataType::Text));
        // Default change.
        users.add_column(Column::new("locale", DataType::VarChar(Some(8))).with_default("'fr'"));
        // New index.
        users.add_index(Index::new("idx_users_email_lower", ["email"]));
    }

    {
        let posts = schema.table_mut("posts").expect("posts");
        // Type change, widening.
        posts.add_column(Column::new("title", DataType::VarChar(Some(500))));
        // New check.
        posts.add_check(Check::new("posts_title_not_empty", "length(title) > 0"));
    }

    // New entity.
    let mut comments = Table::new("comments").for_entity("Comment");
    comments.add_column(Column::new("id", DataType::BigSerial));
    comments.add_column(Column::new("post_id", DataType::BigInt));
    comments.add_column(Column::new("body", DataType::Text));
    comments.set_primary_key(["id"]);
    comments.add_foreign_key(ForeignKey::new(
        "comments_post_id_fkey",
        ["post_id"],
        "posts",
        ["id"],
    ));
    schema.add_table(comments);

    // Removed entity.
    schema.remove_table("tags");
    schema.remove_table("post_tags");

    // No enum change here on purpose: `ALTER TYPE … ADD VALUE` is
    // irreversible (PostgreSQL has no `DROP VALUE`), so putting one in this
    // fixture would make the *reversibility* test unable to roll back and
    // would prove nothing. Adding a variant has its own test below.
    schema
}

// ── harness ─────────────────────────────────────────────────────────────────

struct Fixture {
    directory: PathBuf,
    backend: Backend,
    url: String,
    clock: Version,
}

/// Tests run in parallel threads of one process, and the system clock is not
/// fine-grained enough on every platform to keep two of them apart. A counter
/// is — and two fixtures sharing a directory is exactly the flake that looks
/// like a checksum bug.
static UNIQUE: AtomicU64 = AtomicU64::new(0);

fn unique() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        UNIQUE.fetch_add(1, Ordering::Relaxed)
    )
}

impl Fixture {
    fn new(label: &str, backend: Backend, url: String) -> Self {
        let directory =
            std::env::temp_dir().join(format!("moso-migrate-test-{label}-{}", unique()));
        std::fs::create_dir_all(&directory).expect("creates the migrations directory");
        Self {
            directory,
            backend,
            url,
            clock: Version::from_parts(2026, 1, 1, 0, 0, 0),
        }
    }

    fn generator(&mut self) -> Generator {
        let clock = self.clock;
        self.clock = Version::from_parts(
            clock.year(),
            clock.month(),
            clock.day() + 1,
            clock.hour(),
            clock.minute(),
            clock.second(),
        );
        Generator::new(&self.directory, self.backend).at(clock)
    }

    /// Generates a migration for `before -> after` and writes it.
    fn generate(&mut self, before: &Schema, after: &Schema) -> bool {
        match self
            .generator()
            .make_migration_between(before, after, None, &DropAndAdd)
            .expect("the generator refused")
        {
            Some(generated) => {
                generated.write().expect("writes");
                true
            }
            None => false,
        }
    }

    async fn runner(&self) -> Runner {
        Runner::open(&self.directory, &self.url)
            .await
            .expect("opens the database")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

/// Every PostgreSQL test works in one database and resets it, so they take
/// turns. A poisoned lock is recovered rather than propagated: one failing test
/// should not turn the rest into a cascade of unrelated panics.
static POSTGRES: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn postgres_turn() -> std::sync::MutexGuard<'static, ()> {
    POSTGRES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The PostgreSQL URL, or `None` with a printed skip message.
fn postgres_url(label: &str) -> Option<String> {
    match std::env::var("DATABASE_URL") {
        Ok(url) if url.starts_with("postgres") => Some(url),
        _ => {
            eprintln!(
                "skipping `{label}`: set DATABASE_URL to a PostgreSQL database to run it, e.g.\n  \
                 DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test cargo test -p moso-migrate"
            );
            None
        }
    }
}

/// Drops everything in the connected PostgreSQL schema, so each test starts
/// from nothing without needing a separate database.
async fn reset_postgres(runner: &mut Runner) {
    let connection = runner.connection();
    connection
        .execute("DROP SCHEMA public CASCADE")
        .await
        .expect("drops");
    connection
        .execute("CREATE SCHEMA public")
        .await
        .expect("recreates");
}

fn sqlite_url() -> String {
    let path = std::env::temp_dir().join(format!("moso-migrate-{}.db", unique()));
    format!("sqlite://{}", path.display())
}

// ── 1. round trip ───────────────────────────────────────────────────────────

async fn round_trip(mut fixture: Fixture) {
    let schema = full_schema(fixture.backend);
    assert!(fixture.generate(&Schema::empty(), &schema), "a migration");

    let mut runner = fixture.runner().await;
    if fixture.backend == Backend::Postgres {
        reset_postgres(&mut runner).await;
    }
    let report = runner
        .migrate(&RunnerOptions::default())
        .await
        .expect("applies");
    assert_eq!(report.applied().len(), 1);

    let live = read_schema(runner.connection()).await.expect("reads back");
    let drift = compare(&live, &schema).expect("compares");
    assert!(
        drift.is_empty(),
        "the migration did not reproduce the schema:\n{drift}"
    );
    runner.close().await;
}

#[tokio::test]
async fn round_trip_on_sqlite() {
    round_trip(Fixture::new("round-trip", Backend::Sqlite, sqlite_url())).await;
}

#[tokio::test]
async fn round_trip_on_postgres() {
    let Some(url) = postgres_url("round_trip_on_postgres") else {
        return;
    };
    let _turn = postgres_turn();
    round_trip(Fixture::new("round-trip-pg", Backend::Postgres, url)).await;
}

// ── the evolution: every ALTER in the operation table ────────────────────────

async fn evolution(mut fixture: Fixture) {
    let first = full_schema(fixture.backend);
    let second = evolved_schema(fixture.backend);

    assert!(fixture.generate(&Schema::empty(), &first));
    assert!(fixture.generate(&first, &second), "a second migration");

    let mut runner = fixture.runner().await;
    if fixture.backend == Backend::Postgres {
        reset_postgres(&mut runner).await;
    }
    let report = runner
        // The second migration drops two tables, which is destructive.
        .migrate(&RunnerOptions::default().allow_destructive())
        .await
        .expect("applies");
    assert_eq!(report.applied().len(), 2);

    let live = read_schema(runner.connection()).await.expect("reads back");
    let drift = compare(&live, &second).expect("compares");
    assert!(
        drift.is_empty(),
        "the second migration did not reproduce the schema:\n{drift}"
    );
    runner.close().await;
}

#[tokio::test]
async fn every_alter_round_trips_on_sqlite() {
    evolution(Fixture::new("evolution", Backend::Sqlite, sqlite_url())).await;
}

#[tokio::test]
async fn every_alter_round_trips_on_postgres() {
    let Some(url) = postgres_url("every_alter_round_trips_on_postgres") else {
        return;
    };
    let _turn = postgres_turn();
    evolution(Fixture::new("evolution-pg", Backend::Postgres, url)).await;
}

// ── 2. reversibility ────────────────────────────────────────────────────────

async fn reversibility(mut fixture: Fixture) {
    let first = full_schema(fixture.backend);
    let second = evolved_schema(fixture.backend);
    assert!(fixture.generate(&Schema::empty(), &first));
    assert!(fixture.generate(&first, &second));

    let mut runner = fixture.runner().await;
    if fixture.backend == Backend::Postgres {
        reset_postgres(&mut runner).await;
    }
    let options = RunnerOptions::default().allow_destructive();

    runner.migrate(&options).await.expect("applies");
    let after_first = read_schema(runner.connection()).await.expect("reads");

    runner.rollback(1).await.expect("rolls back");
    let after_rollback = read_schema(runner.connection()).await.expect("reads");
    let back_to_first = compare(&after_rollback, &first).expect("compares");
    assert!(
        back_to_first.is_empty(),
        "rolling back did not restore the previous schema:\n{back_to_first}"
    );

    runner.migrate(&options).await.expect("re-applies");
    let after_redo = read_schema(runner.connection()).await.expect("reads");
    let same = compare(&after_redo, &after_first).expect("compares");
    assert!(
        same.is_empty(),
        "apply → rollback → apply produced a different schema:\n{same}"
    );
    runner.close().await;
}

#[tokio::test]
async fn reversibility_on_sqlite() {
    reversibility(Fixture::new("reverse", Backend::Sqlite, sqlite_url())).await;
}

#[tokio::test]
async fn reversibility_on_postgres() {
    let Some(url) = postgres_url("reversibility_on_postgres") else {
        return;
    };
    let _turn = postgres_turn();
    reversibility(Fixture::new("reverse-pg", Backend::Postgres, url)).await;
}

// ── 3. generator idempotence ────────────────────────────────────────────────

#[test]
fn make_migration_twice_produces_exactly_one_migration() {
    for backend in [Backend::Postgres, Backend::Sqlite] {
        let mut fixture = Fixture::new("idempotence", backend, sqlite_url());
        let schema = full_schema(backend);

        assert!(
            fixture.generate(&Schema::empty(), &schema),
            "run one writes"
        );
        let snapshot = fixture
            .generator()
            .read_snapshot()
            .expect("reads the snapshot it just wrote");
        assert_eq!(snapshot, schema, "the snapshot is the schema, exactly");

        assert!(
            !fixture.generate(&snapshot, &schema),
            "run two must find nothing on {backend}"
        );

        let files = moso_migrate::runner::read_directory(&fixture.directory).expect("reads");
        assert_eq!(files.len(), 1, "exactly one migration on {backend}");
    }
}

#[test]
fn idempotence_holds_after_an_evolution_too() {
    for backend in [Backend::Postgres, Backend::Sqlite] {
        let mut fixture = Fixture::new("idempotence-2", backend, sqlite_url());
        let first = full_schema(backend);
        let second = evolved_schema(backend);

        assert!(fixture.generate(&Schema::empty(), &first));
        assert!(fixture.generate(&first, &second));

        let snapshot = fixture.generator().read_snapshot().expect("reads");
        assert!(
            !fixture.generate(&snapshot, &second),
            "a third run must find nothing on {backend}"
        );
        assert_eq!(
            moso_migrate::runner::read_directory(&fixture.directory)
                .expect("reads")
                .len(),
            2
        );
    }
}

// ── the operation-coverage table, row by row ────────────────────────────────

/// Applies `before -> after` on a real database and asserts the schema matches.
/// Every row of the operation table is one call to this.
async fn operation_round_trips(
    label: &str,
    backend: Backend,
    url: String,
    before: Schema,
    after: Schema,
) {
    let mut fixture = Fixture::new(label, backend, url);
    if !before.is_empty() {
        assert!(
            fixture.generate(&Schema::empty(), &before),
            "{label}: setup"
        );
    }
    let changed = fixture.generate(&before, &after);
    assert!(changed, "{label}: the diff produced no migration");

    let mut runner = fixture.runner().await;
    if backend == Backend::Postgres {
        reset_postgres(&mut runner).await;
    }
    runner
        .migrate(&RunnerOptions::default().allow_destructive())
        .await
        .unwrap_or_else(|error| panic!("{label}: {error}"));

    let live = read_schema(runner.connection()).await.expect("reads");
    let drift = compare(&live, &after).expect("compares");
    assert!(drift.is_empty(), "{label}:\n{drift}");
    runner.close().await;
}

fn base() -> Table {
    let mut table = Table::new("t").for_entity("T");
    table.add_column(Column::new("id", DataType::BigSerial));
    table.set_primary_key(["id"]);
    table
}

fn one(table: Table) -> Schema {
    let mut schema = Schema::empty();
    schema.add_table(table);
    schema
}

macro_rules! coverage {
    ($name:ident, $before:expr, $after:expr) => {
        mod $name {
            use super::*;

            #[tokio::test]
            async fn sqlite() {
                operation_round_trips(
                    concat!(stringify!($name), "-sqlite"),
                    Backend::Sqlite,
                    sqlite_url(),
                    $before,
                    $after,
                )
                .await;
            }

            #[tokio::test]
            async fn postgres() {
                let Some(url) = postgres_url(concat!(stringify!($name), "::postgres")) else {
                    return;
                };
                let _turn = postgres_turn();
                operation_round_trips(
                    concat!(stringify!($name), "-pg"),
                    Backend::Postgres,
                    url,
                    $before,
                    $after,
                )
                .await;
            }
        }
    };
}

coverage!(new_entity, Schema::empty(), one(base()));

coverage!(removed_entity, one(base()), Schema::empty());

coverage!(new_field, one(base()), {
    let mut table = base();
    table.add_column(Column::new("bio", DataType::Text).nullable());
    one(table)
});

coverage!(new_field_not_null_with_default, one(base()), {
    let mut table = base();
    table.add_column(Column::new("locale", DataType::Text).with_default("'en'"));
    one(table)
});

coverage!(
    removed_field,
    {
        let mut table = base();
        table.add_column(Column::new("legacy", DataType::Text).nullable());
        one(table)
    },
    one(base())
);

coverage!(
    type_change,
    {
        let mut table = base();
        table.add_column(Column::new("n", DataType::Integer).nullable());
        one(table)
    },
    {
        let mut table = base();
        table.add_column(Column::new("n", DataType::BigInt).nullable());
        one(table)
    }
);

coverage!(
    nullability_change,
    {
        let mut table = base();
        table.add_column(
            Column::new("n", DataType::BigInt)
                .nullable()
                .with_default("0"),
        );
        one(table)
    },
    {
        let mut table = base();
        table.add_column(Column::new("n", DataType::BigInt).with_default("0"));
        one(table)
    }
);

coverage!(
    default_change,
    {
        let mut table = base();
        table.add_column(Column::new("n", DataType::BigInt).with_default("0"));
        one(table)
    },
    {
        let mut table = base();
        table.add_column(Column::new("n", DataType::BigInt).with_default("1"));
        one(table)
    }
);

coverage!(new_index, one(base()), {
    let mut table = base();
    table.add_column(Column::new("email", DataType::Text).nullable());
    table.add_index(Index::new("idx_t_email", ["email"]));
    one(table)
});

coverage!(new_unique_index, one(base()), {
    let mut table = base();
    table.add_column(Column::new("email", DataType::Text).nullable());
    table.add_index(
        Index::new("t_email_key", ["email"])
            .unique()
            .backing_a_constraint(),
    );
    one(table)
});

coverage!(
    new_foreign_key,
    {
        let mut schema = one(base());
        let mut child = Table::new("c").for_entity("C");
        child.add_column(Column::new("id", DataType::BigSerial));
        child.add_column(Column::new("t_id", DataType::BigInt).nullable());
        child.set_primary_key(["id"]);
        schema.add_table(child);
        schema
    },
    {
        let mut schema = one(base());
        let mut child = Table::new("c").for_entity("C");
        child.add_column(Column::new("id", DataType::BigSerial));
        child.add_column(Column::new("t_id", DataType::BigInt).nullable());
        child.set_primary_key(["id"]);
        child.add_foreign_key(ForeignKey::new("c_t_id_fkey", ["t_id"], "t", ["id"]));
        schema.add_table(child);
        schema
    }
);

coverage!(new_check_constraint, one(base()), {
    let mut table = base();
    table.add_check(Check::new("t_id_positive", "id > 0"));
    one(table)
});

coverage!(many_to_many_join_table, one(base()), {
    let mut schema = one(base());
    let mut other = Table::new("u").for_entity("U");
    other.add_column(Column::new("id", DataType::BigSerial));
    other.set_primary_key(["id"]);
    schema.add_table(other);

    let mut join = Table::new("t_u");
    join.add_column(Column::new("t_id", DataType::BigInt));
    join.add_column(Column::new("u_id", DataType::BigInt));
    join.set_primary_key(["t_id", "u_id"]);
    join.add_foreign_key(ForeignKey::new("t_u_t_id_fkey", ["t_id"], "t", ["id"]));
    join.add_foreign_key(ForeignKey::new("t_u_u_id_fkey", ["u_id"], "u", ["id"]));
    join.add_index(Index::new("t_u_u_id_idx", ["u_id"]));
    schema.add_table(join);
    schema
});

// PostgreSQL-only rows. SQLite has no user-defined types, no schemas and no
// extensions, and the generator says so rather than pretending.

#[tokio::test]
async fn enum_variant_added_on_postgres() {
    let Some(url) = postgres_url("enum_variant_added_on_postgres") else {
        return;
    };
    let _turn = postgres_turn();
    let mut before = one(base());
    before.add_enum(EnumType::new("role", ["admin", "member"]));
    let mut after = one(base());
    after.add_enum(EnumType::new("role", ["admin", "member", "auditor"]));

    operation_round_trips("enum-add", Backend::Postgres, url, before, after).await;
}

#[tokio::test]
async fn enum_variant_removed_emits_a_commented_template() {
    let mut before = Schema::empty();
    before.add_enum(EnumType::new("role", ["admin", "member"]));
    let mut after = Schema::empty();
    after.add_enum(EnumType::new("role", ["admin"]));

    let mut fixture = Fixture::new("enum-remove", Backend::Postgres, sqlite_url());
    assert!(fixture.generate(&before, &after));

    let files = moso_migrate::runner::read_directory(&fixture.directory).expect("reads");
    let body = files[0].body();
    assert!(body.contains("⚠ DESTRUCTIVE"), "{body}");
    assert!(body.contains("CREATE TYPE"), "{body}");
    assert_eq!(files[0].pending_destructive().len(), 1);
}

#[tokio::test]
async fn schema_and_extension_on_postgres() {
    let Some(url) = postgres_url("schema_and_extension_on_postgres") else {
        return;
    };
    let _turn = postgres_turn();
    let mut after = Schema::empty();
    after.add_extension("pg_trgm");
    let mut table = Table::new("events").in_schema("analytics");
    table.add_column(Column::new("id", DataType::BigSerial));
    table.set_primary_key(["id"]);
    after.add_table(table);

    let mut fixture = Fixture::new("schema-ext", Backend::Postgres, url);
    assert!(fixture.generate(&Schema::empty(), &after));

    let mut runner = fixture.runner().await;
    reset_postgres(&mut runner).await;
    runner
        .connection()
        .execute("DROP SCHEMA IF EXISTS analytics CASCADE")
        .await
        .expect("cleans");
    runner
        .migrate(&RunnerOptions::default())
        .await
        .expect("applies");

    let live = read_schema_including(runner.connection(), &["analytics".to_owned()])
        .await
        .expect("reads");
    assert!(live.table("analytics.events").is_some());
    assert!(live.extensions().any(|name| name == "pg_trgm"));

    runner
        .connection()
        .execute("DROP SCHEMA IF EXISTS analytics CASCADE")
        .await
        .expect("cleans");
    runner.close().await;
}

#[tokio::test]
async fn partitioning_on_postgres() {
    let Some(url) = postgres_url("partitioning_on_postgres") else {
        return;
    };
    let _turn = postgres_turn();
    let mut table = Table::new("events").for_entity("Event");
    table.add_column(Column::new("id", DataType::BigInt));
    table.add_column(Column::new(
        "created_at",
        DataType::Timestamp {
            with_time_zone: true,
        },
    ));
    table.set_primary_key(["id", "created_at"]);
    let table = table.partitioned_by(moso_migrate::schema::Partition::range(["created_at"]));

    let mut fixture = Fixture::new("partition", Backend::Postgres, url);
    assert!(fixture.generate(&Schema::empty(), &one(table)));

    let files = moso_migrate::runner::read_directory(&fixture.directory).expect("reads");
    assert!(
        files[0]
            .body()
            .contains("PARTITION BY RANGE (\"created_at\")"),
        "{}",
        files[0].body()
    );

    let mut runner = fixture.runner().await;
    reset_postgres(&mut runner).await;
    runner
        .migrate(&RunnerOptions::default())
        .await
        .expect("a partitioned table is created");
    runner.close().await;
}

// ── rename detection, both forms ────────────────────────────────────────────

#[test]
fn rename_detection_prompts_and_takes_the_non_interactive_form() {
    use moso_migrate::rename::{Oracle, Prompt, RenameAnswer, RenameQuestion};

    // The interactive form.
    let prompt = Prompt::new(&b"r\n"[..], Vec::new());
    assert_eq!(
        prompt
            .answer(&RenameQuestion::column("users", "name", "full_name"))
            .expect("answers"),
        RenameAnswer::Rename
    );

    // The `--rename old:new` form.
    let scripted = Scripted::parse(["users.name:full_name"]).expect("parses");
    let mut before = Table::new("users").for_entity("User");
    before.add_column(Column::new("id", DataType::BigSerial));
    before.add_column(Column::new("name", DataType::Text).nullable());
    before.set_primary_key(["id"]);

    let mut after = Table::new("users").for_entity("User");
    after.add_column(Column::new("id", DataType::BigSerial));
    after.add_column(Column::new("full_name", DataType::Text).nullable());
    after.set_primary_key(["id"]);

    let diff = Diff::compute(&one(before), &one(after), &scripted).expect("diffs");
    assert_eq!(diff.len(), 1);
    assert!(!diff.is_destructive(), "a rename keeps the data");
}

#[tokio::test]
async fn a_renamed_column_keeps_its_data_on_sqlite() {
    let mut before = Table::new("users").for_entity("User");
    before.add_column(Column::new("id", DataType::BigSerial));
    before.add_column(Column::new("name", DataType::Text).nullable());
    before.set_primary_key(["id"]);

    let mut after = Table::new("users").for_entity("User");
    after.add_column(Column::new("id", DataType::BigSerial));
    after.add_column(Column::new("full_name", DataType::Text).nullable());
    after.set_primary_key(["id"]);

    let mut fixture = Fixture::new("rename", Backend::Sqlite, sqlite_url());
    assert!(fixture.generate(&Schema::empty(), &one(before.clone())));

    let mut runner = fixture.runner().await;
    runner
        .migrate(&RunnerOptions::default())
        .await
        .expect("applies");
    runner
        .connection()
        .execute("INSERT INTO \"users\" (\"name\") VALUES ('Ada')")
        .await
        .expect("inserts");
    runner.close().await;

    let scripted = Scripted::parse(["users.name:full_name"]).expect("parses");
    let generated = fixture
        .generator()
        .make_migration_between(&one(before), &one(after), None, &scripted)
        .expect("diffs")
        .expect("a migration");
    generated.write().expect("writes");

    let mut runner = fixture.runner().await;
    runner
        .migrate(&RunnerOptions::default())
        .await
        .expect("applies");
    let rows = runner
        .connection()
        .fetch_text("SELECT \"full_name\" FROM \"users\"")
        .await
        .expect("reads");
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0][0].as_deref(),
        Some("Ada"),
        "the rename kept the row"
    );
    runner.close().await;
}

// ── concurrency ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn ten_concurrent_migrators_apply_each_migration_once() {
    let Some(url) = postgres_url("ten_concurrent_migrators_apply_each_migration_once") else {
        return;
    };
    let _turn = postgres_turn();

    let mut fixture = Fixture::new("concurrent", Backend::Postgres, url.clone());
    let mut table = Table::new("counted").for_entity("Counted");
    table.add_column(Column::new("id", DataType::BigSerial));
    table.set_primary_key(["id"]);
    assert!(fixture.generate(&Schema::empty(), &one(table)));

    {
        let mut runner = fixture.runner().await;
        reset_postgres(&mut runner).await;
        // Create the ledger table before the race: ten concurrent
        // `CREATE TABLE IF NOT EXISTS` calls are a PostgreSQL catalogue race,
        // and that is not what this test is about.
        runner.status().await.expect("creates the ledger");
        runner.close().await;
    }

    let directory = fixture.directory.clone();
    let mut handles = Vec::new();
    for _ in 0..10 {
        let directory = directory.clone();
        let url = url.clone();
        handles.push(tokio::spawn(async move {
            let mut runner = Runner::open(&directory, &url).await.expect("opens");
            let report = runner.migrate(&RunnerOptions::default()).await;
            runner.close().await;
            report.map(|report| report.applied().len())
        }));
    }

    let mut applied_total = 0;
    for handle in handles {
        let outcome = handle.await.expect("the task did not panic");
        applied_total += outcome.expect("every migrator succeeded");
    }
    assert_eq!(
        applied_total, 1,
        "exactly one of the ten processes applied the migration"
    );

    let mut runner = fixture.runner().await;
    let status = runner.status().await.expect("status");
    assert_eq!(status.applied().len(), 1);
    assert!(status.is_clean());
    runner.close().await;
}

// ── the lock-timeout guarantee ──────────────────────────────────────────────

#[tokio::test]
async fn a_migration_fails_fast_rather_than_queuing_behind_a_lock() {
    let Some(url) = postgres_url("a_migration_fails_fast_rather_than_queuing_behind_a_lock") else {
        return;
    };
    let _turn = postgres_turn();

    let mut fixture = Fixture::new("locked", Backend::Postgres, url.clone());
    let mut table = Table::new("locked_table").for_entity("Locked");
    table.add_column(Column::new("id", DataType::BigSerial));
    table.set_primary_key(["id"]);
    let first = one(table.clone());
    assert!(fixture.generate(&Schema::empty(), &first));

    {
        let mut runner = fixture.runner().await;
        reset_postgres(&mut runner).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("applies");
        runner.close().await;
    }

    // A second migration that needs ACCESS EXCLUSIVE on the same table.
    let mut evolved = table;
    evolved.add_column(Column::new("extra", DataType::Text).nullable());
    assert!(fixture.generate(&first, &one(evolved)));

    // Hold a conflicting lock on another connection.
    let mut blocker = moso_migrate::conn::Connection::open(&url)
        .await
        .expect("opens");
    blocker.execute("BEGIN").await.expect("begins");
    blocker
        .execute("LOCK TABLE \"locked_table\" IN ACCESS EXCLUSIVE MODE")
        .await
        .expect("locks");

    let mut runner = fixture.runner().await;
    // `lock_timeout` is one second for this file, so the migration must fail in
    // about a second rather than queueing behind the held lock.
    runner
        .set_timeouts(
            std::time::Duration::from_millis(500),
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("sets");
    let started = std::time::Instant::now();
    let error = runner
        .migrate(&RunnerOptions::default().lock_wait(std::time::Duration::from_secs(5)))
        .await
        .expect_err("the lock is held");
    let elapsed = started.elapsed();

    assert!(
        elapsed < std::time::Duration::from_secs(20),
        "it queued instead of failing fast: {elapsed:?}"
    );
    assert!(
        error.to_string().to_lowercase().contains("lock")
            || error.to_string().contains("canceling"),
        "{error}"
    );

    blocker.execute("ROLLBACK").await.expect("releases");
    blocker.close().await;
    runner.close().await;
}
