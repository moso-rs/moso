//! `moso db` — the migration front end.
//!
//! # Why this does not open the database itself
//!
//! `moso-migrate` is a library and `Runner::open(directory, url)` would work
//! perfectly well from here, which makes "the CLI runs the migrations" the
//! obvious design. It is the wrong one, for two reasons.
//!
//! The first is that a migration may need application logic. `Runner::register`
//! takes a `RustMigration` — a backfill that calls your own code — and those are
//! registered in your `src/db.rs`, in your crate, which this binary cannot link
//! (ADR-0004: there is no link-time registry, so there is no global list to
//! discover). A CLI that ran migrations directly would silently skip them. The
//! same argument settles the newer commands: `make-migration`, `check` and
//! `squash` all diff *your entity graph*, and the list of entities is a
//! statement in your crate for exactly the same reason.
//!
//! The second is the dependency budget. Linking `moso-migrate` into the CLI
//! means linking `moso-orm`, `moso-sql` and `sqlx` — including a bundled SQLite
//! that compiles from C — into a binary whose other seven commands need none of
//! it. `03-crate-layout.md` rule 6 is already over budget.
//!
//! So this delegates, exactly as `moso routes` does: build the application, run
//! it with a `--db-*` flag, read one JSON document off standard output. The
//! application already depends on `moso-migrate`, because it is the thing with a
//! database. Each flag is answered by one call into `moso_migrate::command`,
//! whose report field names *are* the JSON keys read back below.
//!
//! # What it renders
//!
//! Tables, and an exit code. `--json` passes the application's own document
//! through unchanged, so a CI job can act on it without this command becoming
//! the thing that defines the schema.
//!
//! Three of the commands gate a pipeline, and each turns a different field into
//! exit code 1: `status` and `check` on `clean`, `migrate --all-tenants` on
//! `clean` again — a per-tenant failure is a row in the report rather than an
//! error, because tenants are independent and a deploy needs all of them.

use crate::cli::{
    DbArgs, DbCommand, DbMakeMigrationArgs, DbMigrateArgs, DbRollbackArgs, DbSeedArgs, DbSquashArgs,
};
use crate::exit::{CliError, Outcome};
use crate::project::{Db, Project};
use crate::ui::{Level, Ui};

/// Dispatch one `moso db` subcommand.
///
/// # Errors
/// Whatever the protocol or the migration itself failed with.
pub fn run(ui: &Ui, command: &DbCommand) -> Outcome<()> {
    match command {
        DbCommand::Status(args) => status(ui, args),
        DbCommand::Migrate(args) => {
            if args.all_tenants {
                migrate_tenants(ui, args)
            } else {
                migrate(ui, &args.app, &Db::Migrate)
            }
        }
        DbCommand::Redo(args) => migrate(ui, &args.app, &Db::Redo),
        DbCommand::Rollback(args) => rollback(ui, args),
        DbCommand::MakeMigration(args) => make_migration(ui, args),
        DbCommand::Check(args) => check(ui, args),
        DbCommand::Squash(args) => squash(ui, args),
        DbCommand::Seed(args) => seed(ui, args),
    }
}

/// `moso db status`.
fn status(ui: &Ui, args: &DbArgs) -> Outcome<()> {
    let document = ask(ui, &args.app, &Db::Status)?;
    if ui.is_json() {
        ui.emit_json(&document);
        return finish(
            &document,
            "the migration ledger and the files on disk disagree",
            "the specific problem and its fix are printed above",
        );
    }

    let applied = array(&document, "applied");
    let pending = array(&document, "pending");

    if applied.is_empty() && pending.is_empty() {
        ui.status(Level::Ok, "clean", "no migrations, nothing pending");
        return Ok(());
    }

    if applied.is_empty() {
        ui.line(&ui.dim("nothing applied yet"));
    } else {
        ui.heading("Applied");
        let rows: Vec<Vec<String>> = applied
            .iter()
            .map(|row| {
                vec![
                    text(row, "version"),
                    text(row, "name"),
                    text(row, "applied_at"),
                    format!(
                        "{} ms",
                        row.get("duration_ms").and_then(|v| v.as_i64()).unwrap_or(0)
                    ),
                    if row.get("dirty").and_then(serde_json::Value::as_bool) == Some(true) {
                        "DIRTY".to_owned()
                    } else {
                        String::new()
                    },
                ]
            })
            .collect();
        ui.table(&["version", "name", "applied at", "took", ""], &rows);
    }

    ui.blank();
    if pending.is_empty() {
        ui.status(Level::Ok, "up to date", "nothing pending");
    } else {
        ui.heading("Pending");
        for version in &pending {
            ui.line(&format!("  {}", string(version)));
        }
        ui.blank();
        ui.status(
            Level::Warn,
            "pending",
            &format!("{} migration(s); run `moso db migrate`", pending.len()),
        );
    }

    report_trouble(ui, &document);
    finish(
        &document,
        "the migration ledger and the files on disk disagree",
        "the specific problem and its fix are printed above",
    )
}

/// `moso db migrate` and `moso db redo`, which answer with the same document.
fn migrate(ui: &Ui, app: &crate::cli::AppArgs, command: &Db) -> Outcome<()> {
    let document = ask(ui, app, command)?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let applied = array(&document, "applied");
    if applied.is_empty() {
        ui.status(Level::Ok, "up to date", "nothing to apply");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = applied
        .iter()
        .map(|row| {
            vec![
                text(row, "version"),
                text(row, "name"),
                format!(
                    "{} ms",
                    row.get("duration_ms").and_then(|v| v.as_u64()).unwrap_or(0)
                ),
            ]
        })
        .collect();
    ui.table(&["version", "name", "took"], &rows);
    ui.blank();
    ui.status(
        Level::Ok,
        "applied",
        &format!("{} migration(s)", applied.len()),
    );
    Ok(())
}

/// `moso db rollback`.
fn rollback(ui: &Ui, args: &DbRollbackArgs) -> Outcome<()> {
    let document = ask(ui, &args.app, &Db::Rollback(args.steps))?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let reverted = array(&document, "reverted");
    if reverted.is_empty() {
        ui.status(Level::Ok, "nothing to do", "no applied migration to revert");
        return Ok(());
    }
    for version in &reverted {
        ui.status(Level::Ok, "reverted", &string(version));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// make-migration
// ---------------------------------------------------------------------------

/// `moso db make-migration <name>`.
///
/// The application writes the file; this renders what came out, including the
/// SQL itself. Printing the migration is not a nicety — the whole model is that
/// a human reads the file before it runs, and a generator whose output you have
/// to go and find is a generator people apply unread.
fn make_migration(ui: &Ui, args: &DbMakeMigrationArgs) -> Outcome<()> {
    let document = ask(
        ui,
        &args.app,
        &Db::MakeMigration {
            name: args.name.clone(),
            dry_run: args.dry_run,
            renames: args.rename.clone(),
            drop_and_add: args.drop_and_add,
        },
    )?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    if !flag(&document, "changed") {
        ui.status(
            Level::Ok,
            "up to date",
            "the entities and the snapshot already agree",
        );
        return Ok(());
    }

    let written = flag(&document, "written");
    let path = text(&document, "path");
    let snapshot = text(&document, "snapshot_path");

    let changes = array(&document, "changes");
    if !changes.is_empty() {
        ui.heading("Changes");
        for change in &changes {
            ui.line(&format!("  {}", string(change)));
        }
        ui.blank();
    }

    if let Some(migration) = document
        .get("migration")
        .and_then(serde_json::Value::as_str)
    {
        ui.heading(&path);
        ui.line(migration.trim_end());
        ui.blank();
    }

    for advice in &array(&document, "advice") {
        ui.warn(&text(advice, "summary"));
        for line in text(advice, "plan").lines() {
            ui.line(&format!("      {line}"));
        }
        ui.blank();
    }

    if flag(&document, "destructive") {
        ui.status(
            Level::Warn,
            "destructive",
            "the block is commented out and will not run",
        );
        ui.fix(
            "read it, and uncomment it in the same commit as the code that stops using what it \
             drops",
        );
    }

    if written {
        ui.status(Level::Ok, "wrote", &path);
        ui.status(Level::Ok, "wrote", &snapshot);
        ui.fix("read the file, then `moso db migrate`; commit the .sql and the snapshot together");
    } else {
        ui.status(Level::Info, "nothing written", "(--dry-run)");
        ui.fix(&format!("run it again without --dry-run to write {path}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// `moso db check`.
///
/// The gate. It exits non-zero on drift in either direction, so a pipeline can
/// depend on it, and it names what diverged rather than saying that something
/// did — "3 differences" sends someone to `psql` for twenty minutes.
fn check(ui: &Ui, args: &DbArgs) -> Outcome<()> {
    let document = ask(ui, &args.app, &Db::Check)?;
    if ui.is_json() {
        ui.emit_json(&document);
        return finish(&document, DRIFT, DRIFT_HELP);
    }

    let pending = array(&document, "pending");
    if !pending.is_empty() {
        ui.status(
            Level::Warn,
            "pending",
            &format!("{} migration(s) not applied here", pending.len()),
        );
        for version in &pending {
            ui.line(&format!("      {}", string(version)));
        }
        ui.blank();
    }

    for (key, headline) in [
        (
            "missing_in_database",
            "your entities describe it and the database does not have it",
        ),
        (
            "extra_in_database",
            "the database has it and no entity describes it",
        ),
        ("mismatched", "both have it, differently"),
    ] {
        let entries = array(&document, key);
        if entries.is_empty() {
            continue;
        }
        ui.heading(headline);
        for entry in &entries {
            ui.line(&format!("  {}", string(entry)));
        }
        ui.blank();
    }

    if flag(&document, "clean") {
        ui.status(Level::Ok, "no drift", "the database matches your entities");
        return Ok(());
    }

    ui.fix(
        "`moso db make-migration <name>` writes the migration that closes it, or revert whatever \
         changed the database by hand",
    );
    finish(&document, DRIFT, DRIFT_HELP)
}

/// The headline `check` fails with.
const DRIFT: &str = "the database does not match the entity graph";

/// And what to do about it.
const DRIFT_HELP: &str = "everything that diverged is named above";

// ---------------------------------------------------------------------------
// squash
// ---------------------------------------------------------------------------

/// `moso db squash`.
///
/// A report unless `--yes`. Two things it must always do: name every file it
/// would delete before deleting it, and refuse when the range is not applied
/// everywhere — the second is enforced by the application, which is the side
/// that can read the ledger.
fn squash(ui: &Ui, args: &DbSquashArgs) -> Outcome<()> {
    let document = ask(ui, &args.app, &Db::Squash { apply: args.yes })?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let path = text(&document, "path");
    let replaced = array(&document, "replaced");
    let removable = array(&document, "removable");

    ui.heading("Baseline");
    ui.status(
        Level::Info,
        &text(&document, "version"),
        &text(&document, "name"),
    );
    ui.line(&format!("  {path}"));
    ui.blank();

    if replaced.is_empty() {
        ui.status(
            Level::Warn,
            "replaces nothing",
            "there are no migrations to collapse",
        );
        return Ok(());
    }

    ui.heading(&format!("Replaces {} migration(s)", replaced.len()));
    for version in &replaced {
        ui.line(&format!("  {}", string(version)));
    }
    ui.blank();

    // Never delete a file without saying so — in both modes, using the same
    // list, so what a dry run shows is exactly what `--yes` removes.
    let verb = if flag(&document, "written") {
        "deleted"
    } else {
        "would delete"
    };
    ui.heading(&format!("{verb} {} file(s)", removable.len()));
    for file in &removable {
        ui.line(&format!("  {}", string(file)));
    }
    ui.blank();

    if flag(&document, "written") {
        ui.status(Level::Ok, "wrote", &path);
        ui.fix("commit the baseline and the deletions together, then tell the team to pull");
    } else {
        ui.status(Level::Info, "nothing written", "(no --yes)");
        ui.fix("run it again with --yes to write the baseline and delete the files above");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// seed
// ---------------------------------------------------------------------------

/// `moso db seed [name]`.
fn seed(ui: &Ui, args: &DbSeedArgs) -> Outcome<()> {
    let document = ask(
        ui,
        &args.app,
        &Db::Seed {
            name: args.name.clone(),
            force: args.force,
        },
    )?;
    if ui.is_json() {
        ui.emit_json(&document);
        return Ok(());
    }

    let ran = array(&document, "ran");
    let available = array(&document, "available");

    if available.is_empty() {
        ui.status(
            Level::Warn,
            "no seeds",
            "src/db.rs registers none, so there was nothing to run",
        );
        ui.fix("add one to `seeds()` in src/db.rs");
        return Ok(());
    }

    for name in &ran {
        ui.status(Level::Ok, "seeded", &string(name));
    }
    ui.blank();
    ui.status(
        Level::Ok,
        "ran",
        &format!(
            "{} of {} registered seed(s) under the `{}` profile",
            ran.len(),
            available.len(),
            text(&document, "profile")
        ),
    );
    if flag(&document, "forced") {
        ui.warn("--force was used: the production refusal was overridden");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// migrate --all-tenants
// ---------------------------------------------------------------------------

/// `moso db migrate --all-tenants`.
///
/// One line per tenant as the report is read, then a summary. The run does not
/// stop at the first failure — the application already carried on — so the
/// interesting output is which of the twenty tenants is now behind.
fn migrate_tenants(ui: &Ui, args: &DbMigrateArgs) -> Outcome<()> {
    let document = ask(ui, &args.app, &Db::MigrateTenants)?;
    if ui.is_json() {
        ui.emit_json(&document);
        return finish(&document, BEHIND, BEHIND_HELP);
    }

    let tenants = array(&document, "tenants");
    if tenants.is_empty() {
        return Err(
            CliError::user("the application listed no tenants, so nothing was migrated").with_help(
                "fill in `tenants()` in src/db.rs — Moso does not know where you keep the list, \
                 so it cannot be discovered",
            ),
        );
    }

    let mut failed: Vec<String> = Vec::new();
    for tenant in &tenants {
        let name = text(tenant, "tenant");
        match tenant.get("failure").and_then(serde_json::Value::as_str) {
            Some(failure) => {
                ui.status(Level::Fail, &name, failure);
                failed.push(name);
            }
            None => {
                let applied = array(tenant, "applied");
                let detail = if applied.is_empty() {
                    "up to date".to_owned()
                } else {
                    format!("{} migration(s) applied", applied.len())
                };
                ui.status(Level::Ok, &name, &detail);
            }
        }
    }

    ui.blank();
    if failed.is_empty() {
        ui.status(
            Level::Ok,
            "migrated",
            &format!("{} tenant(s)", tenants.len()),
        );
        return Ok(());
    }

    ui.status(
        Level::Fail,
        "behind",
        &format!(
            "{} of {} tenant(s): {}",
            failed.len(),
            tenants.len(),
            failed.join(", ")
        ),
    );
    finish(&document, BEHIND, BEHIND_HELP)
}

/// The headline `migrate --all-tenants` fails with.
const BEHIND: &str = "one or more tenants were not migrated";

/// And what to do about it.
const BEHIND_HELP: &str = "each failure is printed above with the tenant it belongs to; \
                           the others are migrated, so re-running is safe";

// ---------------------------------------------------------------------------
// The protocol
// ---------------------------------------------------------------------------

/// Build the application, run it with `command`, and parse the answer.
fn ask(ui: &Ui, app: &crate::cli::AppArgs, command: &Db) -> Outcome<serde_json::Value> {
    let project = Project::discover(app.manifest_path.as_deref())?;
    project.require_moso()?;

    // Check for the protocol before building, not after running it.
    //
    // A project generated without `--with-db` does not recognise `--db-status`,
    // so `main` falls through to `serve()` — and then this command waits out the
    // whole hour it allows a migration, to finally report a timeout about a flag
    // rather than the missing feature. One `is_file` turns that into a sentence.
    if !project.root.join("src/db.rs").is_file() {
        return Err(CliError::user(format!(
            "`{}` has no database story: src/db.rs is missing",
            project.name
        ))
        .with_help(
            "create a project with `moso new <name> --with-db`, or copy src/db.rs and the \
             migrations/ directory into this one — src/db.rs is what answers the --db-* \
             flags this command sends",
        ));
    }

    if ui.is_verbose() {
        ui.status(Level::Ok, "asking", &command.label());
    }

    let answer = project.db(app, command)?;
    serde_json::from_str(&answer).map_err(|error| {
        CliError::user(format!(
            "`{}` answered `{}` with something that is not JSON: {error}",
            project.name,
            command.label()
        ))
        .with_help("src/db.rs must print exactly one JSON document to stdout")
    })
}

/// Report the four states that mean the ledger and the files disagree.
///
/// Each of these is a genuine problem rather than a warning to scroll past, so
/// each gets its own line naming what to do.
fn report_trouble(ui: &Ui, document: &serde_json::Value) {
    for (key, headline, fix) in [
        (
            "dirty",
            "a previous run failed partway through",
            "inspect the database, finish or undo the change by hand, then delete the row from `moso_migrations`",
        ),
        (
            "changed",
            "a migration file was edited after it ran",
            "restore the original file; the database already has the old version applied",
        ),
        (
            "missing",
            "a migration is recorded as applied but its file is gone",
            "restore the file from version control, or the ledger cannot be verified",
        ),
        (
            "out_of_order",
            "a pending migration sorts before one already applied",
            "rename it to a later version, or apply it with `--allow-out-of-order` if you are certain",
        ),
    ] {
        let entries = array(document, key);
        if entries.is_empty() {
            continue;
        }
        ui.blank();
        ui.warn(&format!("{headline} ({} affected)", entries.len()));
        ui.fix(fix);
    }
}

/// Turn a `"clean": false` document into exit code 1.
///
/// `status`, `check` and `migrate --all-tenants` are all meant to be usable as
/// CI gates, and a gate that always exits 0 gates nothing. The headline differs
/// per command because "not clean" means three different things.
fn finish(document: &serde_json::Value, message: &str, help: &str) -> Outcome<()> {
    if document.get("clean").and_then(serde_json::Value::as_bool) == Some(false) {
        return Err(CliError::user(message.to_owned()).with_help(help.to_owned()));
    }
    Ok(())
}

/// Read an array field, or an empty slice.
fn array(document: &serde_json::Value, key: &str) -> Vec<serde_json::Value> {
    document
        .get(key)
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default()
}

/// Read a string field of an object, or the empty string.
fn text(row: &serde_json::Value, key: &str) -> String {
    row.get(key)
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_owned()
}

/// Read a boolean field, defaulting to false.
fn flag(document: &serde_json::Value, key: &str) -> bool {
    document
        .get(key)
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// Render a JSON string without its quotes.
fn string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_clean_status_is_not_an_error() {
        let document = serde_json::json!({"clean": true, "applied": [], "pending": []});
        assert!(finish(&document, "m", "h").is_ok());
    }

    #[test]
    fn a_dirty_ledger_exits_one() {
        let document = serde_json::json!({"clean": false});
        let error = finish(&document, "m", "h").expect_err("a disagreeing ledger fails");
        assert_eq!(error.fault.code(), 1);
    }

    #[test]
    fn a_document_without_a_clean_field_is_not_treated_as_dirty() {
        // `migrate` and `rollback` answer without one, and neither should be
        // turned into a failure by a check written for `status`.
        assert!(finish(&serde_json::json!({"command": "migrate"}), "m", "h").is_ok());
    }

    #[test]
    fn missing_arrays_read_as_empty_rather_than_panicking() {
        let document = serde_json::json!({});
        assert!(array(&document, "applied").is_empty());
        assert!(array(&document, "pending").is_empty());
        // A field of the wrong type is also empty, not a panic: the application
        // owns `src/db.rs` and may have edited the shape.
        let wrong = serde_json::json!({"applied": "not an array"});
        assert!(array(&wrong, "applied").is_empty());
        assert!(!flag(&wrong, "applied"));
        assert!(!flag(&document, "written"));
    }

    #[test]
    fn the_rollback_flags_carry_the_step_count() {
        assert_eq!(
            Db::Rollback(3).flags(),
            vec!["--db-rollback".to_owned(), "3".to_owned()]
        );
        assert_eq!(Db::Status.flags(), vec!["--db-status".to_owned()]);
        assert_eq!(Db::Rollback(2).label(), "--db-rollback 2");
    }

    #[test]
    fn a_make_migration_names_the_migration_before_any_modifier() {
        // `src/db.rs` reads the name as the token right after the command, so a
        // modifier that got in front of it would be taken for the name.
        let flags = Db::MakeMigration {
            name: "add_locale".to_owned(),
            dry_run: true,
            renames: vec!["name:full_name".to_owned()],
            drop_and_add: false,
        }
        .flags();
        assert_eq!(flags[0], "--db-make-migration");
        assert_eq!(flags[1], "add_locale");
        assert!(flags.contains(&"--db-dry-run".to_owned()));
        assert_eq!(
            flags
                .iter()
                .position(|flag| flag == "--db-rename")
                .map(|at| flags[at + 1].clone()),
            Some("name:full_name".to_owned())
        );
        assert!(!flags.contains(&"--db-drop-and-add".to_owned()));
    }

    #[test]
    fn a_seed_name_precedes_force_for_the_same_reason() {
        let flags = Db::Seed {
            name: Some("dev".to_owned()),
            force: true,
        }
        .flags();
        assert_eq!(flags, vec!["--db-seed", "dev", "--db-force"]);
        assert_eq!(
            Db::Seed {
                name: None,
                force: false
            }
            .flags(),
            vec!["--db-seed"]
        );
    }

    #[test]
    fn a_squash_only_writes_when_it_is_told_to() {
        assert_eq!(
            Db::Squash { apply: false }.flags(),
            vec!["--db-squash".to_owned()]
        );
        assert_eq!(
            Db::Squash { apply: true }.flags(),
            vec!["--db-squash".to_owned(), "--db-apply".to_owned()]
        );
    }

    #[test]
    fn every_db_command_has_a_distinct_flag_the_template_implements() {
        let commands = [
            Db::Status,
            Db::Migrate,
            Db::MigrateTenants,
            Db::Rollback(1),
            Db::Redo,
            Db::Check,
            Db::Squash { apply: true },
            Db::Seed {
                name: None,
                force: true,
            },
            Db::MakeMigration {
                name: "x".to_owned(),
                dry_run: true,
                renames: vec!["a:b".to_owned()],
                drop_and_add: true,
            },
        ];

        let mut primaries: Vec<String> = commands
            .iter()
            .map(|command| command.flags().swap_remove(0))
            .collect();
        primaries.sort();
        primaries.dedup();
        assert_eq!(primaries.len(), commands.len(), "two commands share a flag");

        // The other half of the protocol. A flag renamed here and not in the
        // template makes `moso db` hang against every freshly generated project
        // until the hour-long timeout fires, so assert the two agree — for the
        // modifiers as well as the commands.
        let db_rs = crate::template::DB_FILES
            .iter()
            .find(|file| file.path == "src/db.rs")
            .expect("the template ships src/db.rs")
            .contents;
        for command in &commands {
            for flag in command
                .flags()
                .iter()
                .filter(|flag| flag.starts_with("--db-"))
            {
                assert!(
                    db_rs.contains(&format!("\"{flag}\"")),
                    "src/db.rs does not handle `{flag}`"
                );
            }
        }
    }
}
