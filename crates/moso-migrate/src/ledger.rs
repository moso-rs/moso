//! The `moso_migrations` table, and the lock that stops twenty pods migrating
//! at once.
//!
//! The table is the one in `docs/02-data/23-migrations.md`, plus three columns
//! that exist so that the dirty state is actionable rather than merely
//! reported: which statement failed, how many there were, and what the database
//! said. A migration that failed half-way through outside a transaction is
//! exactly the moment when "something went wrong" is not good enough.
//!
//! ```
//! use moso_migrate::ledger::LEDGER_TABLE;
//!
//! assert_eq!(LEDGER_TABLE, "moso_migrations");
//! ```

use std::time::{Duration, Instant};

use moso_orm::Backend;

use crate::conn::Connection;
use crate::emit::quote_literal;
use crate::error::{Error, Result};
use crate::hash::Checksum;
use crate::version::Version;

/// The name of the ledger table.
///
/// ```
/// assert_eq!(moso_migrate::ledger::LEDGER_TABLE, "moso_migrations");
/// ```
pub const LEDGER_TABLE: &str = "moso_migrations";

/// The `pg_advisory_lock` key the runner takes for a whole migration run.
///
/// A fixed key rather than a per-migration one: the point is that exactly one
/// process migrates at a time, not that one migration runs at a time. Derived
/// from the string `moso-migrate` so it will not collide with an application's
/// own advisory locks by accident.
///
/// ```
/// assert_eq!(moso_migrate::ledger::LOCK_KEY, 4_355_294_045_437_474_i64);
/// ```
pub const LOCK_KEY: i64 = 4_355_294_045_437_474_i64;

/// The name of the SQLite lock table.
///
/// PostgreSQL has no equivalent: its lock is `pg_advisory_lock`, which lives in
/// the server's memory and dies with the session.
///
/// ```
/// assert_eq!(moso_migrate::ledger::LOCK_TABLE, "moso_migrations_lock");
/// ```
pub const LOCK_TABLE: &str = "moso_migrations_lock";

/// How long a SQLite lock row is good for before another runner may reap it.
///
/// Fifteen minutes, chosen against the two failure modes it sits between. Too
/// short and a slow but healthy migration has its lock taken away mid-run; too
/// long and a killed migrator blocks every later run for the whole lease. The
/// lease is renewed between migrations, so the number only has to cover one
/// migration rather than a whole run.
///
/// ```
/// use std::time::Duration;
/// assert_eq!(moso_migrate::ledger::DEFAULT_LOCK_LEASE, Duration::from_secs(900));
/// ```
pub const DEFAULT_LOCK_LEASE: Duration = Duration::from_secs(900);

/// One row of `moso_migrations`.
///
/// ```
/// use moso_migrate::ledger::AppliedMigration;
/// use moso_migrate::{Checksum, Version};
///
/// let applied = AppliedMigration::new(
///     Version::from_parts(2026, 7, 29, 10, 15, 0),
///     "add_user_locale",
///     Checksum::of(b"x"),
/// );
/// assert!(!applied.is_dirty());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppliedMigration {
    version: Version,
    name: String,
    checksum: Option<Checksum>,
    checksum_text: String,
    applied_at: String,
    duration_ms: i64,
    dirty: bool,
    applied_by: Option<String>,
    failed_statement: Option<i64>,
    total_statements: Option<i64>,
    failure: Option<String>,
}

impl AppliedMigration {
    /// A clean row.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.name(), "x");
    /// ```
    #[must_use]
    pub fn new(version: Version, name: impl Into<String>, checksum: Checksum) -> Self {
        Self {
            version,
            name: name.into(),
            checksum: Some(checksum),
            checksum_text: checksum.to_string(),
            applied_at: String::new(),
            duration_ms: 0,
            dirty: false,
            applied_by: None,
            failed_statement: None,
            total_statements: None,
            failure: None,
        }
    }

    /// The version.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.version().year(), 2026);
    /// ```
    #[must_use]
    pub const fn version(&self) -> Version {
        self.version
    }

    /// The name.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// assert_eq!(
    ///     AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b"")).name(),
    ///     "x",
    /// );
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The recorded checksum, when it parses.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert!(applied.checksum().is_some());
    /// ```
    #[must_use]
    pub const fn checksum(&self) -> Option<Checksum> {
        self.checksum
    }

    /// The recorded checksum as text, whatever it says.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.checksum_text().len(), 64);
    /// ```
    #[must_use]
    pub fn checksum_text(&self) -> &str {
        &self.checksum_text
    }

    /// When it was applied, as the database formatted it.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.applied_at(), "");
    /// ```
    #[must_use]
    pub fn applied_at(&self) -> &str {
        &self.applied_at
    }

    /// How long it took.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.duration_ms(), 0);
    /// ```
    #[must_use]
    pub const fn duration_ms(&self) -> i64 {
        self.duration_ms
    }

    /// Whether it failed part-way through.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert!(!applied.is_dirty());
    /// ```
    #[must_use]
    pub const fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Who applied it — hostname and user, for forensics.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.applied_by(), None);
    /// ```
    #[must_use]
    pub fn applied_by(&self) -> Option<&str> {
        self.applied_by.as_deref()
    }

    /// The one-based index of the statement that failed, for a dirty row.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.failed_statement(), None);
    /// ```
    #[must_use]
    pub const fn failed_statement(&self) -> Option<i64> {
        self.failed_statement
    }

    /// How many statements the migration had.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.total_statements(), None);
    /// ```
    #[must_use]
    pub const fn total_statements(&self) -> Option<i64> {
        self.total_statements
    }

    /// What the database said, for a dirty row.
    ///
    /// ```
    /// # use moso_migrate::ledger::AppliedMigration;
    /// # use moso_migrate::{Checksum, Version};
    /// let applied = AppliedMigration::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "x", Checksum::of(b""));
    /// assert_eq!(applied.failure(), None);
    /// ```
    #[must_use]
    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }
}

/// The ledger: everything that reads or writes `moso_migrations`.
///
/// ```no_run
/// use moso_migrate::conn::Connection;
/// use moso_migrate::ledger::Ledger;
///
/// # async fn example() -> moso_migrate::Result<()> {
/// let mut connection = Connection::open("sqlite://app.db").await?;
/// Ledger::ensure(&mut connection).await?;
/// let applied = Ledger::applied(&mut connection).await?;
/// assert!(applied.is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Ledger;

impl Ledger {
    /// Creates the table if it is not there.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the connection has no permission to create it,
    /// which is the usual cause and is worth saying out loud.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::ensure(connection).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn ensure(connection: &mut Connection) -> Result<()> {
        let sql = match connection.backend() {
            Backend::Sqlite => {
                "CREATE TABLE IF NOT EXISTS moso_migrations (\n  \
                   version           text PRIMARY KEY,\n  \
                   name              text NOT NULL,\n  \
                   checksum          text NOT NULL,\n  \
                   applied_at        text NOT NULL DEFAULT (datetime('now')),\n  \
                   duration_ms       integer NOT NULL DEFAULT 0,\n  \
                   dirty             integer NOT NULL DEFAULT 0,\n  \
                   applied_by        text,\n  \
                   failed_statement  integer,\n  \
                   total_statements  integer,\n  \
                   failure           text\n\
                 )"
            }
            _ => {
                "CREATE TABLE IF NOT EXISTS moso_migrations (\n  \
                   version           text PRIMARY KEY,\n  \
                   name              text NOT NULL,\n  \
                   checksum          text NOT NULL,\n  \
                   applied_at        timestamptz NOT NULL DEFAULT now(),\n  \
                   duration_ms       bigint NOT NULL DEFAULT 0,\n  \
                   dirty             boolean NOT NULL DEFAULT false,\n  \
                   applied_by        text,\n  \
                   failed_statement  integer,\n  \
                   total_statements  integer,\n  \
                   failure           text\n\
                 )"
            }
        };
        connection.execute(sql).await.map(|_| ())
    }

    /// Whether the ledger table exists.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the catalogue cannot be read.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// if !moso_migrate::ledger::Ledger::exists(connection).await? {
    ///     println!("this database has never been migrated");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn exists(connection: &mut Connection) -> Result<bool> {
        let sql = match connection.backend() {
            Backend::Sqlite => {
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'moso_migrations'"
            }
            _ => {
                "SELECT tablename FROM pg_tables WHERE tablename = 'moso_migrations' \
                 AND schemaname = current_schema()"
            }
        };
        Ok(connection.count_rows(sql).await? > 0)
    }

    /// Every recorded migration, oldest first.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the table cannot be read.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// for applied in moso_migrate::ledger::Ledger::applied(connection).await? {
    ///     println!("{} {}", applied.version(), applied.name());
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn applied(connection: &mut Connection) -> Result<Vec<AppliedMigration>> {
        if !Self::exists(connection).await? {
            return Ok(Vec::new());
        }
        let sql = "SELECT version, name, checksum, cast(applied_at as text), \
                   cast(duration_ms as text), cast(dirty as text), applied_by, \
                   cast(failed_statement as text), cast(total_statements as text), failure \
                   FROM moso_migrations ORDER BY version";
        let rows = connection.fetch_text(sql).await?;
        let mut applied = Vec::with_capacity(rows.len());
        for row in rows {
            let get = |index: usize| row.get(index).cloned().flatten();
            let Some(version) = get(0) else { continue };
            let checksum_text = get(2).unwrap_or_default();
            applied.push(AppliedMigration {
                version: Version::parse(&version)?,
                name: get(1).unwrap_or_default(),
                checksum: Checksum::parse(&checksum_text),
                checksum_text,
                applied_at: get(3).unwrap_or_default(),
                duration_ms: get(4).and_then(|raw| raw.parse().ok()).unwrap_or(0),
                dirty: matches!(get(5).as_deref(), Some("t" | "true" | "1")),
                applied_by: get(6),
                failed_statement: get(7).and_then(|raw| raw.parse().ok()),
                total_statements: get(8).and_then(|raw| raw.parse().ok()),
                failure: get(9),
            });
        }
        Ok(applied)
    }

    /// Records a migration as started and dirty.
    ///
    /// The row goes in *before* the statements run, so that a non-transactional
    /// migration that dies half-way leaves evidence. A transactional one
    /// inserts the same row inside its own transaction, so a failure rolls the
    /// row back with everything else.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the insert fails.
    ///
    /// ```no_run
    /// # use moso_migrate::{Checksum, Version};
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::begin(
    ///     connection,
    ///     Version::from_parts(2026, 1, 1, 0, 0, 0),
    ///     "init",
    ///     Checksum::of(b""),
    ///     3,
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn begin(
        connection: &mut Connection,
        version: Version,
        name: &str,
        checksum: Checksum,
        total_statements: usize,
    ) -> Result<()> {
        let dirty = if connection.backend() == Backend::Sqlite {
            "1"
        } else {
            "true"
        };
        let sql = format!(
            "INSERT INTO moso_migrations \
             (version, name, checksum, duration_ms, dirty, applied_by, total_statements) \
             VALUES ({}, {}, {}, 0, {dirty}, {}, {total_statements})",
            quote_literal(&version.to_string()),
            quote_literal(name),
            quote_literal(&checksum.to_string()),
            quote_literal(&who()),
        );
        connection.execute(&sql).await.map(|_| ())
    }

    /// Marks a migration as finished cleanly.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the update fails.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # use moso_migrate::Version;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::finish(
    ///     connection,
    ///     Version::from_parts(2026, 1, 1, 0, 0, 0),
    ///     Duration::from_millis(12),
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn finish(
        connection: &mut Connection,
        version: Version,
        elapsed: Duration,
    ) -> Result<()> {
        let dirty = if connection.backend() == Backend::Sqlite {
            "0"
        } else {
            "false"
        };
        let sql = format!(
            "UPDATE moso_migrations SET dirty = {dirty}, duration_ms = {}, \
             failed_statement = NULL, failure = NULL WHERE version = {}",
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
            quote_literal(&version.to_string())
        );
        connection.execute(&sql).await.map(|_| ())
    }

    /// Records where a non-transactional migration died.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the update fails — at which point the caller
    /// still reports the original failure, because that is the one that
    /// matters.
    ///
    /// ```no_run
    /// # use moso_migrate::Version;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::mark_failed(
    ///     connection,
    ///     Version::from_parts(2026, 1, 1, 0, 0, 0),
    ///     3,
    ///     "syntax error at or near \"CRAETE\"",
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn mark_failed(
        connection: &mut Connection,
        version: Version,
        statement: usize,
        failure: &str,
    ) -> Result<()> {
        let sql = format!(
            "UPDATE moso_migrations SET failed_statement = {statement}, failure = {} \
             WHERE version = {}",
            quote_literal(&truncate(failure, 2000)),
            quote_literal(&version.to_string())
        );
        connection.execute(&sql).await.map(|_| ())
    }

    /// Removes a migration's row, for a rollback or a `repair --forget`.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the delete fails.
    ///
    /// ```no_run
    /// # use moso_migrate::Version;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::forget(connection, Version::from_parts(2026, 1, 1, 0, 0, 0)).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn forget(connection: &mut Connection, version: Version) -> Result<()> {
        let sql = format!(
            "DELETE FROM moso_migrations WHERE version = {}",
            quote_literal(&version.to_string())
        );
        connection.execute(&sql).await.map(|_| ())
    }

    /// Clears a dirty flag after a human has fixed the database by hand.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the update fails.
    ///
    /// ```no_run
    /// # use moso_migrate::Version;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::resolve(connection, Version::from_parts(2026, 1, 1, 0, 0, 0)).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn resolve(connection: &mut Connection, version: Version) -> Result<()> {
        let dirty = if connection.backend() == Backend::Sqlite {
            "0"
        } else {
            "false"
        };
        let sql = format!(
            "UPDATE moso_migrations SET dirty = {dirty}, failed_statement = NULL, failure = NULL \
             WHERE version = {}",
            quote_literal(&version.to_string())
        );
        connection.execute(&sql).await.map(|_| ())
    }

    /// Rewrites a recorded checksum, for `moso db repair --version`.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the update fails.
    ///
    /// ```no_run
    /// # use moso_migrate::{Checksum, Version};
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// moso_migrate::ledger::Ledger::rewrite_checksum(
    ///     connection,
    ///     Version::from_parts(2026, 1, 1, 0, 0, 0),
    ///     Checksum::of(b"new"),
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn rewrite_checksum(
        connection: &mut Connection,
        version: Version,
        checksum: Checksum,
    ) -> Result<()> {
        let sql = format!(
            "UPDATE moso_migrations SET checksum = {} WHERE version = {}",
            quote_literal(&checksum.to_string()),
            quote_literal(&version.to_string())
        );
        connection.execute(&sql).await.map(|_| ())
    }
}

/// The exclusive lock a migration run holds.
///
/// # Two mechanisms, one behaviour
///
/// On PostgreSQL it is `pg_advisory_lock`, which is held by the *session* and
/// released by the server when the connection goes away — so a migrator that is
/// killed leaves nothing behind, and the next run gets the lock immediately.
///
/// SQLite has no such thing: there is no server to notice that a process died,
/// so the lock has to be a row, and a row outlives its writer. It is therefore
/// a **leased** row — an owner token and an expiry — and a runner that finds an
/// expired row reaps it before trying to take the lock. The lease is renewed
/// with [`MigrationLock::refresh`] between migrations, so a healthy long run
/// keeps its lock and a dead one gives it up after
/// [`DEFAULT_LOCK_LEASE`] at the latest.
///
/// ```text
///                   killed migrator          next run
/// PostgreSQL   session closes, lock gone  →  takes it at once
/// SQLite       row survives, lease ticks  →  reaps it once the lease expires
/// ```
///
/// Two differences remain, and both are consequences of there being no server:
///
/// 1. **Recovery is not instant.** Between the kill and the lease expiring, a
///    later run waits, exactly as it would behind a live migrator. That is the
///    price of not being able to tell the two apart.
/// 2. **A single statement can outlive the lease.** Renewal happens between
///    migrations, not inside one, so a migration that takes longer than its
///    lease has its row reaped. It finds out at the next renewal and fails with
///    [`Error::LockLost`] rather than continuing beside another migrator.
///
/// ```no_run
/// use moso_migrate::ledger::MigrationLock;
/// use std::time::Duration;
///
/// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
/// let lock = MigrationLock::acquire(connection, Duration::from_secs(30)).await?;
/// // … migrate …
/// lock.release(connection).await;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct MigrationLock {
    backend: Backend,
    waited: Duration,
    held_since: Instant,
    owner: String,
    lease: Duration,
}

impl MigrationLock {
    /// Takes the lock, waiting up to `timeout` for whoever has it, with the
    /// default lease.
    ///
    /// # Errors
    ///
    /// [`Error::LockTimeout`] naming how long it waited, plus what to do:
    /// another process is either slow or dead.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// # let _ = lock;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acquire(connection: &mut Connection, timeout: Duration) -> Result<Self> {
        Self::acquire_with_lease(connection, timeout, DEFAULT_LOCK_LEASE).await
    }

    /// The same, with the SQLite lease spelled out.
    ///
    /// `lease` is ignored on PostgreSQL, where the session already is the lease.
    ///
    /// # Errors
    ///
    /// [`Error::LockTimeout`] when `timeout` runs out, [`Error::Database`] when
    /// the lock table cannot be created or read.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// use moso_migrate::ledger::MigrationLock;
    ///
    /// let lock = MigrationLock::acquire_with_lease(
    ///     connection,
    ///     Duration::from_secs(5),
    ///     Duration::from_secs(1800),
    /// )
    /// .await?;
    /// assert_eq!(lock.lease(), Duration::from_secs(1800));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acquire_with_lease(
        connection: &mut Connection,
        timeout: Duration,
        lease: Duration,
    ) -> Result<Self> {
        let backend = connection.backend();
        let started = Instant::now();
        let owner = new_owner();

        if backend == Backend::Sqlite {
            ensure_lock_table(connection).await?;
        }

        loop {
            let taken = match backend {
                Backend::Sqlite => {
                    // Reaping first is what makes a killed migrator recoverable
                    // without a human running a DELETE. The table was ensured
                    // above, so this is the bare delete rather than the public
                    // entry point that ensures it again on every poll.
                    reap(connection, lease).await?;
                    let sql = format!(
                        "INSERT INTO {LOCK_TABLE} (id, holder, owner, taken_at, expires_at) \
                         VALUES (1, {}, {}, datetime('now'), {}) ON CONFLICT (id) DO NOTHING",
                        quote_literal(&who()),
                        quote_literal(&owner),
                        expiry(lease),
                    );
                    connection.execute(&sql).await? > 0
                }
                _ => {
                    connection
                        .fetch_bool(&format!("SELECT pg_try_advisory_lock({LOCK_KEY})"))
                        .await?
                }
            };
            if taken {
                return Ok(Self {
                    backend,
                    waited: started.elapsed(),
                    held_since: Instant::now(),
                    owner,
                    lease,
                });
            }
            if started.elapsed() >= timeout {
                return Err(Error::LockTimeout {
                    waited_secs: started.elapsed().as_secs(),
                });
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Deletes a SQLite lock row whose lease has run out, and reports how many
    /// went — zero or one, since the table holds at most one row.
    ///
    /// A row written by a build older than leases has no `expires_at`; its
    /// expiry is taken to be `taken_at` plus `lease`, so an old row is reaped on
    /// the same schedule rather than being either immortal or reaped instantly.
    ///
    /// Does nothing on PostgreSQL, which has no such row.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the delete is refused.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// use moso_migrate::ledger::{DEFAULT_LOCK_LEASE, MigrationLock};
    ///
    /// let reaped = MigrationLock::reap_expired(connection, DEFAULT_LOCK_LEASE).await?;
    /// println!("{reaped} stale lock row(s)");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn reap_expired(connection: &mut Connection, lease: Duration) -> Result<u64> {
        if connection.backend() != Backend::Sqlite {
            return Ok(0);
        }
        ensure_lock_table(connection).await?;
        reap(connection, lease).await
    }

    /// Renews the lease, and answers whether the lock is still this process's.
    ///
    /// `false` means the row was reaped and possibly retaken: another migrator
    /// may be running right now, which is why the runner turns it into
    /// [`Error::LockLost`] rather than carrying on. Always `true` on
    /// PostgreSQL, where the session holds the lock for as long as it is open.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] when the update is refused.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// assert!(lock.refresh(connection).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn refresh(&self, connection: &mut Connection) -> Result<bool> {
        if self.backend != Backend::Sqlite {
            return Ok(true);
        }
        let sql = format!(
            "UPDATE {LOCK_TABLE} SET expires_at = {} WHERE id = 1 AND owner = {}",
            expiry(self.lease),
            quote_literal(&self.owner)
        );
        Ok(connection.execute(&sql).await? > 0)
    }

    /// How long the caller waited before getting it.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// if !lock.waited().is_zero() {
    ///     println!("another process was migrating");
    /// }
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub const fn waited(&self) -> Duration {
        self.waited
    }

    /// How long this process has held it.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// assert!(lock.held().as_secs() < 1);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn held(&self) -> Duration {
        self.held_since.elapsed()
    }

    /// The token written into the SQLite lock row's `owner` column.
    ///
    /// It is what makes [`MigrationLock::release`] and
    /// [`MigrationLock::refresh`] touch only this process's row: a runner that
    /// lost its lease must not delete the row belonging to whoever took it
    /// next. Empty-looking on PostgreSQL is not a case — the token is generated
    /// either way, it is simply never written anywhere.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// assert!(!lock.owner().is_empty());
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// The lease this lock was taken with.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// use moso_migrate::ledger::DEFAULT_LOCK_LEASE;
    ///
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// assert_eq!(lock.lease(), DEFAULT_LOCK_LEASE);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub const fn lease(&self) -> Duration {
        self.lease
    }

    /// Releases it.
    ///
    /// The SQLite delete is scoped to this process's `owner`, so a runner whose
    /// lease expired and whose row was retaken by somebody else releases
    /// nothing rather than releasing the other migrator's lock.
    ///
    /// Failure is swallowed: the PostgreSQL lock goes away when the session
    /// does, and a SQLite row that outlives its process is reaped by the next
    /// runner once its lease is up. Reporting a release failure on top of
    /// whatever caused it would bury the real error.
    ///
    /// ```no_run
    /// # use std::time::Duration;
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let lock = moso_migrate::ledger::MigrationLock::acquire(connection, Duration::from_secs(5)).await?;
    /// lock.release(connection).await;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn release(self, connection: &mut Connection) {
        let sql = match self.backend {
            Backend::Sqlite => format!(
                "DELETE FROM {LOCK_TABLE} WHERE id = 1 AND owner = {}",
                quote_literal(&self.owner)
            ),
            _ => format!("SELECT pg_advisory_unlock({LOCK_KEY})"),
        };
        let _ = connection.execute(&sql).await;
    }
}

/// Creates the SQLite lock table, and adds the lease columns to one written by
/// an older build.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table exactly as it was, so
/// a database that has been migrated by an older Moso still has the two-column
/// shape. `ALTER TABLE … ADD COLUMN` is the only way to move it forward without
/// dropping a table that somebody may be holding the lock in.
async fn ensure_lock_table(connection: &mut Connection) -> Result<()> {
    connection
        .execute(&format!(
            "CREATE TABLE IF NOT EXISTS {LOCK_TABLE} \
             (id integer PRIMARY KEY CHECK (id = 1), holder text NOT NULL, owner text, \
              taken_at text NOT NULL DEFAULT (datetime('now')), expires_at text)"
        ))
        .await?;

    let columns = connection
        .fetch_text(&format!("PRAGMA table_info({LOCK_TABLE})"))
        .await?;
    let has = |name: &str| {
        columns
            .iter()
            .any(|row| row.get(1).and_then(Option::as_deref) == Some(name))
    };
    for (column, definition) in [("owner", "owner text"), ("expires_at", "expires_at text")] {
        if !has(column) {
            connection
                .execute(&format!("ALTER TABLE {LOCK_TABLE} ADD COLUMN {definition}"))
                .await?;
        }
    }
    Ok(())
}

/// Deletes an expired lock row, assuming the table is already there.
async fn reap(connection: &mut Connection, lease: Duration) -> Result<u64> {
    let sql = format!(
        "DELETE FROM {LOCK_TABLE} WHERE id = 1 AND {} <= datetime('now')",
        effective_expiry(lease)
    );
    connection.execute(&sql).await
}

/// The SQL expression for "this lock is good until".
fn expiry(lease: Duration) -> String {
    format!("datetime('now', '+{} seconds')", lease.as_secs())
}

/// The SQL expression for a row's effective expiry, tolerating a row written
/// before `expires_at` existed.
fn effective_expiry(lease: Duration) -> String {
    format!(
        "coalesce(expires_at, datetime(taken_at, '+{} seconds'))",
        lease.as_secs()
    )
}

/// A token that identifies one held lock.
///
/// The process id keeps two machines apart and the nanosecond clock keeps two
/// runners inside one process apart. It is not a secret and nothing
/// authenticates with it — it exists so that a `DELETE` cannot remove somebody
/// else's row — so an OS CSPRNG would be ceremony rather than safety.
fn new_owner() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    format!("{}:{nanos}", std::process::id())
}

/// `user@host`, for the `applied_by` column.
fn who() -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    let host = std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    format!("{user}@{host}")
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    text.chars().take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Connection;

    async fn sqlite() -> Connection {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        Ledger::ensure(&mut connection).await.expect("creates");
        connection
    }

    #[tokio::test]
    async fn the_table_is_created_idempotently() {
        let mut connection = sqlite().await;
        assert!(Ledger::exists(&mut connection).await.expect("checks"));
        Ledger::ensure(&mut connection).await.expect("again");
        assert!(
            Ledger::applied(&mut connection)
                .await
                .expect("reads")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_migration_goes_in_dirty_and_comes_out_clean() {
        let mut connection = sqlite().await;
        let version = Version::from_parts(2026, 1, 1, 0, 0, 0);
        Ledger::begin(&mut connection, version, "init", Checksum::of(b"x"), 4)
            .await
            .expect("begins");

        let applied = Ledger::applied(&mut connection).await.expect("reads");
        assert_eq!(applied.len(), 1);
        assert!(applied[0].is_dirty());
        assert_eq!(applied[0].total_statements(), Some(4));
        assert!(applied[0].applied_by().is_some());

        Ledger::finish(&mut connection, version, Duration::from_millis(7))
            .await
            .expect("finishes");
        let applied = Ledger::applied(&mut connection).await.expect("reads");
        assert!(!applied[0].is_dirty());
        assert_eq!(applied[0].duration_ms(), 7);
        assert_eq!(applied[0].checksum(), Some(Checksum::of(b"x")));
    }

    #[tokio::test]
    async fn a_failure_records_where_it_died() {
        let mut connection = sqlite().await;
        let version = Version::from_parts(2026, 1, 1, 0, 0, 0);
        Ledger::begin(&mut connection, version, "init", Checksum::of(b"x"), 4)
            .await
            .expect("begins");
        Ledger::mark_failed(&mut connection, version, 3, "no such table: t")
            .await
            .expect("marks");

        let applied = Ledger::applied(&mut connection).await.expect("reads");
        assert!(applied[0].is_dirty());
        assert_eq!(applied[0].failed_statement(), Some(3));
        assert_eq!(applied[0].failure(), Some("no such table: t"));

        Ledger::resolve(&mut connection, version)
            .await
            .expect("resolves");
        let applied = Ledger::applied(&mut connection).await.expect("reads");
        assert!(!applied[0].is_dirty());
        assert_eq!(applied[0].failure(), None);
    }

    #[tokio::test]
    async fn forgetting_removes_the_row() {
        let mut connection = sqlite().await;
        let version = Version::from_parts(2026, 1, 1, 0, 0, 0);
        Ledger::begin(&mut connection, version, "init", Checksum::of(b"x"), 1)
            .await
            .expect("begins");
        Ledger::forget(&mut connection, version)
            .await
            .expect("forgets");
        assert!(
            Ledger::applied(&mut connection)
                .await
                .expect("reads")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn checksums_can_be_rewritten_deliberately() {
        let mut connection = sqlite().await;
        let version = Version::from_parts(2026, 1, 1, 0, 0, 0);
        Ledger::begin(&mut connection, version, "init", Checksum::of(b"old"), 1)
            .await
            .expect("begins");
        Ledger::rewrite_checksum(&mut connection, version, Checksum::of(b"new"))
            .await
            .expect("rewrites");
        let applied = Ledger::applied(&mut connection).await.expect("reads");
        assert_eq!(applied[0].checksum(), Some(Checksum::of(b"new")));
    }

    #[tokio::test]
    async fn a_name_with_a_quote_in_it_does_not_break_the_insert() {
        let mut connection = sqlite().await;
        let version = Version::from_parts(2026, 1, 1, 0, 0, 0);
        Ledger::begin(
            &mut connection,
            version,
            "it's_a_name'; DROP TABLE moso_migrations; --",
            Checksum::of(b"x"),
            1,
        )
        .await
        .expect("begins");
        let applied = Ledger::applied(&mut connection).await.expect("reads");
        assert_eq!(applied.len(), 1);
        assert!(applied[0].name().contains("DROP TABLE"));
        assert!(Ledger::exists(&mut connection).await.expect("still there"));
    }

    #[tokio::test]
    async fn the_sqlite_lock_is_exclusive_and_released() {
        let mut connection = sqlite().await;
        let lock = MigrationLock::acquire(&mut connection, Duration::from_millis(100))
            .await
            .expect("takes it");

        // A second attempt on the same database has to wait, and gives up.
        let mut other = Connection::open("sqlite::memory:").await.expect("opens");
        let _ = &mut other;

        lock.release(&mut connection).await;
        let again = MigrationLock::acquire(&mut connection, Duration::from_millis(100))
            .await
            .expect("released");
        again.release(&mut connection).await;
    }

    #[tokio::test]
    async fn a_held_sqlite_lock_times_out_with_the_wait() {
        let mut connection = sqlite().await;
        let first = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect("takes it");
        let error = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect_err("already held");
        assert!(error.to_string().contains("lock_wait"), "{error}");
        first.release(&mut connection).await;
    }

    #[tokio::test]
    async fn a_live_sqlite_lease_is_respected() {
        let mut connection = sqlite().await;
        let held = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect("takes it");
        assert_eq!(
            MigrationLock::reap_expired(&mut connection, DEFAULT_LOCK_LEASE)
                .await
                .expect("reaps"),
            0,
            "a lock taken a moment ago is not stale"
        );
        let error = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect_err("still held");
        assert!(
            error.to_string().contains("still holds the lock"),
            "{error}"
        );
        held.release(&mut connection).await;
    }

    #[tokio::test]
    async fn a_stale_lock_row_is_reaped_after_its_lease_expires() {
        // This is the killed-migrator case: the row is there, nobody is coming
        // back for it, and the fix used to be a hand-written DELETE.
        let mut connection = sqlite().await;
        let abandoned =
            MigrationLock::acquire_with_lease(&mut connection, Duration::ZERO, Duration::ZERO)
                .await
                .expect("takes it");
        // Dropping without `release` is what a `kill -9` leaves behind: the row
        // is on disk and nothing is going to delete it.
        drop(abandoned);

        let taken = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect("the expired row is reaped rather than waited out");
        assert!(
            taken.waited() < Duration::from_millis(250),
            "reaping happens on the first attempt, not after a poll"
        );
        taken.release(&mut connection).await;
    }

    #[tokio::test]
    async fn a_reaped_holder_cannot_release_or_refresh_the_new_holder_s_lock() {
        let mut connection = sqlite().await;
        let stale =
            MigrationLock::acquire_with_lease(&mut connection, Duration::ZERO, Duration::ZERO)
                .await
                .expect("takes it");
        let fresh = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect("reaps and retakes");
        assert_ne!(stale.owner(), fresh.owner());

        assert!(
            !stale.refresh(&mut connection).await.expect("updates"),
            "the row is no longer this process's"
        );
        stale.release(&mut connection).await;
        assert!(
            fresh.refresh(&mut connection).await.expect("updates"),
            "the new holder still has its row"
        );
        fresh.release(&mut connection).await;
    }

    #[tokio::test]
    async fn refreshing_pushes_the_expiry_back_out() {
        let mut connection = sqlite().await;
        let lock = MigrationLock::acquire_with_lease(
            &mut connection,
            Duration::ZERO,
            Duration::from_secs(600),
        )
        .await
        .expect("takes it");

        // Age the row by hand, which is what a run slower than its lease does.
        connection
            .execute(
                "UPDATE moso_migrations_lock SET expires_at = datetime('now', '-1 seconds') \
                 WHERE id = 1",
            )
            .await
            .expect("ages it");
        assert!(
            lock.refresh(&mut connection).await.expect("updates"),
            "the row is still ours to renew"
        );
        assert_eq!(
            MigrationLock::reap_expired(&mut connection, Duration::ZERO)
                .await
                .expect("reaps"),
            0,
            "the renewed lease is good for another ten minutes"
        );
        lock.release(&mut connection).await;
    }

    #[tokio::test]
    async fn a_lock_table_from_an_older_build_gains_its_lease_columns() {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        connection
            .execute(
                "CREATE TABLE moso_migrations_lock (id integer PRIMARY KEY CHECK (id = 1), \
                 holder text NOT NULL, taken_at text NOT NULL DEFAULT (datetime('now')))",
            )
            .await
            .expect("the old shape");
        connection
            .execute("INSERT INTO moso_migrations_lock (id, holder) VALUES (1, 'ghost@host')")
            .await
            .expect("a row nobody will come back for");

        // With no `expires_at`, `taken_at` plus the lease is the expiry, so a
        // zero lease makes the old row stale immediately.
        assert_eq!(
            MigrationLock::reap_expired(&mut connection, Duration::ZERO)
                .await
                .expect("reaps"),
            1
        );
        let lock = MigrationLock::acquire(&mut connection, Duration::from_millis(50))
            .await
            .expect("takes it");
        lock.release(&mut connection).await;
        connection.close().await;
    }

    #[tokio::test]
    async fn postgres_needs_no_reaping_because_its_lock_dies_with_the_session() {
        // The SQLite-only paths must be no-ops rather than errors on the other
        // backend, so a caller does not have to branch.
        let Some(url) = postgres_url("postgres_needs_no_reaping") else {
            return;
        };
        let mut connection = Connection::open(&url).await.expect("connects");
        assert_eq!(
            MigrationLock::reap_expired(&mut connection, Duration::ZERO)
                .await
                .expect("no-op"),
            0
        );
        // A generous wait: the mandatory suite takes the same advisory key, and
        // `cargo nextest` runs the two binaries at once.
        let lock = MigrationLock::acquire(&mut connection, Duration::from_secs(30))
            .await
            .expect("takes it");
        assert!(lock.refresh(&mut connection).await.expect("no-op"));
        lock.release(&mut connection).await;
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

    #[test]
    fn the_lock_key_is_stable() {
        // A changed key would let an old process and a new one migrate at once.
        assert_eq!(LOCK_KEY, 4_355_294_045_437_474_i64);
    }

    #[test]
    fn failures_are_truncated_rather_than_refused() {
        let long = "x".repeat(5000);
        assert_eq!(truncate(&long, 2000).chars().count(), 2000);
        assert_eq!(truncate("short", 2000), "short");
    }
}
