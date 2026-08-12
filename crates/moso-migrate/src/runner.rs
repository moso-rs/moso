//! Applying migrations, safely.
//!
//! Every point of the safety policy in `docs/02-data/23-migrations.md` lives
//! here:
//!
//! | Policy | Where |
//! | --- | --- |
//! | nothing applies unless somebody runs it | there is no boot hook anywhere in the framework |
//! | production takes its acknowledgement as a diff, not a flag | [`RunnerOptions::profile`] |
//! | destructive changes need acknowledgement | [`RunnerOptions::allow_destructive`] |
//! | advisory-lock guarded | [`MigrationLock`] |
//! | transactional unless marked otherwise | [`Runner::migrate`] |
//! | checksums | [`Runner::status`] |
//! | `lock_timeout` and `statement_timeout` | [`Runner::apply_one`] |
//! | expand/contract guidance | [`crate::advice`] |
//!
//! The first row is a property of the framework rather than of this module: no
//! code path applies a migration without a caller, so "never auto-apply in
//! production" needs no profile check. What the profile *does* decide is the
//! second row, and it is the only thing [`RunnerOptions::active_profile`]
//! changes on this path.
//!
//! ```no_run
//! use moso_migrate::runner::{Runner, RunnerOptions};
//!
//! # async fn example() -> moso_migrate::Result<()> {
//! let mut runner = Runner::open("migrations", "postgres://moso:moso@localhost/app").await?;
//! let report = runner.migrate(&RunnerOptions::default()).await?;
//! println!("{} applied", report.applied().len());
//! runner.close().await;
//! # Ok(())
//! # }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use moso_orm::Backend;

use crate::conn::Connection;
use crate::error::{Error, Result};
use crate::file::MigrationFile;
use crate::ledger::{AppliedMigration, DEFAULT_LOCK_LEASE, Ledger, MigrationLock};
use crate::rust_migration::{Migrator, RustMigration};
use crate::version::Version;

/// How long the runner waits for another process's lock, by default.
///
/// ```
/// use std::time::Duration;
/// assert_eq!(moso_migrate::runner::DEFAULT_LOCK_WAIT, Duration::from_secs(60));
/// ```
pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(60);

/// What a runner is allowed to do.
///
/// ```
/// use moso_migrate::runner::RunnerOptions;
///
/// let strict = RunnerOptions::default();
/// assert!(!strict.allows_destructive());
/// assert!(!strict.allows_out_of_order());
/// ```
#[derive(Clone, Debug)]
pub struct RunnerOptions {
    allow_destructive: bool,
    allow_out_of_order: bool,
    lock_wait: Duration,
    lock_lease: Duration,
    target: Option<Version>,
    profile: String,
    dry_run: bool,
}

impl Default for RunnerOptions {
    fn default() -> Self {
        Self {
            allow_destructive: false,
            allow_out_of_order: false,
            lock_wait: DEFAULT_LOCK_WAIT,
            lock_lease: DEFAULT_LOCK_LEASE,
            target: None,
            profile: "dev".to_owned(),
            dry_run: false,
        }
    }
}

impl RunnerOptions {
    /// Applies the destructive blocks as written, rather than refusing.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(RunnerOptions::default().allow_destructive().allows_destructive());
    /// ```
    #[must_use]
    pub const fn allow_destructive(mut self) -> Self {
        self.allow_destructive = true;
        self
    }

    /// Applies a migration that sorts before one already applied.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(RunnerOptions::default().allow_out_of_order().allows_out_of_order());
    /// ```
    #[must_use]
    pub const fn allow_out_of_order(mut self) -> Self {
        self.allow_out_of_order = true;
        self
    }

    /// How long to wait for another process's lock.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// # use std::time::Duration;
    /// let options = RunnerOptions::default().lock_wait(Duration::from_secs(300));
    /// assert_eq!(options.lock_wait_duration(), Duration::from_secs(300));
    /// ```
    #[must_use]
    pub const fn lock_wait(mut self, wait: Duration) -> Self {
        self.lock_wait = wait;
        self
    }

    /// How long a SQLite lock row is good for before another runner may reap it.
    ///
    /// Ignored on PostgreSQL, whose advisory lock is held by the session and
    /// therefore needs no lease. See [`MigrationLock`].
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// # use std::time::Duration;
    /// let options = RunnerOptions::default().lock_lease(Duration::from_secs(1800));
    /// assert_eq!(options.lock_lease_duration(), Duration::from_secs(1800));
    /// ```
    #[must_use]
    pub const fn lock_lease(mut self, lease: Duration) -> Self {
        self.lock_lease = lease;
        self
    }

    /// Stops after a specific version, for `moso db migrate --to`.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// # use moso_migrate::Version;
    /// let options = RunnerOptions::default().up_to(Version::from_parts(2026, 1, 1, 0, 0, 0));
    /// assert!(options.target().is_some());
    /// ```
    #[must_use]
    pub const fn up_to(mut self, version: Version) -> Self {
        self.target = Some(version);
        self
    }

    /// The active profile, which decides how a destructive change may be
    /// acknowledged.
    ///
    /// In a production profile — `"production"`, `"prod"` or `"live"`, per
    /// [`RunnerOptions::is_production`] — [`RunnerOptions::allow_destructive`]
    /// is refused. Nothing else changes: the same files apply, and a destructive
    /// block that a human uncommented and committed still runs. What production
    /// will not accept is a flag typed at a shell standing in for the diff,
    /// because the diff is the record of who acknowledged what.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert_eq!(RunnerOptions::default().profile("production").active_profile(), "production");
    /// ```
    #[must_use]
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = profile.into();
        self
    }

    /// Prints what would run without running it.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(RunnerOptions::default().dry_run().is_dry_run());
    /// ```
    #[must_use]
    pub const fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Whether destructive blocks are applied.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(!RunnerOptions::default().allows_destructive());
    /// ```
    #[must_use]
    pub const fn allows_destructive(&self) -> bool {
        self.allow_destructive
    }

    /// Whether out-of-order migrations are applied.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(!RunnerOptions::default().allows_out_of_order());
    /// ```
    #[must_use]
    pub const fn allows_out_of_order(&self) -> bool {
        self.allow_out_of_order
    }

    /// How long the lock wait is.
    ///
    /// ```
    /// # use moso_migrate::runner::{RunnerOptions, DEFAULT_LOCK_WAIT};
    /// assert_eq!(RunnerOptions::default().lock_wait_duration(), DEFAULT_LOCK_WAIT);
    /// ```
    #[must_use]
    pub const fn lock_wait_duration(&self) -> Duration {
        self.lock_wait
    }

    /// How long the SQLite lock lease is.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// # use moso_migrate::ledger::DEFAULT_LOCK_LEASE;
    /// assert_eq!(RunnerOptions::default().lock_lease_duration(), DEFAULT_LOCK_LEASE);
    /// ```
    #[must_use]
    pub const fn lock_lease_duration(&self) -> Duration {
        self.lock_lease
    }

    /// The version to stop at.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(RunnerOptions::default().target().is_none());
    /// ```
    #[must_use]
    pub const fn target(&self) -> Option<Version> {
        self.target
    }

    /// The active profile.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert_eq!(RunnerOptions::default().active_profile(), "dev");
    /// ```
    #[must_use]
    pub fn active_profile(&self) -> &str {
        &self.profile
    }

    /// Whether this is a dry run.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(!RunnerOptions::default().is_dry_run());
    /// ```
    #[must_use]
    pub const fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Whether the profile is one where `allow_destructive` is refused.
    ///
    /// ```
    /// # use moso_migrate::runner::RunnerOptions;
    /// assert!(RunnerOptions::default().profile("production").is_production());
    /// assert!(RunnerOptions::default().profile("prod").is_production());
    /// assert!(!RunnerOptions::default().is_production());
    /// ```
    #[must_use]
    pub fn is_production(&self) -> bool {
        matches!(self.profile.as_str(), "production" | "prod" | "live")
    }

    /// The one thing the profile decides on the migrate path.
    ///
    /// # Errors
    ///
    /// [`Error::RefusedInProduction`] when `allow_destructive` is set against a
    /// production profile.
    fn guard_profile(&self) -> Result<()> {
        if self.is_production() && self.allow_destructive {
            return Err(Error::RefusedInProduction {
                command: "moso db migrate --allow-destructive",
                profile: self.profile.clone(),
                help: "in production the acknowledgement is the committed diff that uncomments \
                       the `-- +migrate destructive` block, not a flag typed at a shell — \
                       uncomment it, commit it, deploy it",
            });
        }
        Ok(())
    }
}

/// What one migration run did.
///
/// ```
/// use moso_migrate::runner::MigrateReport;
///
/// let report = MigrateReport::default();
/// assert!(report.applied().is_empty());
/// assert!(report.is_up_to_date());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MigrateReport {
    applied: Vec<(Version, String, Duration)>,
    skipped: Vec<Version>,
    waited: Duration,
    dry_run: bool,
}

impl MigrateReport {
    /// The migrations that ran, in order, with how long each took.
    ///
    /// ```
    /// assert!(moso_migrate::runner::MigrateReport::default().applied().is_empty());
    /// ```
    #[must_use]
    pub fn applied(&self) -> &[(Version, String, Duration)] {
        &self.applied
    }

    /// Migrations that were already applied.
    ///
    /// ```
    /// assert!(moso_migrate::runner::MigrateReport::default().skipped().is_empty());
    /// ```
    #[must_use]
    pub fn skipped(&self) -> &[Version] {
        &self.skipped
    }

    /// How long the run waited for another process's lock — the number that
    /// tells you whether twenty pods really did start at once.
    ///
    /// ```
    /// assert!(moso_migrate::runner::MigrateReport::default().waited().is_zero());
    /// ```
    #[must_use]
    pub const fn waited(&self) -> Duration {
        self.waited
    }

    /// Whether nothing needed doing.
    ///
    /// ```
    /// assert!(moso_migrate::runner::MigrateReport::default().is_up_to_date());
    /// ```
    #[must_use]
    pub fn is_up_to_date(&self) -> bool {
        self.applied.is_empty()
    }

    /// Whether this was a dry run.
    ///
    /// ```
    /// assert!(!moso_migrate::runner::MigrateReport::default().was_dry_run());
    /// ```
    #[must_use]
    pub const fn was_dry_run(&self) -> bool {
        self.dry_run
    }
}

/// What `moso db status` prints.
///
/// ```
/// use moso_migrate::runner::Status;
///
/// let status = Status::default();
/// assert!(status.is_clean());
/// ```
#[derive(Clone, Debug, Default)]
pub struct Status {
    applied: Vec<AppliedMigration>,
    pending: Vec<Version>,
    dirty: Vec<AppliedMigration>,
    changed: Vec<(Version, String, String)>,
    missing: Vec<Version>,
    out_of_order: Vec<(Version, Version)>,
}

impl Status {
    /// Everything the ledger records.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().applied().is_empty());
    /// ```
    #[must_use]
    pub fn applied(&self) -> &[AppliedMigration] {
        &self.applied
    }

    /// Versions on disk that have not run.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().pending().is_empty());
    /// ```
    #[must_use]
    pub fn pending(&self) -> &[Version] {
        &self.pending
    }

    /// Migrations that failed part-way through.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().dirty().is_empty());
    /// ```
    #[must_use]
    pub fn dirty(&self) -> &[AppliedMigration] {
        &self.dirty
    }

    /// Applied migrations whose file has changed: version, recorded checksum,
    /// current checksum.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().changed().is_empty());
    /// ```
    #[must_use]
    pub fn changed(&self) -> &[(Version, String, String)] {
        &self.changed
    }

    /// Applied migrations whose file is gone.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().missing().is_empty());
    /// ```
    #[must_use]
    pub fn missing(&self) -> &[Version] {
        &self.missing
    }

    /// Pending migrations that sort before an applied one: the pending version
    /// and the applied one it sorts before.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().out_of_order().is_empty());
    /// ```
    #[must_use]
    pub fn out_of_order(&self) -> &[(Version, Version)] {
        &self.out_of_order
    }

    /// Whether there is nothing wrong — pending migrations are not wrong.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().is_clean());
    /// ```
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.dirty.is_empty() && self.changed.is_empty() && self.missing.is_empty()
    }

    /// Turns the first problem into the error that explains it.
    ///
    /// # Errors
    ///
    /// [`Error::Dirty`], [`Error::ChecksumMismatch`] or [`Error::MissingFile`],
    /// whichever applies, in that order of severity.
    ///
    /// ```
    /// assert!(moso_migrate::runner::Status::default().into_result().is_ok());
    /// ```
    pub fn into_result(self) -> Result<Self> {
        if let Some(dirty) = self.dirty.first() {
            return Err(Error::Dirty {
                version: dirty.version(),
                name: dirty.name().to_owned(),
                statement: usize::try_from(dirty.failed_statement().unwrap_or(0)).unwrap_or(0),
                total: usize::try_from(dirty.total_statements().unwrap_or(0)).unwrap_or(0),
                sql: dirty
                    .failure()
                    .unwrap_or("the failure was not recorded")
                    .to_owned(),
            });
        }
        if let Some((version, recorded, actual)) = self.changed.first() {
            return Err(Error::ChecksumMismatch {
                version: *version,
                name: self
                    .applied
                    .iter()
                    .find(|applied| applied.version() == *version)
                    .map_or_else(String::new, |applied| applied.name().to_owned()),
                recorded: recorded.clone(),
                actual: actual.clone(),
            });
        }
        if let Some(version) = self.missing.first() {
            return Err(Error::MissingFile { version: *version });
        }
        Ok(self)
    }
}

/// Reads a migrations directory and applies it.
///
/// ```no_run
/// use moso_migrate::runner::Runner;
///
/// # async fn example() -> moso_migrate::Result<()> {
/// let mut runner = Runner::open("migrations", "sqlite://app.db").await?;
/// println!("{} on disk", runner.files().len());
/// runner.close().await;
/// # Ok(())
/// # }
/// ```
pub struct Runner {
    directory: PathBuf,
    connection: Connection,
    files: Vec<MigrationFile>,
    rust: Vec<Box<dyn RustMigration>>,
}

impl std::fmt::Debug for Runner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Runner")
            .field("directory", &self.directory)
            .field("backend", &self.connection.backend())
            .field("files", &self.files.len())
            .field("rust_migrations", &self.rust.len())
            .finish()
    }
}

impl Runner {
    /// Opens a connection and reads the directory.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory cannot be read,
    /// [`Error::DuplicateVersion`] when two files share a version, plus
    /// everything [`Connection::open`] returns.
    ///
    /// ```no_run
    /// # async fn example() -> moso_migrate::Result<()> {
    /// let runner = moso_migrate::runner::Runner::open("migrations", "sqlite://app.db").await?;
    /// # let _ = runner;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(directory: impl AsRef<Path>, url: &str) -> Result<Self> {
        let connection = Connection::open(url).await?;
        Self::with_connection(directory, connection)
    }

    /// Uses a connection the caller already has.
    ///
    /// # Errors
    ///
    /// As [`Runner::open`], minus the connection failures.
    ///
    /// ```no_run
    /// # async fn example(connection: moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let runner = moso_migrate::runner::Runner::with_connection("migrations", connection)?;
    /// # let _ = runner;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_connection(directory: impl AsRef<Path>, connection: Connection) -> Result<Self> {
        let directory = directory.as_ref().to_path_buf();
        let files = read_directory(&directory)?;
        Ok(Self {
            directory,
            connection,
            files,
            rust: Vec::new(),
        })
    }

    /// Registers a migration written in Rust.
    ///
    /// ```no_run
    /// # use futures_util::future::BoxFuture;
    /// # use moso_migrate::rust_migration::{Migrator, RustMigration};
    /// # use moso_migrate::{Result, Version};
    /// # struct Backfill;
    /// # impl RustMigration for Backfill {
    /// #     fn version(&self) -> Version { Version::from_parts(2026, 1, 1, 0, 0, 0) }
    /// #     fn name(&self) -> &str { "backfill" }
    /// #     fn up<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
    /// #         Box::pin(async { Ok(()) })
    /// #     }
    /// # }
    /// # async fn example(mut runner: moso_migrate::runner::Runner) {
    /// runner.register(Backfill);
    /// # }
    /// ```
    pub fn register(&mut self, migration: impl RustMigration) {
        self.rust.push(Box::new(migration));
    }

    /// The directory it reads.
    ///
    /// ```no_run
    /// # async fn example(runner: &moso_migrate::runner::Runner) {
    /// println!("{}", runner.directory().display());
    /// # }
    /// ```
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The backend.
    ///
    /// ```no_run
    /// # async fn example(runner: &moso_migrate::runner::Runner) {
    /// assert_eq!(runner.backend(), moso_orm::Backend::Sqlite);
    /// # }
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.connection.backend()
    }

    /// The SQL migrations on disk, oldest first.
    ///
    /// ```no_run
    /// # async fn example(runner: &moso_migrate::runner::Runner) {
    /// for file in runner.files() {
    ///     println!("{}", file.id());
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn files(&self) -> &[MigrationFile] {
        &self.files
    }

    /// The connection, for a caller that wants to run something alongside.
    ///
    /// ```no_run
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// runner.connection().execute("SELECT 1").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub const fn connection(&mut self) -> &mut Connection {
        &mut self.connection
    }

    /// Closes the connection.
    ///
    /// ```no_run
    /// # async fn example(runner: moso_migrate::runner::Runner) {
    /// runner.close().await;
    /// # }
    /// ```
    pub async fn close(self) {
        self.connection.close().await;
    }

    /// Every version, SQL and Rust, in order.
    ///
    /// ```no_run
    /// # async fn example(runner: &moso_migrate::runner::Runner) {
    /// assert!(runner.versions().windows(2).all(|pair| pair[0] < pair[1]));
    /// # }
    /// ```
    #[must_use]
    pub fn versions(&self) -> Vec<Version> {
        let mut versions: Vec<Version> = self
            .files
            .iter()
            .map(MigrationFile::version)
            .chain(self.rust.iter().map(|migration| migration.version()))
            .collect();
        versions.sort_unstable();
        versions
    }

    /// Compares the ledger with the directory.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the ledger cannot be read.
    ///
    /// ```no_run
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// let status = runner.status().await?;
    /// println!("{} pending", status.pending().len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn status(&mut self) -> Result<Status> {
        Ledger::ensure(&mut self.connection).await?;
        let applied = Ledger::applied(&mut self.connection).await?;
        let applied_versions: BTreeMap<Version, &AppliedMigration> = applied
            .iter()
            .map(|record| (record.version(), record))
            .collect();

        let mut status = Status {
            dirty: applied.iter().filter(|r| r.is_dirty()).cloned().collect(),
            ..Status::default()
        };

        // `(checksum, is_a_squashed_baseline)`. A baseline deliberately has a
        // different body from the migration whose version it took over, so a
        // checksum mismatch on one is expected rather than alarming.
        let mut on_disk: BTreeMap<Version, (String, bool)> = BTreeMap::new();
        for file in &self.files {
            on_disk.insert(
                file.version(),
                (file.checksum().to_string(), !file.replaces().is_empty()),
            );
        }
        for migration in &self.rust {
            on_disk.insert(
                migration.version(),
                (migration.fingerprint().to_string(), false),
            );
        }

        // A version a baseline replaces is no longer expected to have a file.
        let replaced: Vec<Version> = self
            .files
            .iter()
            .flat_map(|file| file.replaces().iter().copied())
            .collect();

        for (version, record) in &applied_versions {
            match on_disk.get(version) {
                Some((checksum, _)) if checksum == record.checksum_text() => {}
                Some((_, true)) => {}
                Some((checksum, false)) => status.changed.push((
                    *version,
                    record.checksum_text().to_owned(),
                    checksum.clone(),
                )),
                None if replaced.contains(version) => {}
                None => status.missing.push(*version),
            }
        }

        let highest_applied = applied_versions.keys().copied().next_back();
        for version in on_disk.keys() {
            if applied_versions.contains_key(version) {
                continue;
            }
            status.pending.push(*version);
            if let Some(highest) = highest_applied
                && *version < highest
            {
                status.out_of_order.push((*version, highest));
            }
        }

        status.applied = applied;
        Ok(status)
    }

    /// Applies every pending migration.
    ///
    /// # Errors
    ///
    /// Everything the safety policy refuses: [`Error::Dirty`],
    /// [`Error::ChecksumMismatch`], [`Error::MissingFile`],
    /// [`Error::OutOfOrder`], [`Error::Destructive`],
    /// [`Error::ManualMigrationRequired`], [`Error::RefusedInProduction`] when
    /// `allow_destructive` is set against a production profile,
    /// [`Error::LockTimeout`], [`Error::LockLost`], and whatever the database
    /// says about a statement.
    ///
    /// ```no_run
    /// # use moso_migrate::runner::RunnerOptions;
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// let report = runner.migrate(&RunnerOptions::default()).await?;
    /// println!("{} applied", report.applied().len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn migrate(&mut self, options: &RunnerOptions) -> Result<MigrateReport> {
        // The profile is read here, before anything is opened or locked, so a
        // run that production will not accept is refused with nothing done.
        options.guard_profile()?;
        let status = self.status().await?.into_result()?;

        if !options.allows_out_of_order()
            && let Some((version, applied)) = status.out_of_order().first()
        {
            return Err(Error::OutOfOrder {
                version: *version,
                applied: *applied,
            });
        }

        let mut pending: Vec<Version> = status.pending().to_vec();
        if let Some(target) = options.target() {
            pending.retain(|version| *version <= target);
        }

        let mut report = MigrateReport {
            skipped: status
                .applied()
                .iter()
                .map(AppliedMigration::version)
                .collect(),
            dry_run: options.is_dry_run(),
            ..MigrateReport::default()
        };
        if pending.is_empty() {
            return Ok(report);
        }

        // Check every file's destructive gate before touching the database, so
        // a run that will be refused is refused before it half-applies.
        for version in &pending {
            if let Some(file) = self.file(*version) {
                file.statements_to_apply(options.allows_destructive())?;
            }
        }

        if options.is_dry_run() {
            report.applied = pending
                .iter()
                .map(|version| (*version, self.name_of(*version), Duration::ZERO))
                .collect();
            return Ok(report);
        }

        let lock = MigrationLock::acquire_with_lease(
            &mut self.connection,
            options.lock_wait_duration(),
            options.lock_lease_duration(),
        )
        .await?;
        report.waited = lock.waited();

        // Another process may have applied some of these while we waited.
        let already: Vec<Version> = Ledger::applied(&mut self.connection)
            .await?
            .iter()
            .map(AppliedMigration::version)
            .collect();
        pending.retain(|version| !already.contains(version));

        let outcome = self.apply_all(&pending, options, &lock, &mut report).await;
        lock.release(&mut self.connection).await;
        outcome?;
        Ok(report)
    }

    async fn apply_all(
        &mut self,
        pending: &[Version],
        options: &RunnerOptions,
        lock: &MigrationLock,
        report: &mut MigrateReport,
    ) -> Result<()> {
        for version in pending {
            // Renew before each file rather than after, so the lease covers the
            // migration that is about to run. A file slower than the whole lease
            // is the one case this cannot cover, and it is why losing the lock
            // is an error rather than a warning.
            if !lock.refresh(&mut self.connection).await? {
                return Err(Error::LockLost {
                    held_secs: lock.held().as_secs(),
                });
            }
            let started = Instant::now();
            self.apply_one(*version, options).await?;
            report
                .applied
                .push((*version, self.name_of(*version), started.elapsed()));
        }
        Ok(())
    }

    /// Applies one migration, with its timeouts and its transaction.
    ///
    /// A transactional migration runs `BEGIN`, its statements, the ledger
    /// insert and `COMMIT` — so a failure leaves nothing at all, including no
    /// ledger row. A non-transactional one inserts its ledger row *first*,
    /// dirty, and clears the flag at the end; a failure in the middle therefore
    /// leaves a row that says exactly which statement died.
    ///
    /// # Errors
    ///
    /// [`Error::RefusedInProduction`] when `allow_destructive` is set against a
    /// production profile, and otherwise whatever the database says, wrapped
    /// with the statement that caused it.
    ///
    /// ```no_run
    /// # use moso_migrate::runner::RunnerOptions;
    /// # use moso_migrate::Version;
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// runner.apply_one(Version::from_parts(2026, 1, 1, 0, 0, 0), &RunnerOptions::default()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn apply_one(&mut self, version: Version, options: &RunnerOptions) -> Result<()> {
        // Repeated from `migrate` because this is public and reachable on its
        // own; the check is cheap and a guard that only one entry point applies
        // is a guard with a hole in it.
        options.guard_profile()?;
        if let Some(index) = self.rust.iter().position(|m| m.version() == version) {
            return self.apply_rust(index).await;
        }
        let Some(index) = self.files.iter().position(|file| file.version() == version) else {
            return Err(Error::MissingFile { version });
        };

        // A squashed baseline that stands in for migrations this database has
        // already applied must not run: it would recreate tables that exist.
        // It is recorded instead, and the rows it replaces are removed, so the
        // ledger ends up looking like a database that started from the
        // baseline.
        let replaces = self.files[index].replaces().to_vec();
        if !replaces.is_empty() {
            let already: Vec<Version> = Ledger::applied(&mut self.connection)
                .await?
                .iter()
                .map(AppliedMigration::version)
                .collect();
            if replaces.iter().all(|version| already.contains(version)) {
                let file = &self.files[index];
                let name = file.id().name().to_owned();
                let checksum = file.checksum();
                Ledger::begin(&mut self.connection, version, &name, checksum, 0).await?;
                Ledger::finish(&mut self.connection, version, Duration::ZERO).await?;
                for replaced in replaces {
                    if replaced != version {
                        Ledger::forget(&mut self.connection, replaced).await?;
                    }
                }
                return Ok(());
            }
        }

        let file = &self.files[index];
        let statements = file.statements_to_apply(options.allows_destructive())?;
        let transactional = file.is_transactional();
        let lock_timeout = file.lock_timeout();
        let statement_timeout = file.statement_timeout();
        let checksum = file.checksum();
        let name = file.id().name().to_owned();
        let total = statements.len();

        self.set_timeouts(lock_timeout, statement_timeout).await?;
        let started = Instant::now();

        if transactional {
            self.connection.execute("BEGIN").await?;
            match self
                .run_statements(&statements, version, &name, checksum, total, false)
                .await
            {
                Ok(()) => {
                    Ledger::finish(&mut self.connection, version, started.elapsed()).await?;
                    self.connection.execute("COMMIT").await?;
                    Ok(())
                }
                Err(error) => {
                    let _ = self.connection.execute("ROLLBACK").await;
                    Err(error)
                }
            }
        } else {
            self.run_statements(&statements, version, &name, checksum, total, true)
                .await?;
            Ledger::finish(&mut self.connection, version, started.elapsed()).await
        }
    }

    async fn run_statements(
        &mut self,
        statements: &[String],
        version: Version,
        name: &str,
        checksum: crate::hash::Checksum,
        total: usize,
        record_failures: bool,
    ) -> Result<()> {
        Ledger::begin(&mut self.connection, version, name, checksum, total).await?;
        for (index, statement) in statements.iter().enumerate() {
            let outcome = if is_foreign_key_check(statement) {
                match self.connection.count_rows(statement).await {
                    Ok(0) => Ok(()),
                    Ok(violations) => Err(Error::Unsupported {
                        backend: self.connection.backend().as_str(),
                        operation: format!(
                            "finish `{version}`: the table rebuild left {violations} foreign-key \
                             violation(s)"
                        ),
                        help: "the copied rows reference something that is not there; check the \
                               `INSERT INTO … SELECT` in this migration"
                            .to_owned(),
                    }),
                    Err(error) => Err(error),
                }
            } else {
                self.connection.execute(statement).await.map(|_| ())
            };

            if let Err(error) = outcome {
                if record_failures {
                    let _ = Ledger::mark_failed(
                        &mut self.connection,
                        version,
                        index + 1,
                        &error.to_string(),
                    )
                    .await;
                }
                return Err(error);
            }
        }
        Ok(())
    }

    async fn apply_rust(&mut self, index: usize) -> Result<()> {
        // The migration is taken out of the vector for the duration of the run
        // so that `&mut self.connection` and `&self.rust[index]` do not overlap.
        let migration = self.rust.remove(index);
        let version = migration.version();
        let name = migration.name().to_owned();
        let fingerprint = migration.fingerprint();
        let transactional = migration.is_transactional();
        let started = Instant::now();

        let outcome = async {
            if transactional {
                self.connection.execute("BEGIN").await?;
            }
            Ledger::begin(&mut self.connection, version, &name, fingerprint, 0).await?;
            let mut migrator = Migrator::new(&mut self.connection);
            match migration.up(&mut migrator).await {
                Ok(()) => {
                    Ledger::finish(&mut self.connection, version, started.elapsed()).await?;
                    if transactional {
                        self.connection.execute("COMMIT").await?;
                    }
                    Ok(())
                }
                Err(error) => {
                    if transactional {
                        let _ = self.connection.execute("ROLLBACK").await;
                    } else {
                        let _ = Ledger::mark_failed(
                            &mut self.connection,
                            version,
                            0,
                            &error.to_string(),
                        )
                        .await;
                    }
                    Err(Error::MigrationFailed {
                        version,
                        message: error.to_string(),
                    })
                }
            }
        }
        .await;

        self.rust.insert(index, migration);
        outcome
    }

    /// Rolls back the last `steps` applied migrations.
    ///
    /// # Errors
    ///
    /// [`Error::MissingFile`] when a rolled-back migration's file is gone —
    /// there is no way to undo something whose `down` section nobody has.
    /// [`Error::Unsupported`] when the migration is irreversible.
    ///
    /// ```no_run
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// let undone = runner.rollback(1).await?;
    /// println!("{} rolled back", undone.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn rollback(&mut self, steps: usize) -> Result<Vec<Version>> {
        Ledger::ensure(&mut self.connection).await?;
        let applied = Ledger::applied(&mut self.connection).await?;
        let targets: Vec<Version> = applied
            .iter()
            .rev()
            .take(steps)
            .map(AppliedMigration::version)
            .collect();

        let lock = MigrationLock::acquire(&mut self.connection, DEFAULT_LOCK_WAIT).await?;
        let mut undone = Vec::with_capacity(targets.len());
        let outcome = async {
            for version in &targets {
                if !lock.refresh(&mut self.connection).await? {
                    return Err(Error::LockLost {
                        held_secs: lock.held().as_secs(),
                    });
                }
                self.rollback_one(*version).await?;
                undone.push(*version);
            }
            Ok::<(), Error>(())
        }
        .await;
        lock.release(&mut self.connection).await;
        outcome?;
        Ok(undone)
    }

    async fn rollback_one(&mut self, version: Version) -> Result<()> {
        if let Some(index) = self.rust.iter().position(|m| m.version() == version) {
            let migration = self.rust.remove(index);
            let outcome = if migration.is_reversible() {
                let mut migrator = Migrator::new(&mut self.connection);
                migration.down(&mut migrator).await
            } else {
                Err(Error::Unsupported {
                    backend: self.connection.backend().as_str(),
                    operation: format!("roll back `{version}`"),
                    help: format!(
                        "`{}` declares `is_reversible() -> false`; write a new migration that \
                         undoes it instead",
                        migration.name()
                    ),
                })
            };
            self.rust.insert(index, migration);
            outcome?;
            Ledger::forget(&mut self.connection, version).await?;
            return Ok(());
        }

        let Some(index) = self.files.iter().position(|file| file.version() == version) else {
            return Err(Error::MissingFile { version });
        };
        let file = &self.files[index];
        if !file.is_reversible() || file.down().is_empty() {
            return Err(Error::Unsupported {
                backend: self.connection.backend().as_str(),
                operation: format!("roll back `{}`", file.id()),
                help: format!(
                    "`{}` has no `-- +migrate down` section; write a new migration that undoes it",
                    file.id().file_name("sql")
                ),
            });
        }
        let statements = file.down().to_vec();
        let transactional = file.is_transactional();
        let lock_timeout = file.lock_timeout();
        let statement_timeout = file.statement_timeout();

        self.set_timeouts(lock_timeout, statement_timeout).await?;
        if transactional {
            self.connection.execute("BEGIN").await?;
        }
        for statement in &statements {
            if let Err(error) = self.connection.execute(statement).await {
                if transactional {
                    let _ = self.connection.execute("ROLLBACK").await;
                }
                return Err(error);
            }
        }
        Ledger::forget(&mut self.connection, version).await?;
        if transactional {
            self.connection.execute("COMMIT").await?;
        }
        Ok(())
    }

    /// Rolls the last migration back and applies it again — the development
    /// loop.
    ///
    /// # Errors
    ///
    /// As [`Runner::rollback`] and [`Runner::migrate`].
    ///
    /// ```no_run
    /// # use moso_migrate::runner::RunnerOptions;
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// runner.redo(&RunnerOptions::default()).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn redo(&mut self, options: &RunnerOptions) -> Result<MigrateReport> {
        self.rollback(1).await?;
        self.migrate(options).await
    }

    /// Sets `lock_timeout` and `statement_timeout` for the session.
    ///
    /// This is safety-policy point 6, and it is the single default in this
    /// crate most likely to prevent an outage: a migration queued behind a long
    /// transaction queues every query behind *itself*, and five seconds later
    /// the site is down. Failing fast turns that into a retry.
    ///
    /// SQLite has neither setting; its equivalent, `busy_timeout`, is set when
    /// the connection is opened.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the settings are refused.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(runner: &mut moso_migrate::runner::Runner) -> moso_migrate::Result<()> {
    /// runner.set_timeouts(Duration::from_secs(5), Duration::from_secs(60)).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_timeouts(&mut self, lock: Duration, statement: Duration) -> Result<()> {
        if self.connection.backend() != Backend::Postgres {
            return Ok(());
        }
        self.connection
            .execute(&format!("SET lock_timeout = '{}ms'", lock.as_millis()))
            .await?;
        self.connection
            .execute(&format!(
                "SET statement_timeout = '{}ms'",
                statement.as_millis()
            ))
            .await?;
        Ok(())
    }

    fn file(&self, version: Version) -> Option<&MigrationFile> {
        self.files.iter().find(|file| file.version() == version)
    }

    fn name_of(&self, version: Version) -> String {
        self.file(version)
            .map(|file| file.id().name().to_owned())
            .or_else(|| {
                self.rust
                    .iter()
                    .find(|m| m.version() == version)
                    .map(|m| m.name().to_owned())
            })
            .unwrap_or_default()
    }
}

/// Whether a statement is the SQLite rebuild's foreign-key check, which
/// reports violations as rows rather than as an error.
fn is_foreign_key_check(statement: &str) -> bool {
    statement
        .trim()
        .trim_end_matches(';')
        .trim()
        .eq_ignore_ascii_case("PRAGMA foreign_key_check")
}

/// Reads every `*.sql` in a directory, in version order.
///
/// # Errors
///
/// [`Error::Io`] when the directory cannot be listed,
/// [`Error::DuplicateVersion`] when two files share a version.
///
/// ```no_run
/// let files = moso_migrate::runner::read_directory("migrations")?;
/// println!("{} migrations", files.len());
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn read_directory(directory: impl AsRef<Path>) -> Result<Vec<MigrationFile>> {
    let directory = directory.as_ref();
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let entries = std::fs::read_dir(directory).map_err(|source| {
        Error::io(
            "listing",
            directory,
            "create it with `moso db make-migration`, or point `--dir` at the right place",
            source,
        )
    })?;

    let mut files = Vec::new();
    let mut seen: BTreeMap<Version, String> = BTreeMap::new();
    for entry in entries {
        let entry = entry.map_err(|source| {
            Error::io(
                "listing",
                directory,
                "check the directory's permissions",
                source,
            )
        })?;
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("sql") {
            continue;
        }
        let file = MigrationFile::read(&path)?;
        if let Some(first) = seen.insert(file.version(), file.id().file_name("sql")) {
            return Err(Error::DuplicateVersion {
                version: file.version(),
                first,
                second: file.id().file_name("sql"),
            });
        }
        files.push(file);
    }
    files.sort_by_key(MigrationFile::version);
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Connection;

    fn write(directory: &Path, name: &str, body: &str) {
        std::fs::write(directory.join(name), body).expect("writes");
    }

    fn temp_dir(label: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "moso-migrate-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::create_dir_all(&base).expect("creates");
        base
    }

    async fn runner(directory: &Path) -> Runner {
        let connection = Connection::open("sqlite::memory:").await.expect("opens");
        Runner::with_connection(directory, connection).expect("reads")
    }

    #[tokio::test]
    async fn an_empty_directory_has_nothing_to_do() {
        let dir = temp_dir("empty");
        let mut runner = runner(&dir).await;
        let report = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");
        assert!(report.is_up_to_date());
        runner.close().await;
    }

    #[tokio::test]
    async fn migrations_apply_in_order_and_are_recorded() {
        let dir = temp_dir("order");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer primary key);\n\
             -- +migrate down\nDROP TABLE a;\n",
        );
        write(
            &dir,
            "20260102T000000_b.sql",
            "-- +migrate up\nCREATE TABLE b (id integer primary key);\n\
             -- +migrate down\nDROP TABLE b;\n",
        );

        let mut runner = runner(&dir).await;
        let report = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");
        assert_eq!(report.applied().len(), 2);
        assert_eq!(report.applied()[0].1, "a");

        let status = runner.status().await.expect("status");
        assert!(status.pending().is_empty());
        assert!(status.is_clean());
        assert_eq!(status.applied().len(), 2);
        assert!(!status.applied()[0].is_dirty());

        // Running again does nothing.
        let again = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");
        assert!(again.is_up_to_date());
        runner.close().await;
    }

    #[tokio::test]
    async fn a_failing_transactional_migration_leaves_nothing_behind() {
        let dir = temp_dir("rollback");
        write(
            &dir,
            "20260101T000000_bad.sql",
            "-- +migrate up\nCREATE TABLE ok (id integer);\nCRAETE TABLE oops (id integer);\n",
        );
        let mut runner = runner(&dir).await;
        let error = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("syntax error");
        assert!(error.to_string().contains("CRAETE"), "{error}");

        let status = runner.status().await.expect("status");
        assert!(
            status.applied().is_empty(),
            "the ledger row rolled back too"
        );
        assert_eq!(status.pending().len(), 1);

        let tables = runner
            .connection()
            .count_rows("SELECT name FROM sqlite_master WHERE name = 'ok'")
            .await
            .expect("counts");
        assert_eq!(tables, 0, "the first statement rolled back too");
        runner.close().await;
    }

    #[tokio::test]
    async fn a_failing_non_transactional_migration_is_dirty_and_says_where() {
        let dir = temp_dir("dirty");
        write(
            &dir,
            "20260101T000000_bad.sql",
            "-- moso:transactional false\n\
             -- +migrate up\nCREATE TABLE ok (id integer);\nCRAETE TABLE oops (id integer);\n",
        );
        let mut runner = runner(&dir).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("syntax error");

        let status = runner.status().await.expect("status");
        assert_eq!(status.dirty().len(), 1);
        assert_eq!(status.dirty()[0].failed_statement(), Some(2));
        assert_eq!(status.dirty()[0].total_statements(), Some(2));

        let error = status.into_result().expect_err("dirty");
        assert!(error.to_string().contains("statement 2 of 2"), "{error}");
        assert!(error.to_string().contains("repair --resolve"), "{error}");

        // The first statement did take effect: that is what dirty means.
        let tables = runner
            .connection()
            .count_rows("SELECT name FROM sqlite_master WHERE name = 'ok'")
            .await
            .expect("counts");
        assert_eq!(tables, 1);
        runner.close().await;
    }

    #[tokio::test]
    async fn editing_an_applied_migration_is_refused_by_checksum() {
        let dir = temp_dir("checksum");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        );
        let mut runner = runner(&dir).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");

        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer, extra text);\n",
        );
        let connection = std::mem::replace(
            runner.connection(),
            Connection::open("sqlite::memory:")
                .await
                .expect("placeholder"),
        );
        let mut edited = Runner::with_connection(&dir, connection).expect("reads");

        let error = edited
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("checksum");
        assert!(
            error
                .to_string()
                .contains("has changed since it was applied"),
            "{error}"
        );
        assert!(
            error.to_string().contains("write a new migration"),
            "{error}"
        );
        edited.close().await;
    }

    #[tokio::test]
    async fn a_destructive_migration_is_refused_until_acknowledged() {
        let dir = temp_dir("destructive");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        );
        write(
            &dir,
            "20260102T000000_drop.sql",
            "-- moso:destructive\n\
             -- +migrate up\n\
             -- ⚠ DESTRUCTIVE: it drops `a`.\n\
             -- +migrate destructive\n\
             -- DROP TABLE a;\n\
             -- +migrate end\n",
        );

        let mut runner = runner(&dir).await;
        let error = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("--allow-destructive"), "{error}");

        // Nothing at all ran: the gate is checked before the first statement.
        assert!(runner.status().await.expect("status").applied().is_empty());

        let report = runner
            .migrate(&RunnerOptions::default().allow_destructive())
            .await
            .expect("allowed");
        assert_eq!(report.applied().len(), 2);
        let tables = runner
            .connection()
            .count_rows("SELECT name FROM sqlite_master WHERE name = 'a'")
            .await
            .expect("counts");
        assert_eq!(tables, 0, "the drop ran");
        runner.close().await;
    }

    #[tokio::test]
    async fn production_refuses_the_allow_destructive_flag() {
        let dir = temp_dir("prod");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        );
        write(
            &dir,
            "20260102T000000_drop.sql",
            "-- moso:destructive\n\
             -- +migrate up\n\
             -- ⚠ DESTRUCTIVE: it drops `a`.\n\
             -- +migrate destructive\n\
             -- DROP TABLE a;\n\
             -- +migrate end\n",
        );

        let mut runner = runner(&dir).await;
        let options = RunnerOptions::default()
            .profile("production")
            .allow_destructive();
        let error = runner.migrate(&options).await.expect_err("refused");
        assert!(
            error.to_string().contains("`production` profile"),
            "{error}"
        );
        assert!(error.to_string().contains("committed diff"), "{error}");

        // Nothing ran, including the migration that is not destructive at all:
        // the profile is read before the directory is even classified.
        assert!(runner.status().await.expect("status").applied().is_empty());

        // The same run in production without the flag applies the ordinary
        // migration and still refuses the unacknowledged block.
        let error = runner
            .migrate(&RunnerOptions::default().profile("production"))
            .await
            .expect_err("still gated");
        assert!(error.to_string().contains("--allow-destructive"), "{error}");
        runner.close().await;
    }

    #[tokio::test]
    async fn a_production_migration_with_the_block_uncommented_still_applies() {
        let dir = temp_dir("prod-ack");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        );
        write(
            &dir,
            "20260102T000000_drop.sql",
            "-- moso:destructive\n\
             -- +migrate up\n\
             -- ⚠ DESTRUCTIVE: it drops `a`.\n\
             -- +migrate destructive\n\
             DROP TABLE a;\n\
             -- +migrate end\n",
        );

        let mut runner = runner(&dir).await;
        let report = runner
            .migrate(&RunnerOptions::default().profile("production"))
            .await
            .expect("the diff is the acknowledgement production accepts");
        assert_eq!(report.applied().len(), 2);
        runner.close().await;
    }

    #[tokio::test]
    async fn a_manual_block_is_refused_however_the_flags_are_set() {
        let dir = temp_dir("manual");
        write(
            &dir,
            "20260101T000000_rewrite.sql",
            "-- moso:destructive\n\
             -- +migrate up\n\
             -- ⚠ DESTRUCTIVE: rewrite the type `user_role`.\n\
             -- +migrate destructive\n\
             -- -- CREATE TYPE \"user_role_new\" AS ENUM ('admin');\n\
             -- +migrate end\n",
        );
        let mut runner = runner(&dir).await;
        for options in [
            RunnerOptions::default(),
            RunnerOptions::default().allow_destructive(),
        ] {
            let error = runner.migrate(&options).await.expect_err("a template");
            assert!(error.to_string().contains("cannot apply"), "{error}");
        }
        assert!(runner.status().await.expect("status").applied().is_empty());
        runner.close().await;
    }

    #[tokio::test]
    async fn a_stale_sqlite_lock_does_not_block_the_next_run() {
        // The killed-migrator case, end to end: the lock row is on disk with an
        // expired lease and `migrate` reaps it instead of waiting `lock_wait`
        // out and failing.
        let dir = temp_dir("stale-lock");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        );
        let mut runner = runner(&dir).await;
        let abandoned =
            MigrationLock::acquire_with_lease(runner.connection(), Duration::ZERO, Duration::ZERO)
                .await
                .expect("takes it");
        drop(abandoned);

        let report = runner
            .migrate(&RunnerOptions::default().lock_wait(Duration::from_millis(50)))
            .await
            .expect("the expired row is reaped");
        assert_eq!(report.applied().len(), 1);
        runner.close().await;
    }

    #[tokio::test]
    async fn out_of_order_is_detected_and_can_be_allowed() {
        let dir = temp_dir("ooo");
        write(&dir, "20260102T000000_b.sql", "-- +migrate up\nSELECT 1;\n");
        let mut runner = runner(&dir).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");

        write(&dir, "20260101T000000_a.sql", "-- +migrate up\nSELECT 1;\n");
        let connection = std::mem::replace(
            runner.connection(),
            Connection::open("sqlite::memory:")
                .await
                .expect("placeholder"),
        );
        let mut merged = Runner::with_connection(&dir, connection).expect("reads");

        let error = merged
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("out of order");
        assert!(
            error.to_string().contains("--allow-out-of-order"),
            "{error}"
        );

        let report = merged
            .migrate(&RunnerOptions::default().allow_out_of_order())
            .await
            .expect("allowed");
        assert_eq!(report.applied().len(), 1);
        merged.close().await;
    }

    #[tokio::test]
    async fn rollback_undoes_the_last_migration() {
        let dir = temp_dir("rollback-one");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n-- +migrate down\nDROP TABLE a;\n",
        );
        let mut runner = runner(&dir).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");

        let undone = runner.rollback(1).await.expect("rolls back");
        assert_eq!(undone.len(), 1);
        assert!(runner.status().await.expect("status").applied().is_empty());
        assert_eq!(
            runner
                .connection()
                .count_rows("SELECT name FROM sqlite_master WHERE name = 'a'")
                .await
                .expect("counts"),
            0
        );
        runner.close().await;
    }

    #[tokio::test]
    async fn an_irreversible_migration_refuses_to_roll_back() {
        let dir = temp_dir("irreversible");
        write(&dir, "20260101T000000_a.sql", "-- +migrate up\nSELECT 1;\n");
        let mut runner = runner(&dir).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");
        let error = runner.rollback(1).await.expect_err("no down section");
        assert!(error.to_string().contains("+migrate down"), "{error}");
        runner.close().await;
    }

    #[tokio::test]
    async fn a_dry_run_touches_nothing() {
        let dir = temp_dir("dry");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer);\n",
        );
        let mut runner = runner(&dir).await;
        let report = runner
            .migrate(&RunnerOptions::default().dry_run())
            .await
            .expect("dry run");
        assert_eq!(report.applied().len(), 1);
        assert!(report.was_dry_run());
        assert!(runner.status().await.expect("status").applied().is_empty());
        runner.close().await;
    }

    #[tokio::test]
    async fn migrate_to_stops_at_the_target() {
        let dir = temp_dir("target");
        write(&dir, "20260101T000000_a.sql", "-- +migrate up\nSELECT 1;\n");
        write(&dir, "20260102T000000_b.sql", "-- +migrate up\nSELECT 1;\n");
        write(&dir, "20260103T000000_c.sql", "-- +migrate up\nSELECT 1;\n");

        let mut runner = runner(&dir).await;
        let report = runner
            .migrate(&RunnerOptions::default().up_to(Version::from_parts(2026, 1, 2, 0, 0, 0)))
            .await
            .expect("runs");
        assert_eq!(report.applied().len(), 2);
        assert_eq!(runner.status().await.expect("status").pending().len(), 1);
        runner.close().await;
    }

    #[tokio::test]
    async fn a_missing_file_for_an_applied_migration_is_reported() {
        let dir = temp_dir("missing");
        write(&dir, "20260101T000000_a.sql", "-- +migrate up\nSELECT 1;\n");
        let mut runner = runner(&dir).await;
        runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");

        std::fs::remove_file(dir.join("20260101T000000_a.sql")).expect("removes");
        let connection = std::mem::replace(
            runner.connection(),
            Connection::open("sqlite::memory:")
                .await
                .expect("placeholder"),
        );
        let mut without = Runner::with_connection(&dir, connection).expect("reads");
        let error = without
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("missing");
        assert!(error.to_string().contains("recorded as applied"), "{error}");
        without.close().await;
    }

    #[tokio::test]
    async fn two_files_with_one_version_are_refused_at_load() {
        let dir = temp_dir("duplicate");
        write(&dir, "20260101T000000_a.sql", "-- +migrate up\nSELECT 1;\n");
        write(&dir, "20260101T000000_b.sql", "-- +migrate up\nSELECT 1;\n");
        let connection = Connection::open("sqlite::memory:").await.expect("opens");
        let error = Runner::with_connection(&dir, connection).expect_err("duplicate");
        assert!(error.to_string().contains("share the version"), "{error}");
    }

    #[tokio::test]
    async fn a_rust_migration_runs_alongside_the_sql_ones() {
        use futures_util::future::BoxFuture;

        struct Seed;
        impl RustMigration for Seed {
            fn version(&self) -> Version {
                Version::from_parts(2026, 1, 2, 0, 0, 0)
            }
            fn name(&self) -> &str {
                "seed"
            }
            fn is_reversible(&self) -> bool {
                true
            }
            fn up<'a>(&'a self, m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    m.execute("INSERT INTO a (id) VALUES (1)").await?;
                    Ok(())
                })
            }
            fn down<'a>(&'a self, m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    m.execute("DELETE FROM a").await?;
                    Ok(())
                })
            }
        }

        let dir = temp_dir("rust");
        write(
            &dir,
            "20260101T000000_a.sql",
            "-- +migrate up\nCREATE TABLE a (id integer primary key);\n\
             -- +migrate down\nDROP TABLE a;\n",
        );
        let mut runner = runner(&dir).await;
        runner.register(Seed);

        let report = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect("runs");
        assert_eq!(report.applied().len(), 2);
        assert_eq!(
            runner
                .connection()
                .count_rows("SELECT * FROM a")
                .await
                .expect("counts"),
            1
        );

        runner.rollback(1).await.expect("rolls back the rust one");
        assert_eq!(
            runner
                .connection()
                .count_rows("SELECT * FROM a")
                .await
                .expect("counts"),
            0
        );
        runner.close().await;
    }

    #[tokio::test]
    async fn a_sqlite_rebuild_that_drops_a_column_is_gated_like_any_other_drop() {
        use crate::generator::Generator;
        use crate::rename::DropAndAdd;
        use crate::schema::{Check, Column, Schema, Table};
        use moso_sql::DataType;

        fn users(legacy: bool, checked: bool) -> Schema {
            let mut table = Table::new("users").for_entity("User");
            table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
            table.add_column(Column::new("email", DataType::Text).for_field("email"));
            if legacy {
                table.add_column(
                    Column::new("legacy_id", DataType::Integer)
                        .nullable()
                        .for_field("legacy_id"),
                );
            }
            table.set_primary_key(["id"]);
            if checked {
                // A check constraint is one of the changes SQLite cannot make
                // in place, so it is what forces the rebuild.
                table.add_check(Check::new("users_id_positive", "id > 0"));
            }
            let mut schema = Schema::empty();
            schema.add_table(table);
            schema
        }

        let dir = temp_dir("rebuild-gate");
        // The first migration builds the table the second one rebuilds.
        let create = Generator::new(&dir, Backend::Sqlite)
            .at(Version::from_parts(2026, 1, 1, 0, 0, 0))
            .make_migration_between(&Schema::empty(), &users(true, false), None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        create.write().expect("writes");

        let rebuild = Generator::new(&dir, Backend::Sqlite)
            .at(Version::from_parts(2026, 1, 2, 0, 0, 0))
            .make_migration_between(&users(true, false), &users(false, true), None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        assert!(
            rebuild.migration().contains("-- moso:destructive"),
            "a rebuild that drops a column is a destructive migration:\n{}",
            rebuild.migration()
        );
        assert!(
            rebuild.migration().contains("-- +migrate destructive"),
            "{}",
            rebuild.migration()
        );
        rebuild.write().expect("writes");

        let mut runner = runner(&dir).await;
        let error = runner
            .migrate(&RunnerOptions::default())
            .await
            .expect_err("the drop is not acknowledged");
        assert!(error.to_string().contains("--allow-destructive"), "{error}");

        // The gate is checked before the lock, so nothing at all ran — not even
        // the migration that creates the table.
        assert!(runner.status().await.expect("status").applied().is_empty());

        let report = runner
            .migrate(&RunnerOptions::default().allow_destructive())
            .await
            .expect("acknowledged");
        assert_eq!(report.applied().len(), 2);

        let columns = runner
            .connection()
            .fetch_text("PRAGMA table_info(users)")
            .await
            .expect("reads the table back");
        let names: Vec<String> = columns
            .iter()
            .filter_map(|row| row.get(1).cloned().flatten())
            .collect();
        assert_eq!(names, ["id", "email"], "the rebuild dropped the column");
        runner.close().await;
    }

    #[test]
    fn the_foreign_key_check_is_recognised() {
        assert!(is_foreign_key_check("PRAGMA foreign_key_check"));
        assert!(is_foreign_key_check("  pragma FOREIGN_KEY_CHECK ; "));
        assert!(!is_foreign_key_check("PRAGMA foreign_keys = on"));
    }

    #[test]
    fn production_is_recognised_by_several_spellings() {
        for profile in ["production", "prod", "live"] {
            assert!(RunnerOptions::default().profile(profile).is_production());
        }
        for profile in ["dev", "test", "staging"] {
            assert!(!RunnerOptions::default().profile(profile).is_production());
        }
    }

    #[test]
    fn the_profile_gates_the_flag_and_nothing_else() {
        // Every combination, so that "the profile is consulted" means one
        // documented thing rather than an unwritten set of them.
        assert!(RunnerOptions::default().guard_profile().is_ok());
        assert!(
            RunnerOptions::default()
                .allow_destructive()
                .guard_profile()
                .is_ok()
        );
        assert!(
            RunnerOptions::default()
                .profile("production")
                .guard_profile()
                .is_ok()
        );
        assert!(
            RunnerOptions::default()
                .profile("production")
                .allow_out_of_order()
                .up_to(Version::from_parts(2026, 1, 1, 0, 0, 0))
                .guard_profile()
                .is_ok(),
            "only `allow_destructive` is refused"
        );
        assert!(
            RunnerOptions::default()
                .profile("prod")
                .allow_destructive()
                .guard_profile()
                .is_err()
        );
    }
}
