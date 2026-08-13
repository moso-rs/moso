//! How `moso db` talks to this application's database.
//!
//! `moso db` does not link your crate. Every subcommand runs
//!
//! ```text
//! cargo run --quiet -- --db-<command>
//! ```
//!
//! and reads exactly one JSON document off standard output — the same shape of
//! protocol as `src/dump.rs`, for the same reason: the CLI is one prebuilt
//! binary and your application is arbitrary code, so the CLI asks and your
//! `main` answers.
//!
//! | flag                         | what it does                                     |
//! | ---------------------------- | ------------------------------------------------ |
//! | `--db-status`                | reports applied, pending and dirty migrations    |
//! | `--db-migrate`               | applies every pending migration                  |
//! | `--db-migrate-tenants`       | applies them to every tenant [`tenants`] lists   |
//! | `--db-rollback <N>`          | reverts the last `N` (default 1)                 |
//! | `--db-redo`                  | reverts one and re-applies it                    |
//! | `--db-make-migration <NAME>` | diffs your entities and writes the migration     |
//! | `--db-check`                 | drift: your entities against the live database   |
//! | `--db-squash`                | collapses every migration into one baseline      |
//! | `--db-seed [NAME]`           | runs the seeds registered in [`seeds`]           |
//!
//! Five modifiers may follow one of those, never appear on their own, and are
//! ignored by the commands that do not read them:
//!
//! | modifier                | read by          | what it changes                    |
//! | ----------------------- | ---------------- | ---------------------------------- |
//! | `--db-dry-run`          | `make-migration` | builds the files and writes nothing |
//! | `--db-rename <OLD:NEW>` | `make-migration` | answers one rename question         |
//! | `--db-drop-and-add`     | `make-migration` | answers the rest as drop-and-add    |
//! | `--db-apply`            | `squash`         | writes the baseline and deletes     |
//! | `--db-force`            | `seed`           | seeds a production profile anyway   |
//!
//! # Why this file is yours and not the framework's
//!
//! Because the interesting decisions are yours. A migration that needs
//! application logic — backfilling a column by calling your own code — is
//! registered in [`register`], and the framework has no way to know about it.
//! So is the list of entities the generator diffs against, the list of tenants a
//! routed deployment has, and the fixture data a seed inserts. All four are
//! visible, in this file, rather than behind a flag on a binary you did not
//! write. There is no link-time registry (ADR-0004), so nothing can discover
//! any of them for you, and nothing pretends to.
//!
//! Everything except the one JSON document goes to standard error, or the CLI
//! cannot parse the answer.

// Named imports rather than two `prelude::*`: `moso` and `moso_migrate` both
// export an `Error` and a `Result`, and two glob imports of the same name are
// an ambiguity error at every use site rather than a shadowing that silently
// picks one. `Result` below is always Moso's; the migration crate's is spelled
// `MigrateResult`.
use moso::deps::serde_json::{Value, json, to_value};
// `Config` is the trait that provides `AppConfig::load_from`; it is in the
// prelude, which this file deliberately does not glob-import.
use moso::{Config as _, Error, Result};
use moso_migrate::command::{
    self, MakeMigrationOptions, RenamePolicy, SquashOptions, TenantTarget,
};
use moso_migrate::conn::backend_of;
use moso_migrate::prelude::BoxFuture;
use moso_migrate::rust_migration::Migrator;
use moso_migrate::seed::{Seed, SeedOptions, Seeds};
use moso_migrate::{MigrateReport, Result as MigrateResult, Runner, RunnerOptions, Status};

/// Where the migration files live, relative to the crate root.
pub const MIGRATIONS: &str = "migrations";

/// One thing `moso db` can ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Report what is applied and what is pending.
    Status,
    /// Apply every pending migration.
    Migrate,
    /// Apply every pending migration to every tenant [`tenants`] lists.
    MigrateTenants,
    /// Revert the last `n` migrations.
    Rollback(usize),
    /// Revert one migration and apply it again.
    Redo,
    /// Diff the entity graph against the snapshot and write a migration.
    MakeMigration {
        /// What to call it. Slugified. Without one the generator suggests a
        /// name from the diff itself.
        name: Option<String>,
        /// Build the files and write nothing.
        dry_run: bool,
        /// `old:new` answers to the rename questions the diff cannot settle.
        renames: Vec<String>,
        /// Answer every remaining rename question as a drop and an add.
        drop_and_add: bool,
    },
    /// Compare the entity graph with the live database, in both directions.
    Check,
    /// Collapse every migration into one baseline.
    Squash {
        /// Write the baseline and delete the files it replaces.
        apply: bool,
    },
    /// Run one seed, or every seed.
    Seed {
        /// Which seed. Every registered one when absent.
        name: Option<String>,
        /// Run even under a production profile.
        force: bool,
    },
}

/// The command the command line asked for, if any.
///
/// The primary flag comes first and its modifiers follow it, which is what lets
/// this be a `position` and a handful of lookups rather than a parser. A
/// modifier on its own is not a command: `requested` returns `None` and `main`
/// goes on to serve.
#[must_use]
pub fn requested() -> Option<Command> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let position = arguments.iter().position(|argument| {
        matches!(
            argument.as_str(),
            "--db-status"
                | "--db-migrate"
                | "--db-migrate-tenants"
                | "--db-rollback"
                | "--db-redo"
                | "--db-make-migration"
                | "--db-check"
                | "--db-squash"
                | "--db-seed"
        )
    })?;
    let rest = &arguments[position + 1..];

    match arguments[position].as_str() {
        "--db-status" => Some(Command::Status),
        "--db-migrate" => Some(Command::Migrate),
        "--db-migrate-tenants" => Some(Command::MigrateTenants),
        "--db-redo" => Some(Command::Redo),
        "--db-check" => Some(Command::Check),
        "--db-squash" => Some(Command::Squash {
            apply: rest.iter().any(|argument| argument == "--db-apply"),
        }),
        "--db-rollback" => {
            // Without a count it reverts a single migration, which is the only
            // default that cannot surprise somebody.
            let steps = rest
                .first()
                .and_then(|next| next.parse::<usize>().ok())
                .unwrap_or(1);
            Some(Command::Rollback(steps.max(1)))
        }
        "--db-make-migration" => Some(Command::MakeMigration {
            name: argument_of(rest),
            dry_run: rest.iter().any(|argument| argument == "--db-dry-run"),
            renames: values_of(rest, "--db-rename"),
            drop_and_add: rest.iter().any(|argument| argument == "--db-drop-and-add"),
        }),
        "--db-seed" => Some(Command::Seed {
            name: argument_of(rest),
            force: rest.iter().any(|argument| argument == "--db-force"),
        }),
        _ => None,
    }
}

/// The value that immediately follows the primary flag, when there is one.
///
/// Only the *first* token, and only when it is not itself a flag: a name is
/// passed adjacent to the command it names, so anything further along belongs
/// to a modifier and must not be mistaken for it.
fn argument_of(rest: &[String]) -> Option<String> {
    rest.first()
        .filter(|argument| !argument.starts_with("--"))
        .cloned()
}

/// Every value of a repeatable modifier, in the order they were given.
fn values_of(rest: &[String], flag: &str) -> Vec<String> {
    rest.windows(2)
        .filter(|pair| pair[0] == flag)
        .map(|pair| pair[1].clone())
        .collect()
}

/// Answer `command` with one JSON document on standard output.
///
/// # Errors
/// When `DATABASE_URL` is unset or unreachable, when a migration fails, when
/// the ledger disagrees with the files on disk, and when a command that reads
/// your entity graph is asked for while that graph is empty.
pub async fn run(command: Command) -> Result<()> {
    let config = crate::AppConfig::load_from(&crate::loader()?)?;
    let url = config.database_url.expose();

    let profile = moso::config::Profile::detect().to_string();
    let options = RunnerOptions::default().profile(profile.clone());

    // ── your entity graph ───────────────────────────────────────────────────
    //
    // `make-migration`, `check` and `squash` all diff against this list, and it
    // is empty because this project has no `#[derive(Entity)]` models yet.
    // Nothing can find them for you — there is no link-time registry — so each
    // one is named here, in a statement you can read:
    //
    //     let entities = vec![crate::models::User::descriptor()];
    //
    // Spelling the element type needs `moso`'s `orm` feature, which is also
    // what gives you `#[derive(Entity)]`; until then the empty list takes its
    // type from the calls below. The three commands that read it refuse while
    // it is empty rather than reporting every table in the database as drift.
    let entities = Vec::new();

    let document = match &command {
        Command::Status => status(url).await?,
        Command::Migrate => migrate(url, &options).await?,
        Command::Rollback(steps) => rollback(url, *steps).await?,
        Command::Redo => redo(url, &options).await?,

        Command::MigrateTenants => {
            let report = command::migrate_tenants(MIGRATIONS, &tenants(), &options, &register)
                .await
                .map_err(Error::internal)?;
            document_of(&report)?
        }

        Command::MakeMigration {
            name,
            dry_run,
            renames,
            drop_and_add,
        } => {
            require_entities(&entities, "make-migration")?;
            let backend = backend_of(url).map_err(Error::internal)?;
            let mut wanted =
                MakeMigrationOptions::default().renames(rename_policy(renames, *drop_and_add));
            if let Some(name) = name {
                wanted = wanted.name(name);
            }

            // Plan before writing. `command::make_migration` writes with
            // `std::fs::write`, which truncates, and the version is a timestamp
            // to the second — so two runs inside one second name one file. The
            // dry run costs nothing (it reads the snapshot and computes) and it
            // is what lets an existing file be refused instead of replaced.
            let planned =
                command::make_migration(MIGRATIONS, backend, &entities, &wanted.clone().dry_run())
                    .map_err(Error::internal)?;

            let report = if *dry_run {
                planned
            } else {
                refuse_to_overwrite(planned.path())?;
                command::make_migration(MIGRATIONS, backend, &entities, &wanted)
                    .map_err(Error::internal)?
            };
            document_of(&report)?
        }

        Command::Check => {
            require_entities(&entities, "check")?;
            let report = command::check(MIGRATIONS, url, &entities)
                .await
                .map_err(Error::internal)?;
            document_of(&report)?
        }

        Command::Squash { apply } => {
            require_entities(&entities, "squash")?;
            let backend = backend_of(url).map_err(Error::internal)?;
            refuse_partial_squash(&read_status(url).await?)?;
            let mut wanted = SquashOptions::default();
            if *apply {
                wanted = wanted.apply();
            }
            let report = command::squash(MIGRATIONS, backend, &entities, &wanted)
                .map_err(Error::internal)?;
            document_of(&report)?
        }

        Command::Seed { name, force } => {
            let mut wanted = SeedOptions::default().profile(profile);
            if *force {
                wanted = wanted.force();
            }
            let report = command::seed(url, &seeds(), name.as_deref(), &wanted)
                .await
                .map_err(Error::internal)?;
            document_of(&report)?
        }
    };

    println!(
        "{}",
        moso::deps::serde_json::to_string_pretty(&document).map_err(Error::internal)?
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// What only you can supply
// ---------------------------------------------------------------------------

/// Registers the migrations that need application logic.
///
/// One home, called by the single-database path *and* by every tenant, so a
/// backfill cannot run against your own database and be skipped for a customer.
/// A [`RustMigration`](moso_migrate::rust_migration::RustMigration) runs inside
/// the same transaction and the same advisory lock as the SQL ones, so it
/// cannot half-apply.
///
/// Add one by dropping the underscore and calling `runner.register(MyBackfill)`.
fn register(_runner: &mut Runner) {}

/// Every tenant `moso db migrate --all-tenants` should migrate.
///
/// Empty, because Moso does not know where you keep your tenant list — it is a
/// table, a config file or a directory of URLs, and it is yours. Build one
/// target per tenant:
///
/// ```ignore
/// vec![
///     TenantTarget::schema("acme", url, "tenant_acme"),   // schema per tenant
///     TenantTarget::database("globex", "postgres://…/globex"), // database each
/// ]
/// ```
///
/// A tenant that fails does not stop the others: each one's outcome is a row in
/// the report, and `moso db migrate --all-tenants` exits non-zero naming the
/// ones that failed.
fn tenants() -> Vec<TenantTarget> {
    Vec::new()
}

/// The seeds `moso db seed` can run.
///
/// A seed is not a migration: it is not versioned, it is not recorded, and it
/// is meant to be run again — so idempotence is the seed's own job, which is
/// what `ON CONFLICT DO NOTHING` below is doing.
fn seeds() -> Seeds {
    let mut seeds = Seeds::default();
    seeds.add(Dev);
    seeds
}

/// Fixture data for a development database.
///
/// `is_safe_in_production` is left at its default of `false`, so `moso db seed`
/// refuses under a production profile unless `--force` is typed. Leave it that
/// way: a seeded account in production is an incident, not a convenience.
struct Dev;

impl Seed for Dev {
    fn name(&self) -> &str {
        "dev"
    }

    fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, MigrateResult<()>> {
        Box::pin(async move {
            migrator
                .execute(
                    "INSERT INTO greetings (id, name, message, created_at) \
                     VALUES (1, 'world', 'hello, world', CURRENT_TIMESTAMP) \
                     ON CONFLICT DO NOTHING",
                )
                .await?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// The four commands that hold one runner
// ---------------------------------------------------------------------------

/// Open the runner and register everything that needs registering.
async fn open(url: &str) -> Result<Runner> {
    let mut runner = Runner::open(MIGRATIONS, url)
        .await
        .map_err(Error::internal)?;
    register(&mut runner);
    Ok(runner)
}

/// Everything `moso db status` renders.
async fn status(url: &str) -> Result<Value> {
    let mut runner = open(url).await?;
    let outcome = runner.status().await;
    runner.close().await;
    let status = outcome.map_err(Error::internal)?;

    let applied: Vec<Value> = status
        .applied()
        .iter()
        .map(|row| {
            json!({
                "version": row.version().to_string(),
                "name": row.name(),
                "applied_at": row.applied_at(),
                "duration_ms": row.duration_ms(),
                "dirty": row.is_dirty(),
                "applied_by": row.applied_by(),
                "failure": row.failure(),
            })
        })
        .collect();

    Ok(json!({
        "command": "status",
        "clean": status.is_clean(),
        "applied": applied,
        "pending": status.pending().iter().map(ToString::to_string).collect::<Vec<_>>(),
        "dirty": status.dirty().iter().map(|row| row.version().to_string()).collect::<Vec<_>>(),
        // A file whose checksum no longer matches what was applied: somebody
        // edited a migration after it ran, which is the one thing a ledger
        // exists to catch.
        "changed": status.changed().iter().map(|(version, _, _)| version.to_string()).collect::<Vec<_>>(),
        // Applied, but the file is gone.
        "missing": status.missing().iter().map(ToString::to_string).collect::<Vec<_>>(),
        "out_of_order": status.out_of_order().iter().map(|(pending, applied)| json!({
            "pending": pending.to_string(),
            "applied_after": applied.to_string(),
        })).collect::<Vec<_>>(),
    }))
}

/// `moso db migrate`.
async fn migrate(url: &str, options: &RunnerOptions) -> Result<Value> {
    let mut runner = open(url).await?;
    let outcome = runner.migrate(options).await;
    runner.close().await;
    Ok(report_json(&outcome.map_err(Error::internal)?))
}

/// `moso db redo`.
async fn redo(url: &str, options: &RunnerOptions) -> Result<Value> {
    let mut runner = open(url).await?;
    let outcome = runner.redo(options).await;
    runner.close().await;
    Ok(report_json(&outcome.map_err(Error::internal)?))
}

/// `moso db rollback`.
async fn rollback(url: &str, steps: usize) -> Result<Value> {
    let mut runner = open(url).await?;
    let outcome = runner.rollback(steps).await;
    runner.close().await;
    let reverted = outcome.map_err(Error::internal)?;
    Ok(json!({
        "command": "rollback",
        "reverted": reverted.iter().map(ToString::to_string).collect::<Vec<_>>(),
    }))
}

/// The ledger, read and released — what `squash` checks itself against.
async fn read_status(url: &str) -> Result<Status> {
    let mut runner = open(url).await?;
    let outcome = runner.status().await;
    runner.close().await;
    outcome.map_err(Error::internal)
}

/// The shape `migrate` and `redo` both answer with.
fn report_json(report: &MigrateReport) -> Value {
    json!({
        "command": "migrate",
        "up_to_date": report.is_up_to_date(),
        "applied": report
            .applied()
            .iter()
            .map(|(version, name, elapsed)| json!({
                "version": version.to_string(),
                "name": name,
                "duration_ms": elapsed.as_millis(),
            }))
            .collect::<Vec<_>>(),
        "skipped": report.skipped().iter().map(ToString::to_string).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

/// Turn one of `moso_migrate`'s reports into the document to print.
///
/// Every report's field names *are* the JSON keys, so there is nothing to
/// translate: a renamed field reaches `moso db` without this file being edited.
fn document_of<T: moso::deps::serde::Serialize>(report: &T) -> Result<Value> {
    to_value(report).map_err(Error::internal)
}

/// Refuse a command that can only lie while the entity list is empty.
///
/// Generic over the element so that it reads before the type is nameable: it
/// asks whether there is anything in the list, and nothing else.
fn require_entities<T>(entities: &[T], command: &str) -> Result<()> {
    if !entities.is_empty() {
        return Ok(());
    }
    Err(Error::internal_msg(format!(
        "`moso db {command}` compares your Rust entities with the database, and the entity list \
         in src/db.rs is empty — an empty graph would report every table as drift and generate a \
         migration that drops them, so it is refused; list your #[derive(Entity)] models in \
         `run`'s `entities` binding first"
    )))
}

/// Refuse to replace a migration that is already on disk.
///
/// Only reachable when two generations land in the same second, or when a
/// snapshot was reverted while its `.sql` was kept — but the cost of the check
/// is one `exists` and the cost of being wrong is a lost migration.
fn refuse_to_overwrite(path: Option<&str>) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if !std::path::Path::new(path).exists() {
        return Ok(());
    }
    Err(Error::internal_msg(format!(
        "`{path}` already exists, so `moso db make-migration` would replace it; rename or delete \
         that file, or wait a second and run the command again — the version is a timestamp"
    )))
}

/// Refuse a squash while anything on disk is unapplied or the ledger is unsound.
///
/// A squash collapses **every** file into one baseline carrying
/// `-- moso:replaces`, and the runner records that baseline as applied without
/// running it only when every version it names is already in the ledger. Squash
/// with something pending and the baseline runs in full against a database that
/// already has half of it.
fn refuse_partial_squash(status: &Status) -> Result<()> {
    if !status.pending().is_empty() {
        return Err(Error::internal_msg(format!(
            "{} migration(s) on disk have not been applied to this database, so a baseline that \
             claims to replace them would run in full on every database that already has them; \
             run `moso db migrate` everywhere first",
            status.pending().len()
        )));
    }
    if !status.is_clean() {
        return Err(Error::internal_msg(
            "the ledger and the files on disk disagree, so the set of migrations a baseline would \
             replace cannot be established; run `moso db status` and fix what it names first",
        ));
    }
    Ok(())
}

/// How the flags spell an answer to a rename question.
///
/// A diff cannot tell a rename from a drop-and-add, and the difference is
/// whether the data survives. [`RenamePolicy::Ask`] is deliberately not
/// reachable from here: `moso db` runs this binary with standard input closed,
/// so a prompt would fail rather than ask. Refusing — and naming the flag that
/// answers — is the default for the same reason CI needs it to be.
fn rename_policy(renames: &[String], drop_and_add: bool) -> RenamePolicy {
    if renames.is_empty() {
        return if drop_and_add {
            RenamePolicy::DropAndAdd
        } else {
            RenamePolicy::Refuse
        };
    }
    RenamePolicy::Scripted {
        pairs: renames.to_vec(),
        strict: !drop_and_add,
    }
}
