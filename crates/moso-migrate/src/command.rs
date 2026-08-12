//! The `moso db` command surface: one call per subcommand, one JSON document
//! back.
//!
//! # Why this module exists
//!
//! `moso db` does not link this crate. It builds the application's own binary
//! and runs it with a `--db-*` flag, then reads exactly one JSON document off
//! standard output — because a migration may need application logic
//! ([`RustMigration`](crate::rust_migration::RustMigration)) that only the
//! application can register, and because linking a bundled SQLite into a CLI
//! whose other commands need no database is a dependency budget nobody wants to
//! pay.
//!
//! That protocol has a shape, and this module is it. Everything below is one
//! function per subcommand, taking what the flags carry and returning a
//! `Serialize` report whose field names *are* the JSON keys. The generated
//! `src/db.rs` is then a match arm and a `println!`, and the CLI renders a
//! table without ever becoming the thing that decides what a migration is.
//!
//! ```text
//!   moso db check ──► cargo run -- --db-check ──► command::check(..)
//!         ▲                                            │
//!         └────────── one JSON document ◄──────────────┘
//! ```
//!
//! | Function | Flag | `command` key |
//! | --- | --- | --- |
//! | [`make_migration`] | `--db-make-migration` | `make-migration` |
//! | [`check`] | `--db-check` | `check` |
//! | [`squash`] | `--db-squash` | `squash` |
//! | [`seed`] | `--db-seed` | `seed` |
//! | [`migrate_tenants`] | `--db-migrate-tenants` | `migrate-tenants` |
//!
//! Every report carries `command` so a caller can tell the documents apart, and
//! every report that can fail a CI job carries `clean`, which is the boolean
//! `moso db` turns into an exit code.
//!
//! # What these are not
//!
//! They are not the only way in. [`Generator`], [`Squash`], [`Seeds`],
//! [`check::compare`](crate::check::compare) and [`Runner`] all stay public and
//! do more than these do — a partial squash, a drift check against something
//! other than the entity graph, a generator with a pinned clock. These exist so
//! that the common case is one call with a stable signature, not to hide the
//! layer underneath.
//!
//! The examples that reach a database are `no_run`: they compile on every
//! machine and connect on none, because a doctest that needs a PostgreSQL
//! server would fail on a laptop rather than teach anything. The ones that do
//! run are the offline halves — generation and squashing — which need no server
//! at all.
//!
//! ```no_run
//! use moso_migrate::command::{self, MakeMigrationOptions};
//! use moso_orm::Backend;
//!
//! # fn example(entities: &[&moso_orm::descriptor::EntityDescriptor]) -> moso_migrate::Result<()> {
//! let report = command::make_migration(
//!     "migrations",
//!     Backend::Postgres,
//!     entities,
//!     &MakeMigrationOptions::default(),
//! )?;
//! println!("{}", serde_json::to_string_pretty(&report).expect("a report serialises"));
//! # Ok(())
//! # }
//! ```

use std::path::Path;

use moso_orm::Backend;
use moso_orm::descriptor::EntityDescriptor;
use serde::Serialize;

use crate::advice::Advice;
use crate::check::Drift;
use crate::conn::{self, Connection};
use crate::error::{Error, Result};
use crate::generator::Generator;
use crate::rename::{DropAndAdd, Oracle, Prompt, RefuseToGuess, RenameAnswer, Scripted};
use crate::runner::{MigrateReport, Runner, RunnerOptions};
use crate::schema::Schema;
use crate::seed::{SeedOptions, Seeds};
use crate::squash::Squash;
use crate::version::Version;

// ---------------------------------------------------------------------------
// Shared report pieces
// ---------------------------------------------------------------------------

/// One expand/contract warning, flattened for JSON.
///
/// ```
/// use moso_migrate::advice::Advice;
/// use moso_migrate::command::AdviceReport;
///
/// let report = AdviceReport::from(&Advice::dropping_a_column("users", "legacy_id"));
/// assert!(report.summary().contains("legacy_id"));
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct AdviceReport {
    summary: String,
    plan: String,
}

impl AdviceReport {
    /// The one-line headline.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// # use moso_migrate::command::AdviceReport;
    /// let report = AdviceReport::from(&Advice::renaming("users", "name", "full_name"));
    /// assert!(!report.summary().is_empty());
    /// ```
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// The multi-line plan under it.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// # use moso_migrate::command::AdviceReport;
    /// let report = AdviceReport::from(&Advice::renaming("users", "name", "full_name"));
    /// assert!(!report.plan().is_empty());
    /// ```
    #[must_use]
    pub fn plan(&self) -> &str {
        &self.plan
    }
}

impl From<&Advice> for AdviceReport {
    fn from(advice: &Advice) -> Self {
        Self {
            summary: advice.summary().to_owned(),
            plan: advice.plan().to_owned(),
        }
    }
}

/// One applied migration, flattened for JSON.
///
/// The same three fields `moso db migrate` already renders, so the tenant
/// report and the single-database report print through the same code.
///
/// ```
/// # fn example(entry: &moso_migrate::command::AppliedEntry) {
/// println!("{} {} {}ms", entry.version(), entry.name(), entry.duration_ms());
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct AppliedEntry {
    version: String,
    name: String,
    duration_ms: u64,
}

impl AppliedEntry {
    /// The version, in its canonical 15-character spelling.
    ///
    /// ```
    /// # fn example(entry: &moso_migrate::command::AppliedEntry) {
    /// assert_eq!(entry.version().len(), 15);
    /// # }
    /// ```
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The migration's name.
    ///
    /// ```
    /// # fn example(entry: &moso_migrate::command::AppliedEntry) {
    /// println!("{}", entry.name());
    /// # }
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// How long it took.
    ///
    /// ```
    /// # fn example(entry: &moso_migrate::command::AppliedEntry) {
    /// assert!(entry.duration_ms() < 1_000_000);
    /// # }
    /// ```
    #[must_use]
    pub const fn duration_ms(&self) -> u64 {
        self.duration_ms
    }
}

fn applied_entries(report: &MigrateReport) -> Vec<AppliedEntry> {
    report
        .applied()
        .iter()
        .map(|(version, name, elapsed)| AppliedEntry {
            version: version.to_string(),
            name: name.clone(),
            duration_ms: u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX),
        })
        .collect()
}

// ---------------------------------------------------------------------------
// make-migration
// ---------------------------------------------------------------------------

/// How a rename question is answered when the generator runs without a human.
///
/// A diff cannot tell a rename from a drop-and-add, and the difference is
/// whether the data survives, so somebody has to answer. This is the mapping
/// from what a command line can carry to the oracles in
/// [`rename`](crate::rename).
///
/// ```
/// use moso_migrate::command::RenamePolicy;
///
/// // The CI answer: refuse, and name the flag that would have answered.
/// assert!(matches!(RenamePolicy::default(), RenamePolicy::Refuse));
/// ```
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub enum RenamePolicy {
    /// Ask on the terminal, [`Prompt::stdio`] — which writes to standard error,
    /// so it does not corrupt the one JSON document the `--db-*` protocol
    /// answers with. Errors on a closed input rather than guessing, so it is
    /// safe to pick when there may be no terminal.
    Ask,
    /// Answer from `--rename old:new` pairs, [`Scripted`]. `strict` decides
    /// what an unanswered question does: refuse it, or treat it as a drop and
    /// an add.
    Scripted {
        /// The `old:new` pairs, exactly as they were typed.
        pairs: Vec<String>,
        /// Whether a question the pairs do not cover is an error.
        strict: bool,
    },
    /// Refuse every question and name the flag, [`RefuseToGuess`]. The default,
    /// because a generator that guesses in CI produces a migration nobody
    /// reviewed that either keeps a column's data or does not.
    #[default]
    Refuse,
    /// Everything is a drop and an add, [`DropAndAdd`]. For a first migration
    /// against an empty database, and for tests.
    DropAndAdd,
}

impl RenamePolicy {
    /// Builds the oracle this policy names.
    ///
    /// # Errors
    ///
    /// [`Error::NeedsAnswer`] when a `--rename` pair is not `old:new`.
    fn oracle(&self) -> Result<Box<dyn Oracle>> {
        Ok(match self {
            Self::Ask => Box::new(Prompt::stdio()),
            Self::Scripted { pairs, strict } => {
                // `Scripted::parse` is strict already, so the loose form is the
                // one that has to say what it wants.
                let scripted = Scripted::parse(pairs)?;
                Box::new(scripted.otherwise(if *strict {
                    None
                } else {
                    Some(RenameAnswer::DropAndAdd)
                }))
            }
            Self::Refuse => Box::new(RefuseToGuess),
            Self::DropAndAdd => Box::new(DropAndAdd),
        })
    }
}

/// What `moso db make-migration` was asked to do.
///
/// ```
/// use moso_migrate::command::MakeMigrationOptions;
///
/// let options = MakeMigrationOptions::default().name("add user locale").dry_run();
/// assert!(options.is_dry_run());
/// assert_eq!(options.migration_name(), Some("add user locale"));
/// ```
#[derive(Clone, Debug, Default)]
pub struct MakeMigrationOptions {
    name: Option<String>,
    renames: RenamePolicy,
    dry_run: bool,
}

impl MakeMigrationOptions {
    /// Names the migration; it is slugified. Without one the generator suggests
    /// a name from the diff.
    ///
    /// ```
    /// # use moso_migrate::command::MakeMigrationOptions;
    /// assert_eq!(MakeMigrationOptions::default().name("x").migration_name(), Some("x"));
    /// ```
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// How rename questions are answered.
    ///
    /// ```
    /// # use moso_migrate::command::{MakeMigrationOptions, RenamePolicy};
    /// let options = MakeMigrationOptions::default().renames(RenamePolicy::DropAndAdd);
    /// assert!(matches!(options.rename_policy(), RenamePolicy::DropAndAdd));
    /// ```
    #[must_use]
    pub fn renames(mut self, policy: RenamePolicy) -> Self {
        self.renames = policy;
        self
    }

    /// Builds the migration and writes nothing.
    ///
    /// ```
    /// # use moso_migrate::command::MakeMigrationOptions;
    /// assert!(MakeMigrationOptions::default().dry_run().is_dry_run());
    /// ```
    #[must_use]
    pub const fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// The name, if one was given.
    ///
    /// ```
    /// # use moso_migrate::command::MakeMigrationOptions;
    /// assert_eq!(MakeMigrationOptions::default().migration_name(), None);
    /// ```
    #[must_use]
    pub fn migration_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// The rename policy.
    ///
    /// ```
    /// # use moso_migrate::command::{MakeMigrationOptions, RenamePolicy};
    /// assert!(matches!(MakeMigrationOptions::default().rename_policy(), RenamePolicy::Refuse));
    /// ```
    #[must_use]
    pub const fn rename_policy(&self) -> &RenamePolicy {
        &self.renames
    }

    /// Whether nothing will be written.
    ///
    /// ```
    /// # use moso_migrate::command::MakeMigrationOptions;
    /// assert!(!MakeMigrationOptions::default().is_dry_run());
    /// ```
    #[must_use]
    pub const fn is_dry_run(&self) -> bool {
        self.dry_run
    }
}

/// What `moso db make-migration` produced.
///
/// `changed` is `false` when the entities and the snapshot already agree, which
/// is the answer that makes the generator idempotent — every other field is
/// then empty or `null`.
///
/// ```
/// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
/// if report.has_changes() {
///     println!("wrote {}", report.path().unwrap_or_default());
/// }
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct MakeMigrationReport {
    command: &'static str,
    changed: bool,
    written: bool,
    destructive: bool,
    version: Option<String>,
    name: Option<String>,
    path: Option<String>,
    snapshot_path: Option<String>,
    changes: Vec<String>,
    advice: Vec<AdviceReport>,
    migration: Option<String>,
}

impl MakeMigrationReport {
    /// Always `"make-migration"`.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// assert_eq!(report.command(), "make-migration");
    /// # }
    /// ```
    #[must_use]
    pub const fn command(&self) -> &'static str {
        self.command
    }

    /// Whether there was anything to generate.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// if !report.has_changes() {
    ///     eprintln!("no changes");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub const fn has_changes(&self) -> bool {
        self.changed
    }

    /// Whether the files reached the disk. `false` for a dry run.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// println!("{}", report.was_written());
    /// # }
    /// ```
    #[must_use]
    pub const fn was_written(&self) -> bool {
        self.written
    }

    /// Whether the migration contains a destructive block waiting for a human.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// if report.is_destructive() {
    ///     eprintln!("read the file before committing it");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        self.destructive
    }

    /// The version, when something was generated.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// println!("{:?}", report.version());
    /// # }
    /// ```
    #[must_use]
    pub fn version(&self) -> Option<&str> {
        self.version.as_deref()
    }

    /// The slugified name, when something was generated.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// println!("{:?}", report.name());
    /// # }
    /// ```
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Where the `.sql` went, or would have gone.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// println!("{:?}", report.path());
    /// # }
    /// ```
    #[must_use]
    pub fn path(&self) -> Option<&str> {
        self.path.as_deref()
    }

    /// Where the snapshot went, or would have gone.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// println!("{:?}", report.snapshot_path());
    /// # }
    /// ```
    #[must_use]
    pub fn snapshot_path(&self) -> Option<&str> {
        self.snapshot_path.as_deref()
    }

    /// One line per change, the same lines the file's header carries.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// for line in report.changes() {
    ///     eprintln!("  {line}");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn changes(&self) -> &[String] {
        &self.changes
    }

    /// Expand/contract warnings for changes that break a rolling deploy.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// for advice in report.advice() {
    ///     eprintln!("{}", advice.summary());
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn advice(&self) -> &[AdviceReport] {
        &self.advice
    }

    /// The migration's text, which is what a dry run prints.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::MakeMigrationReport) {
    /// if let Some(sql) = report.migration() {
    ///     println!("{sql}");
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn migration(&self) -> Option<&str> {
        self.migration.as_deref()
    }
}

/// Diffs `entities` against the committed snapshot and writes a migration.
///
/// This is the whole of `moso db make-migration`: read `.schema.json`, build
/// the desired schema from the descriptors, diff, plan for `backend`, write the
/// `.sql` and the new snapshot. Nothing touches a database, so it works offline
/// and produces the same bytes on every machine.
///
/// The descriptor list is the caller's to assemble. There is no link-time
/// registry (ADR-0004), so nothing walks a crate looking for
/// `#[derive(Entity)]` types — and an entity left off the list looks exactly
/// like a table you want dropped.
///
/// # Errors
///
/// [`Error::NeedsAnswer`] when a rename question has no answer under the
/// [`RenamePolicy`], [`Error::Unsupported`] when a change cannot be expressed on
/// `backend`, [`Error::Snapshot`] when `.schema.json` is not one this build
/// reads, and [`Error::Io`] when the files cannot be written.
///
/// ```
/// use moso_migrate::command::{make_migration, MakeMigrationOptions};
/// use moso_orm::Backend;
///
/// // No entities and no snapshot: nothing to do, and nothing written.
/// let report = make_migration(
///     "does-not-exist",
///     Backend::Postgres,
///     &[],
///     &MakeMigrationOptions::default(),
/// )?;
/// assert!(!report.has_changes());
/// assert_eq!(report.command(), "make-migration");
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn make_migration(
    directory: impl AsRef<Path>,
    backend: Backend,
    entities: &[&EntityDescriptor],
    options: &MakeMigrationOptions,
) -> Result<MakeMigrationReport> {
    let generator = Generator::new(directory.as_ref().to_path_buf(), backend);
    let oracle = options.renames.oracle()?;
    let Some(generated) =
        generator.make_migration(entities, options.migration_name(), oracle.as_ref())?
    else {
        return Ok(MakeMigrationReport {
            command: "make-migration",
            changed: false,
            written: false,
            destructive: false,
            version: None,
            name: None,
            path: None,
            snapshot_path: None,
            changes: Vec::new(),
            advice: Vec::new(),
            migration: None,
        });
    };

    if !options.is_dry_run() {
        generated.write()?;
    }
    Ok(MakeMigrationReport {
        command: "make-migration",
        changed: true,
        written: !options.is_dry_run(),
        destructive: generated.diff().is_destructive(),
        version: Some(generated.id().version().to_string()),
        name: Some(generated.id().name().to_owned()),
        path: Some(generated.path().display().to_string()),
        snapshot_path: Some(generated.snapshot_path().display().to_string()),
        changes: generated.diff().summary(),
        advice: generated.advice().iter().map(AdviceReport::from).collect(),
        migration: Some(generated.migration().to_owned()),
    })
}

// ---------------------------------------------------------------------------
// check
// ---------------------------------------------------------------------------

/// What `moso db check` found.
///
/// `clean` is the exit code: drift in either direction fails, and a pending
/// migration on its own does not, because a pending migration is the *fix* for
/// missing-in-database rather than a second problem.
///
/// ```
/// # fn example(report: &moso_migrate::command::CheckReport) {
/// if !report.is_clean() {
///     eprintln!("{}", report.report());
/// }
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct CheckReport {
    command: &'static str,
    clean: bool,
    missing_in_database: Vec<String>,
    extra_in_database: Vec<String>,
    mismatched: Vec<String>,
    pending: Vec<String>,
    report: String,
}

impl CheckReport {
    /// Always `"check"`.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// assert_eq!(report.command(), "check");
    /// # }
    /// ```
    #[must_use]
    pub const fn command(&self) -> &'static str {
        self.command
    }

    /// Whether the database matches. Pending migrations do not make it false.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// println!("{}", report.is_clean());
    /// # }
    /// ```
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.clean
    }

    /// What the entities describe and the database does not have.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// println!("{:?}", report.missing_in_database());
    /// # }
    /// ```
    #[must_use]
    pub fn missing_in_database(&self) -> &[String] {
        &self.missing_in_database
    }

    /// What the database has and no entity describes.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// println!("{:?}", report.extra_in_database());
    /// # }
    /// ```
    #[must_use]
    pub fn extra_in_database(&self) -> &[String] {
        &self.extra_in_database
    }

    /// What both have, differently.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// println!("{:?}", report.mismatched());
    /// # }
    /// ```
    #[must_use]
    pub fn mismatched(&self) -> &[String] {
        &self.mismatched
    }

    /// Migrations on disk that have not been applied.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// println!("{:?}", report.pending());
    /// # }
    /// ```
    #[must_use]
    pub fn pending(&self) -> &[String] {
        &self.pending
    }

    /// The printable report, which is [`Drift`]'s own `Display`.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::CheckReport) {
    /// eprintln!("{}", report.report());
    /// # }
    /// ```
    #[must_use]
    pub fn report(&self) -> &str {
        &self.report
    }
}

fn check_report(drift: &Drift) -> CheckReport {
    CheckReport {
        command: "check",
        clean: drift.is_empty(),
        missing_in_database: drift.missing_in_database().to_vec(),
        extra_in_database: drift.extra_in_database().to_vec(),
        mismatched: drift.mismatched().to_vec(),
        pending: drift.pending().iter().map(ToString::to_string).collect(),
        report: drift.to_string(),
    }
}

/// Reads the live database back and compares it with `entities`, in both
/// directions.
///
/// This is the whole of `moso db check`: open the database, list the pending
/// migrations, introspect the live schema, and diff it against what the
/// entities describe. Missing-in-database catches an unapplied migration;
/// extra-in-database catches somebody's `psql` session. One direction alone
/// would be half a check.
///
/// To compare against something other than the entity graph — the committed
/// snapshot, a hand-built [`Schema`] — use
/// [`check::compare`](crate::check::compare) directly.
///
/// # Errors
///
/// [`Error::Database`] when the database cannot be reached or its catalogue
/// cannot be read, [`Error::Io`] when the migrations directory cannot be listed,
/// and whatever [`Schema::from_entities`] refuses.
///
/// ```no_run
/// # async fn example(entities: &[&moso_orm::descriptor::EntityDescriptor])
/// # -> moso_migrate::Result<()> {
/// let report = moso_migrate::command::check(
///     "migrations",
///     "postgres://moso:moso@localhost/app",
///     entities,
/// )
/// .await?;
/// assert!(report.is_clean(), "{}", report.report());
/// # Ok(())
/// # }
/// ```
pub async fn check(
    directory: impl AsRef<Path>,
    url: &str,
    entities: &[&EntityDescriptor],
) -> Result<CheckReport> {
    let expected = Schema::from_entities(entities.iter().copied())?;
    let mut runner = Runner::open(directory, url).await?;
    let outcome = async {
        let pending = runner.status().await?.pending().to_vec();
        crate::check::check(runner.connection(), &expected, pending).await
    }
    .await;
    runner.close().await;
    Ok(check_report(&outcome?))
}

// ---------------------------------------------------------------------------
// squash
// ---------------------------------------------------------------------------

/// What `moso db squash` was asked to do.
///
/// ```
/// use moso_migrate::command::SquashOptions;
///
/// assert!(!SquashOptions::default().will_apply(), "a squash is a dry run by default");
/// ```
#[derive(Clone, Debug, Default)]
pub struct SquashOptions {
    apply: bool,
    at: Option<Version>,
}

impl SquashOptions {
    /// Writes the baseline and deletes the files it replaces.
    ///
    /// Off by default. A squash rewrites version-controlled history, and
    /// deleting files during what might have been a look is unforgivable.
    ///
    /// ```
    /// # use moso_migrate::command::SquashOptions;
    /// assert!(SquashOptions::default().apply().will_apply());
    /// ```
    #[must_use]
    pub const fn apply(mut self) -> Self {
        self.apply = true;
        self
    }

    /// Fixes the version the baseline gets when the directory is empty, which
    /// is what makes the output testable byte for byte.
    ///
    /// ```
    /// # use moso_migrate::command::SquashOptions;
    /// # use moso_migrate::Version;
    /// let options = SquashOptions::default().at(Version::from_parts(2026, 1, 1, 0, 0, 0));
    /// assert!(options.clock().is_some());
    /// ```
    #[must_use]
    pub const fn at(mut self, version: Version) -> Self {
        self.at = Some(version);
        self
    }

    /// Whether the files will be written.
    ///
    /// ```
    /// # use moso_migrate::command::SquashOptions;
    /// assert!(!SquashOptions::default().will_apply());
    /// ```
    #[must_use]
    pub const fn will_apply(&self) -> bool {
        self.apply
    }

    /// The pinned version, if one was set.
    ///
    /// ```
    /// # use moso_migrate::command::SquashOptions;
    /// assert!(SquashOptions::default().clock().is_none());
    /// ```
    #[must_use]
    pub const fn clock(&self) -> Option<Version> {
        self.at
    }
}

/// What `moso db squash` produced.
///
/// ```
/// # fn example(report: &moso_migrate::command::SquashReport) {
/// println!("{} files collapse into {}", report.replaced().len(), report.path());
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct SquashReport {
    command: &'static str,
    written: bool,
    version: String,
    name: String,
    path: String,
    replaced: Vec<String>,
    removable: Vec<String>,
    migration: String,
}

impl SquashReport {
    /// Always `"squash"`.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// assert_eq!(report.command(), "squash");
    /// # }
    /// ```
    #[must_use]
    pub const fn command(&self) -> &'static str {
        self.command
    }

    /// Whether the baseline was written and the collapsed files deleted.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// println!("{}", report.was_written());
    /// # }
    /// ```
    #[must_use]
    pub const fn was_written(&self) -> bool {
        self.written
    }

    /// The baseline's version, which is the oldest version it replaces.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// println!("{}", report.version());
    /// # }
    /// ```
    #[must_use]
    pub fn version(&self) -> &str {
        &self.version
    }

    /// The baseline's name.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// println!("{}", report.name());
    /// # }
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the baseline goes.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// println!("{}", report.path());
    /// # }
    /// ```
    #[must_use]
    pub fn path(&self) -> &str {
        &self.path
    }

    /// The versions the baseline stands in for.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// println!("{:?}", report.replaced());
    /// # }
    /// ```
    #[must_use]
    pub fn replaced(&self) -> &[String] {
        &self.replaced
    }

    /// The files that are now redundant.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// println!("{:?}", report.removable());
    /// # }
    /// ```
    #[must_use]
    pub fn removable(&self) -> &[String] {
        &self.removable
    }

    /// The baseline's text.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SquashReport) {
    /// assert!(report.migration().contains("-- moso:replaces"));
    /// # }
    /// ```
    #[must_use]
    pub fn migration(&self) -> &str {
        &self.migration
    }
}

/// Collapses every migration in `directory` into one baseline that produces the
/// schema `entities` describe.
///
/// This is the whole of `moso db squash`. The baseline carries
/// `-- moso:replaces`, and the runner records it as applied *without running
/// it* when every version it replaces is already in the ledger, which is the
/// only mechanism that works for a team where some databases are old and some
/// are new.
///
/// It squashes **everything**, deliberately. A partial squash needs the schema
/// as of the cut-off rather than the current one — a baseline built from
/// today's entities followed by migrations that add today's columns fails on a
/// fresh database — and nothing here can know that schema. Callers who do know
/// it have [`Squash::over_directory`].
///
/// # Errors
///
/// [`Error::Io`] when the directory cannot be read or written,
/// [`Error::Unsupported`] when the schema cannot be created on `backend`, and
/// whatever [`Schema::from_entities`] refuses.
///
/// ```
/// use moso_migrate::command::{squash, SquashOptions};
/// use moso_migrate::Version;
/// use moso_orm::Backend;
///
/// // An empty directory and no entities: a baseline that replaces nothing.
/// let report = squash(
///     "does-not-exist",
///     Backend::Postgres,
///     &[],
///     &SquashOptions::default().at(Version::from_parts(2026, 1, 1, 0, 0, 0)),
/// )?;
/// assert!(report.replaced().is_empty());
/// assert!(!report.was_written());
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn squash(
    directory: impl AsRef<Path>,
    backend: Backend,
    entities: &[&EntityDescriptor],
    options: &SquashOptions,
) -> Result<SquashReport> {
    let directory = directory.as_ref();
    let schema = Schema::from_entities(entities.iter().copied())?;
    let at = options.clock().unwrap_or_else(Version::now);
    // `before` is exclusive, and every file on disk has a version in the past,
    // so `at` collapses all of them.
    let squash = Squash::over_directory(directory, at, &schema, backend, at)?;

    if options.will_apply() {
        squash.apply(directory)?;
    }
    Ok(SquashReport {
        command: "squash",
        written: options.will_apply(),
        version: squash.id().version().to_string(),
        name: squash.id().name().to_owned(),
        path: directory
            .join(squash.id().file_name("sql"))
            .display()
            .to_string(),
        replaced: squash.replaced().iter().map(ToString::to_string).collect(),
        removable: squash
            .removable()
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
        migration: squash.migration().to_owned(),
    })
}

// ---------------------------------------------------------------------------
// seed
// ---------------------------------------------------------------------------

/// What `moso db seed` did.
///
/// ```
/// # fn example(report: &moso_migrate::command::SeedReport) {
/// println!("ran {:?} of {:?}", report.ran(), report.available());
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct SeedReport {
    command: &'static str,
    ran: Vec<String>,
    available: Vec<String>,
    profile: String,
    forced: bool,
}

impl SeedReport {
    /// Always `"seed"`.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SeedReport) {
    /// assert_eq!(report.command(), "seed");
    /// # }
    /// ```
    #[must_use]
    pub const fn command(&self) -> &'static str {
        self.command
    }

    /// The seeds that ran, in order.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SeedReport) {
    /// println!("{:?}", report.ran());
    /// # }
    /// ```
    #[must_use]
    pub fn ran(&self) -> &[String] {
        &self.ran
    }

    /// Every registered seed's name, so a wrong `--name` is answerable.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SeedReport) {
    /// println!("{:?}", report.available());
    /// # }
    /// ```
    #[must_use]
    pub fn available(&self) -> &[String] {
        &self.available
    }

    /// The profile the run was made under.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SeedReport) {
    /// println!("{}", report.profile());
    /// # }
    /// ```
    #[must_use]
    pub fn profile(&self) -> &str {
        &self.profile
    }

    /// Whether the production refusal was overridden.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::SeedReport) {
    /// assert!(!report.was_forced() || report.profile() != "dev");
    /// # }
    /// ```
    #[must_use]
    pub const fn was_forced(&self) -> bool {
        self.forced
    }
}

/// Runs one registered seed by name, or every one when `name` is `None`.
///
/// This is the whole of `moso db seed`: open the database, run the seeds,
/// close. A seed is not a migration — not versioned, not recorded, meant to be
/// run again — and idempotence is the seed author's job, which is why the ones
/// that ship in the templates say `ON CONFLICT DO NOTHING`.
///
/// # Errors
///
/// [`Error::RefusedInProduction`] when the profile is production and neither
/// [`SeedOptions::force`] nor the seed's own `is_safe_in_production` allows it,
/// [`Error::NeedsAnswer`] when `name` matches nothing — with the registered
/// names in the help line — and whatever the seed's own body returns.
///
/// ```no_run
/// use moso_migrate::seed::{SeedOptions, Seeds};
///
/// # async fn example(seeds: &Seeds) -> moso_migrate::Result<()> {
/// let report = moso_migrate::command::seed(
///     "sqlite://app.db",
///     seeds,
///     Some("dev"),
///     &SeedOptions::default(),
/// )
/// .await?;
/// println!("{:?}", report.ran());
/// # Ok(())
/// # }
/// ```
pub async fn seed(
    url: &str,
    seeds: &Seeds,
    name: Option<&str>,
    options: &SeedOptions,
) -> Result<SeedReport> {
    let mut connection = Connection::open(url).await?;
    let outcome = seeds.run(&mut connection, name, options).await;
    connection.close().await;
    Ok(SeedReport {
        command: "seed",
        ran: outcome?,
        available: seeds.names().into_iter().map(ToOwned::to_owned).collect(),
        profile: options.active_profile().to_owned(),
        forced: options.is_forced(),
    })
}

// ---------------------------------------------------------------------------
// migrate --all-tenants
// ---------------------------------------------------------------------------

/// One tenant's database, for [`migrate_tenants`].
///
/// The two routed tenancy models differ only in where the isolation lives, so
/// they differ only in how a target is built: a whole database each, or a named
/// schema each inside one database.
///
/// ```
/// use moso_migrate::command::TenantTarget;
///
/// let per_database = TenantTarget::database("acme", "postgres://localhost/acme");
/// assert_eq!(per_database.schema_name(), None);
///
/// let per_schema = TenantTarget::schema("acme", "postgres://localhost/app", "tenant_7");
/// assert_eq!(per_schema.schema_name(), Some("tenant_7"));
/// ```
#[derive(Clone, Debug)]
pub struct TenantTarget {
    tenant: String,
    url: String,
    schema: Option<String>,
}

impl TenantTarget {
    /// A tenant with a database of its own, `TenancyModel::DatabasePerTenant`.
    ///
    /// ```
    /// # use moso_migrate::command::TenantTarget;
    /// let target = TenantTarget::database("acme", "postgres://localhost/acme");
    /// assert_eq!(target.tenant(), "acme");
    /// ```
    #[must_use]
    pub fn database(tenant: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            tenant: tenant.into(),
            url: url.into(),
            schema: None,
        }
    }

    /// A tenant with a schema of its own, `TenancyModel::SchemaPerTenant`.
    ///
    /// The schema is created if it is missing, because "migrate this tenant" is
    /// exactly what a tenant that has never been migrated needs, and a run
    /// against a schema that does not exist fails with a message about the
    /// search path rather than about the tenant.
    ///
    /// ```
    /// # use moso_migrate::command::TenantTarget;
    /// let target = TenantTarget::schema("acme", "postgres://localhost/app", "tenant_7");
    /// assert_eq!(target.schema_name(), Some("tenant_7"));
    /// ```
    #[must_use]
    pub fn schema(
        tenant: impl Into<String>,
        url: impl Into<String>,
        schema: impl Into<String>,
    ) -> Self {
        Self {
            tenant: tenant.into(),
            url: url.into(),
            schema: Some(schema.into()),
        }
    }

    /// The tenant key, which is what the report is keyed by.
    ///
    /// ```
    /// # use moso_migrate::command::TenantTarget;
    /// assert_eq!(TenantTarget::database("acme", "sqlite://a.db").tenant(), "acme");
    /// ```
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The database URL, as it was given.
    ///
    /// ```
    /// # use moso_migrate::command::TenantTarget;
    /// let target = TenantTarget::database("acme", "sqlite://acme.db");
    /// assert_eq!(target.url(), "sqlite://acme.db");
    /// ```
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The schema to migrate inside that database, when there is one.
    ///
    /// ```
    /// # use moso_migrate::command::TenantTarget;
    /// assert_eq!(TenantTarget::database("a", "sqlite://a.db").schema_name(), None);
    /// ```
    #[must_use]
    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_deref()
    }
}

/// What migrating one tenant did.
///
/// A failure is a field rather than a returned error: under a routed model the
/// tenants are independent, so one that fails says nothing about the next, and
/// an operator needs the whole picture rather than the first line of it.
///
/// ```
/// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
/// if let Some(failure) = outcome.failure() {
///     eprintln!("{}: {failure}", outcome.tenant());
/// }
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct TenantOutcome {
    tenant: String,
    url: String,
    schema: Option<String>,
    applied: Vec<AppliedEntry>,
    up_to_date: bool,
    waited_ms: u64,
    failure: Option<String>,
}

impl TenantOutcome {
    /// The tenant key.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// println!("{}", outcome.tenant());
    /// # }
    /// ```
    #[must_use]
    pub fn tenant(&self) -> &str {
        &self.tenant
    }

    /// The database it ran against, with the password removed.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// assert!(!outcome.url().contains("hunter2"));
    /// # }
    /// ```
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The schema it ran in, under schema-per-tenant.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// println!("{:?}", outcome.schema_name());
    /// # }
    /// ```
    #[must_use]
    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// The migrations that ran for this tenant.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// println!("{}", outcome.applied().len());
    /// # }
    /// ```
    #[must_use]
    pub fn applied(&self) -> &[AppliedEntry] {
        &self.applied
    }

    /// Whether this tenant had nothing pending.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// println!("{}", outcome.is_up_to_date());
    /// # }
    /// ```
    #[must_use]
    pub const fn is_up_to_date(&self) -> bool {
        self.up_to_date
    }

    /// How long this tenant waited for the migration lock.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// println!("{}ms", outcome.waited_ms());
    /// # }
    /// ```
    #[must_use]
    pub const fn waited_ms(&self) -> u64 {
        self.waited_ms
    }

    /// What went wrong, when something did.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// assert_eq!(outcome.failure().is_none(), outcome.is_ok());
    /// # }
    /// ```
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// Whether this tenant is migrated.
    ///
    /// ```
    /// # fn example(outcome: &moso_migrate::command::TenantOutcome) {
    /// println!("{}", outcome.is_ok());
    /// # }
    /// ```
    #[must_use]
    pub const fn is_ok(&self) -> bool {
        self.failure.is_none()
    }
}

/// What `moso db migrate --all-tenants` did, tenant by tenant.
///
/// ```
/// # fn example(report: &moso_migrate::command::TenantMigrateReport) {
/// if !report.is_clean() {
///     for outcome in report.failures() {
///         eprintln!("{} is behind", outcome.tenant());
///     }
/// }
/// # }
/// ```
#[derive(Clone, Debug, Serialize)]
#[non_exhaustive]
pub struct TenantMigrateReport {
    command: &'static str,
    clean: bool,
    tenants: Vec<TenantOutcome>,
}

impl TenantMigrateReport {
    /// Always `"migrate-tenants"`.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::TenantMigrateReport) {
    /// assert_eq!(report.command(), "migrate-tenants");
    /// # }
    /// ```
    #[must_use]
    pub const fn command(&self) -> &'static str {
        self.command
    }

    /// Whether every tenant is migrated.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::TenantMigrateReport) {
    /// println!("{}", report.is_clean());
    /// # }
    /// ```
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.clean
    }

    /// Every tenant, in the order they were given.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::TenantMigrateReport) {
    /// println!("{}", report.tenants().len());
    /// # }
    /// ```
    #[must_use]
    pub fn tenants(&self) -> &[TenantOutcome] {
        &self.tenants
    }

    /// Only the tenants that failed.
    ///
    /// ```
    /// # fn example(report: &moso_migrate::command::TenantMigrateReport) {
    /// assert_eq!(report.failures().is_empty(), report.is_clean());
    /// # }
    /// ```
    #[must_use]
    pub fn failures(&self) -> Vec<&TenantOutcome> {
        self.tenants
            .iter()
            .filter(|outcome| !outcome.is_ok())
            .collect()
    }
}

/// Applies every pending migration to every tenant.
///
/// This is `moso db migrate --all-tenants`. Under
/// `TenancyModel::DatabasePerTenant` each target is a whole database; under
/// `TenancyModel::SchemaPerTenant` each is a named schema inside one, and the
/// runner sets `search_path` to it so the ledger, the tables and the lock all
/// land in the tenant's own schema. The tenant list is the caller's: Moso does
/// not know where you keep it.
///
/// `register` is handed each tenant's runner before it migrates, and is where
/// [`Runner::register`] goes for a migration written in Rust. Pass `&|_| {}`
/// when there are none — a run that silently skipped them would be a schema
/// that is *almost* migrated.
///
/// A tenant that fails is recorded in the report and the run continues, because
/// tenants are independent and a deploy needs to know about all of them, not
/// the first. [`TenantMigrateReport::is_clean`] is the exit code.
///
/// # Errors
///
/// [`Error::RefusedInProduction`] when `allow_destructive` is set against a
/// production profile — checked once, before any tenant is opened. Per-tenant
/// failures are values in the report rather than errors.
///
/// ```no_run
/// use moso_migrate::command::{migrate_tenants, TenantTarget};
/// use moso_migrate::RunnerOptions;
///
/// # async fn example() -> moso_migrate::Result<()> {
/// let tenants = [
///     TenantTarget::schema("acme", "postgres://localhost/app", "tenant_7"),
///     TenantTarget::schema("globex", "postgres://localhost/app", "tenant_8"),
/// ];
/// let report = migrate_tenants(
///     "migrations",
///     &tenants,
///     &RunnerOptions::default(),
///     &|_runner| {},
/// )
/// .await?;
/// assert!(report.is_clean());
/// # Ok(())
/// # }
/// ```
pub async fn migrate_tenants(
    directory: impl AsRef<Path>,
    tenants: &[TenantTarget],
    options: &RunnerOptions,
    register: &dyn Fn(&mut Runner),
) -> Result<TenantMigrateReport> {
    // Refused once, up front: a flag production will not accept must not have
    // already migrated three tenants by the time it is noticed.
    if options.is_production() && options.allows_destructive() {
        return Err(Error::RefusedInProduction {
            command: "moso db migrate --all-tenants --allow-destructive",
            profile: options.active_profile().to_owned(),
            help: "in production the acknowledgement is the committed diff that uncomments the \
                   `-- +migrate destructive` block, not a flag typed at a shell — uncomment it, \
                   commit it, deploy it",
        });
    }

    let directory = directory.as_ref();
    let mut outcomes = Vec::with_capacity(tenants.len());
    for target in tenants {
        let outcome = migrate_one_tenant(directory, target, options, register).await;
        outcomes.push(match outcome {
            Ok(report) => TenantOutcome {
                tenant: target.tenant.clone(),
                url: conn::redact(&target.url),
                schema: target.schema.clone(),
                applied: applied_entries(&report),
                up_to_date: report.is_up_to_date(),
                waited_ms: u64::try_from(report.waited().as_millis()).unwrap_or(u64::MAX),
                failure: None,
            },
            Err(error) => TenantOutcome {
                tenant: target.tenant.clone(),
                url: conn::redact(&target.url),
                schema: target.schema.clone(),
                applied: Vec::new(),
                up_to_date: false,
                waited_ms: 0,
                failure: Some(error.to_string()),
            },
        });
    }

    Ok(TenantMigrateReport {
        command: "migrate-tenants",
        clean: outcomes.iter().all(TenantOutcome::is_ok),
        tenants: outcomes,
    })
}

async fn migrate_one_tenant(
    directory: &Path,
    target: &TenantTarget,
    options: &RunnerOptions,
    register: &dyn Fn(&mut Runner),
) -> Result<MigrateReport> {
    let mut connection = Connection::open(&target.url).await?;
    if let Some(schema) = target.schema_name()
        && let Err(error) = enter_schema(&mut connection, schema).await
    {
        connection.close().await;
        return Err(error);
    }

    let mut runner = Runner::with_connection(directory, connection)?;
    register(&mut runner);
    let report = runner.migrate(options).await;
    runner.close().await;
    report
}

/// Creates the tenant's schema if it is missing and points the session at it.
///
/// The name reaches SQL only as a validated, quoted identifier — an
/// [`Ident`](moso_sql::Ident) that refuses a quote or a control character, and
/// a qualified name is refused outright because a schema is one name. That is
/// the same promise the tenancy router makes, and it is the whole defence
/// against a tenant key chosen by an attacker.
async fn enter_schema(connection: &mut Connection, schema: &str) -> Result<()> {
    if connection.backend() != Backend::Postgres {
        return Err(Error::Unsupported {
            backend: connection.backend().as_str(),
            operation: format!("migrate the tenant schema `{schema}`"),
            help: "SQLite has no schemas; give each tenant a database file of its own and build \
                   the target with `TenantTarget::database`"
                .to_owned(),
        });
    }
    if schema.contains('.') {
        return Err(Error::Unsupported {
            backend: connection.backend().as_str(),
            operation: format!("migrate the tenant schema `{schema}`"),
            help: "a schema is one identifier, so a qualified name cannot be one; check the \
                   tenant key and the prefix it is built from"
                .to_owned(),
        });
    }
    let quoted = crate::emit::quote_name(moso_sql::Ident::new(schema)?.as_str());
    connection
        .execute(&format!("CREATE SCHEMA IF NOT EXISTS {quoted}"))
        .await?;
    connection
        .execute(&format!("SET search_path TO {quoted}, public"))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── fixtures ────────────────────────────────────────────────────────────

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "moso-command-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|since| since.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&base).expect("creates");
        base
    }

    fn users() -> Schema {
        use crate::schema::{Column, Table};
        use moso_sql::DataType;

        let mut table = Table::new("users").for_entity("User");
        table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        table.add_column(Column::new("email", DataType::Text).for_field("email"));
        table.set_primary_key(["id"]);
        let mut schema = Schema::empty();
        schema.add_table(table);
        schema
    }

    /// Writes a snapshot into `directory`, which is how a test gets a `before`
    /// without going through an entity graph.
    fn snapshot(directory: &Path, schema: &Schema) {
        std::fs::write(
            directory.join(crate::generator::SNAPSHOT_FILE),
            schema.to_json(),
        )
        .expect("writes");
    }

    // ── make-migration ──────────────────────────────────────────────────────

    #[test]
    fn make_migration_answers_no_changes_without_writing_anything() {
        let dir = temp_dir("no-changes");
        snapshot(&dir, &Schema::empty());
        let report = make_migration(
            &dir,
            Backend::Postgres,
            &[],
            &MakeMigrationOptions::default(),
        )
        .expect("diffs");
        assert!(!report.has_changes());
        assert!(!report.was_written());
        assert_eq!(report.command(), "make-migration");
        assert!(report.migration().is_none());
    }

    #[test]
    fn make_migration_reports_a_drop_as_destructive_and_writes_two_files() {
        let dir = temp_dir("drop");
        // The snapshot has a table; no entities means it is being dropped.
        snapshot(&dir, &users());
        let report = make_migration(
            &dir,
            Backend::Postgres,
            &[],
            &MakeMigrationOptions::default().renames(RenamePolicy::DropAndAdd),
        )
        .expect("diffs");

        assert!(report.has_changes());
        assert!(report.is_destructive());
        assert!(report.was_written());
        assert_eq!(report.changes(), ["drop the table `users`"]);
        assert!(std::path::Path::new(report.path().expect("a path")).is_file());
        assert!(std::path::Path::new(report.snapshot_path().expect("a path")).is_file());
        assert!(
            report
                .migration()
                .expect("the text")
                .contains("-- moso:destructive")
        );
    }

    #[test]
    fn a_dry_run_builds_the_migration_and_touches_nothing() {
        let dir = temp_dir("dry");
        snapshot(&dir, &users());
        let report = make_migration(
            &dir,
            Backend::Postgres,
            &[],
            &MakeMigrationOptions::default()
                .renames(RenamePolicy::DropAndAdd)
                .dry_run(),
        )
        .expect("diffs");

        assert!(report.has_changes());
        assert!(!report.was_written());
        assert!(!std::path::Path::new(report.path().expect("a path")).exists());
    }

    #[test]
    fn the_report_serialises_with_the_keys_the_protocol_names() {
        let dir = temp_dir("json");
        snapshot(&dir, &Schema::empty());
        let report = make_migration(
            &dir,
            Backend::Postgres,
            &[],
            &MakeMigrationOptions::default(),
        )
        .expect("diffs");

        let document: serde_json::Value =
            serde_json::to_value(&report).expect("a report serialises");
        for key in [
            "command",
            "changed",
            "written",
            "destructive",
            "version",
            "name",
            "path",
            "snapshot_path",
            "changes",
            "advice",
            "migration",
        ] {
            assert!(document.get(key).is_some(), "missing `{key}`: {document}");
        }
        assert_eq!(document["command"], "make-migration");
    }

    #[test]
    fn the_default_rename_policy_refuses_rather_than_guessing() {
        // Renaming a table and dropping it produce the same diff, and in CI the
        // right answer is to stop and name the flag.
        let dir = temp_dir("rename");
        snapshot(&dir, &users());

        let source = users();
        let table = source.table("users").expect("the table");
        let mut moved = crate::schema::Table::new("accounts").for_entity("User");
        for column in table.columns() {
            moved.add_column(column.clone());
        }
        moved.set_primary_key(table.primary_key().to_vec());
        let mut renamed = Schema::empty();
        renamed.add_table(moved);

        let generator = Generator::new(dir.clone(), Backend::Postgres);
        let before = generator.read_snapshot().expect("reads");
        let error = generator
            .make_migration_between(&before, &renamed, None, &RefuseToGuess)
            .expect_err("needs an answer");
        assert!(error.to_string().contains("--rename"), "{error}");

        // And the scripted policy answers it.
        let policy = RenamePolicy::Scripted {
            pairs: vec!["users:accounts".to_owned()],
            strict: true,
        };
        let oracle = policy.oracle().expect("parses");
        let generated = generator
            .make_migration_between(&before, &renamed, None, oracle.as_ref())
            .expect("diffs")
            .expect("a migration");
        assert!(generated.migration().contains("RENAME TO \"accounts\""));
    }

    #[test]
    fn a_malformed_rename_pair_is_refused_with_the_shape_it_wanted() {
        let policy = RenamePolicy::Scripted {
            pairs: vec!["no-colon-here".to_owned()],
            strict: false,
        };
        let Err(error) = policy.oracle() else {
            panic!("`no-colon-here` is not an `old:new` pair");
        };
        assert!(
            error.to_string().contains("--rename users.name:full_name"),
            "{error}"
        );
    }

    #[test]
    fn a_loose_scripted_policy_answers_an_unlisted_question_and_a_strict_one_does_not() {
        use crate::rename::RenameQuestion;

        let question = RenameQuestion::table("orders", "purchases");
        let loose = RenamePolicy::Scripted {
            pairs: vec!["users:accounts".to_owned()],
            strict: false,
        };
        assert_eq!(
            loose
                .oracle()
                .expect("parses")
                .answer(&question)
                .expect("answers"),
            RenameAnswer::DropAndAdd
        );

        let strict = RenamePolicy::Scripted {
            pairs: vec!["users:accounts".to_owned()],
            strict: true,
        };
        assert!(strict.oracle().expect("parses").answer(&question).is_err());
    }

    // ── squash ──────────────────────────────────────────────────────────────

    #[test]
    fn squash_collapses_every_file_and_writes_only_when_asked() {
        let dir = temp_dir("squash");
        for name in ["20260101T000000_a.sql", "20260201T000000_b.sql"] {
            std::fs::write(dir.join(name), "-- +migrate up\nSELECT 1;\n").expect("writes");
        }
        snapshot(&dir, &users());

        let generator = Generator::new(dir.clone(), Backend::Postgres);
        let schema = generator.read_snapshot().expect("reads");
        let squashed = Squash::over_directory(
            &dir,
            Version::from_parts(2026, 3, 1, 0, 0, 0),
            &schema,
            Backend::Postgres,
            Version::from_parts(2026, 3, 1, 0, 0, 0),
        )
        .expect("plans");
        assert_eq!(squashed.replaced().len(), 2);

        // Through the entry point, with no entities: the baseline is empty but
        // it still replaces both files, and it does not write by default.
        let report =
            squash(&dir, Backend::Postgres, &[], &SquashOptions::default()).expect("plans");
        assert_eq!(report.replaced().len(), 2);
        assert_eq!(report.removable().len(), 2);
        assert!(!report.was_written());
        assert_eq!(crate::runner::read_directory(&dir).expect("reads").len(), 2);

        let applied = squash(
            &dir,
            Backend::Postgres,
            &[],
            &SquashOptions::default().apply(),
        )
        .expect("applies");
        assert!(applied.was_written());
        let remaining = crate::runner::read_directory(&dir).expect("reads");
        assert_eq!(remaining.len(), 1, "one baseline is left");
        assert!(!remaining[0].replaces().is_empty());
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    // ── seed ────────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn seed_runs_the_named_seed_and_lists_the_rest() {
        use crate::rust_migration::Migrator;
        use crate::seed::Seed;
        use futures_util::future::BoxFuture;

        struct Dev;
        impl Seed for Dev {
            fn name(&self) -> &str {
                "dev"
            }
            fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    migrator
                        .execute("CREATE TABLE IF NOT EXISTS seeded (id integer)")
                        .await?;
                    Ok(())
                })
            }
        }
        struct Demo;
        impl Seed for Demo {
            fn name(&self) -> &str {
                "demo"
            }
            fn run<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }

        let mut seeds = Seeds::default();
        seeds.add(Dev);
        seeds.add(Demo);

        let report = seed(
            "sqlite::memory:",
            &seeds,
            Some("dev"),
            &SeedOptions::default(),
        )
        .await
        .expect("runs");
        assert_eq!(report.ran(), ["dev"]);
        assert_eq!(report.available(), ["dev", "demo"]);
        assert_eq!(report.profile(), "dev");
        assert!(!report.was_forced());
    }

    #[tokio::test]
    async fn seed_refuses_production_and_says_which_names_exist() {
        use crate::rust_migration::Migrator;
        use crate::seed::Seed;
        use futures_util::future::BoxFuture;

        struct Dev;
        impl Seed for Dev {
            fn name(&self) -> &str {
                "dev"
            }
            fn run<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        let mut seeds = Seeds::default();
        seeds.add(Dev);

        let error = seed(
            "sqlite::memory:",
            &seeds,
            None,
            &SeedOptions::default().profile("production"),
        )
        .await
        .expect_err("refused");
        assert!(error.to_string().contains("--force"), "{error}");

        let error = seed(
            "sqlite::memory:",
            &seeds,
            Some("nope"),
            &SeedOptions::default(),
        )
        .await
        .expect_err("unknown");
        assert!(error.to_string().contains("dev"), "{error}");
    }

    // ── tenants ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn every_tenant_database_is_migrated_and_reported_separately() {
        let dir = temp_dir("tenants");
        std::fs::write(
            dir.join("20260101T000000_a.sql"),
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        )
        .expect("writes");

        let files = temp_dir("tenant-files");
        let targets: Vec<TenantTarget> = ["acme", "globex"]
            .iter()
            .map(|tenant| {
                TenantTarget::database(
                    *tenant,
                    format!("sqlite://{}", files.join(tenant).display()),
                )
            })
            .collect();

        let report = migrate_tenants(&dir, &targets, &RunnerOptions::default(), &|_| {})
            .await
            .expect("migrates");
        assert!(report.is_clean(), "{:?}", report.failures());
        assert_eq!(report.tenants().len(), 2);
        assert_eq!(report.tenants()[0].tenant(), "acme");
        assert_eq!(report.tenants()[0].applied().len(), 1);
        assert_eq!(report.tenants()[0].applied()[0].name(), "a");

        // Running again is a no-op for every tenant, which is what makes this
        // safe to put in a deploy step.
        let again = migrate_tenants(&dir, &targets, &RunnerOptions::default(), &|_| {})
            .await
            .expect("migrates");
        assert!(again.tenants().iter().all(TenantOutcome::is_up_to_date));

        std::fs::remove_dir_all(&dir).expect("cleans up");
        std::fs::remove_dir_all(&files).expect("cleans up");
    }

    #[tokio::test]
    async fn a_failing_tenant_does_not_stop_the_others() {
        let dir = temp_dir("tenant-failure");
        std::fs::write(
            dir.join("20260101T000000_a.sql"),
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        )
        .expect("writes");

        let files = temp_dir("tenant-failure-files");
        let targets = [
            TenantTarget::database("broken", "mysql://localhost/nope"),
            TenantTarget::database("fine", format!("sqlite://{}", files.join("fine").display())),
        ];

        let report = migrate_tenants(&dir, &targets, &RunnerOptions::default(), &|_| {})
            .await
            .expect("reports rather than raising");
        assert!(!report.is_clean());
        assert_eq!(report.failures().len(), 1);
        assert_eq!(report.failures()[0].tenant(), "broken");
        assert_eq!(report.tenants()[1].applied().len(), 1);

        std::fs::remove_dir_all(&dir).expect("cleans up");
        std::fs::remove_dir_all(&files).expect("cleans up");
    }

    #[tokio::test]
    async fn a_tenant_url_never_carries_its_password_into_the_report() {
        let dir = temp_dir("tenant-secret");
        let targets = [TenantTarget::database(
            "acme",
            "postgres://moso:hunter2@127.0.0.1:1/acme",
        )];
        let report = migrate_tenants(&dir, &targets, &RunnerOptions::default(), &|_| {})
            .await
            .expect("reports");
        let document = serde_json::to_string(&report).expect("serialises");
        assert!(!document.contains("hunter2"), "{document}");
        assert!(document.contains("moso:***@"), "{document}");
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    #[tokio::test]
    async fn schema_per_tenant_is_refused_on_sqlite_by_name() {
        let dir = temp_dir("tenant-sqlite-schema");
        let targets = [TenantTarget::schema("acme", "sqlite::memory:", "tenant_7")];
        let report = migrate_tenants(&dir, &targets, &RunnerOptions::default(), &|_| {})
            .await
            .expect("reports");
        let failure = report.failures()[0].failure().expect("a message");
        assert!(failure.contains("SQLite has no schemas"), "{failure}");
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    #[tokio::test]
    async fn a_production_run_refuses_the_flag_before_any_tenant_is_opened() {
        let dir = temp_dir("tenant-prod");
        let targets = [TenantTarget::database("acme", "sqlite::memory:")];
        let error = migrate_tenants(
            &dir,
            &targets,
            &RunnerOptions::default()
                .profile("production")
                .allow_destructive(),
            &|_| {},
        )
        .await
        .expect_err("refused");
        assert!(
            error.to_string().contains("`production` profile"),
            "{error}"
        );
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    #[tokio::test]
    async fn a_tenant_schema_gets_its_own_ledger() {
        let Some(url) = postgres_url("a_tenant_schema_gets_its_own_ledger") else {
            return;
        };
        let dir = temp_dir("tenant-schema");
        std::fs::write(
            dir.join("20260101T000000_a.sql"),
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        )
        .expect("writes");

        let targets = [
            TenantTarget::schema("acme", &url, "moso_tenant_acme"),
            TenantTarget::schema("globex", &url, "moso_tenant_globex"),
        ];
        let report = migrate_tenants(&dir, &targets, &RunnerOptions::default(), &|_| {})
            .await
            .expect("migrates");
        assert!(report.is_clean(), "{:?}", report.failures());

        let mut connection = Connection::open(&url).await.expect("connects");
        for schema in ["moso_tenant_acme", "moso_tenant_globex"] {
            let rows = connection
                .count_rows(&format!(
                    "SELECT tablename FROM pg_tables WHERE schemaname = '{schema}' \
                     AND tablename IN ('a', 'moso_migrations')"
                ))
                .await
                .expect("reads");
            assert_eq!(rows, 2, "`{schema}` has its own table and its own ledger");
            connection
                .execute(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
                .await
                .expect("cleans up");
        }
        connection.close().await;
        std::fs::remove_dir_all(&dir).expect("cleans up");
    }

    #[tokio::test]
    async fn a_tenant_name_that_is_not_an_identifier_never_reaches_sql() {
        let Some(url) = postgres_url("a_tenant_name_that_is_not_an_identifier") else {
            return;
        };
        let mut connection = Connection::open(&url).await.expect("connects");
        let error = enter_schema(&mut connection, "a\"; DROP TABLE users; --")
            .await
            .expect_err("not an identifier");
        assert!(error.to_string().contains("identifier"), "{error}");
        connection.close().await;
    }

    /// The PostgreSQL URL, or `None` with a note, so a machine without a server
    /// skips rather than fails.
    fn postgres_url(label: &str) -> Option<String> {
        match std::env::var("DATABASE_URL") {
            Ok(url) if url.starts_with("postgres") => Some(url),
            _ => {
                eprintln!(
                    "skipping `{label}`: set DATABASE_URL to a PostgreSQL database to run it"
                );
                None
            }
        }
    }
}
