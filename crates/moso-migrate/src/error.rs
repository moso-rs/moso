//! What can go wrong between an entity graph and a migrated database.
//!
//! Every variant follows the style guide in `docs/04-devex/41-diagnostics.md`:
//! it names the thing that is wrong in the user's vocabulary (a table, a
//! column, a file on disk), and it gives a fix as something the reader can
//! paste. A migration error is read by someone who is either about to deploy or
//! in the middle of failing to, so "invalid state" is not an acceptable
//! message.

use std::path::PathBuf;

use crate::version::Version;

/// The result type used throughout this crate.
///
/// ```
/// use moso_migrate::Result;
///
/// fn table_count(schema: &moso_migrate::Schema) -> Result<usize> {
///     Ok(schema.tables().count())
/// }
/// # let _ = table_count(&moso_migrate::Schema::empty());
/// ```
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Anything the migration system can refuse to do.
///
/// ```
/// use moso_migrate::Error;
///
/// let error = Error::Destructive {
///     file: "20260729T101500_drop_legacy.sql".to_owned(),
///     operations: vec!["ALTER TABLE users DROP COLUMN legacy_id".to_owned()],
/// };
/// assert!(error.to_string().contains("--allow-destructive"));
/// ```
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A statement could not be rendered for the target dialect.
    #[error(transparent)]
    Sql(#[from] moso_sql::Error),

    /// An identifier coming out of a live database, a snapshot file or a
    /// `--rename` argument is not a legal SQL identifier.
    #[error(transparent)]
    Ident(#[from] moso_sql::IdentError),

    /// Reading or writing something under `migrations/` failed.
    #[error("{action} `{}` failed: {source}\nhelp: {help}", path.display())]
    Io {
        /// What was being attempted, as a verb phrase: `"reading"`.
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// What to do about it.
        help: &'static str,
        /// The underlying failure.
        #[source]
        source: std::io::Error,
    },

    /// `migrations/.schema.json` is not a snapshot this build understands.
    #[error(
        "`{}` is not a schema snapshot this version of Moso can read: {reason}\n\
         help: regenerate it with `moso db make-migration --rebuild-snapshot`, or check out the \
         branch that wrote it",
        path.display()
    )]
    Snapshot {
        /// The snapshot file.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
    },

    /// A migration file on disk does not follow the format.
    #[error(
        "`{}` is not a well-formed migration: {reason}\n\
         help: {help}",
        path.display()
    )]
    MalformedMigration {
        /// The file.
        path: PathBuf,
        /// Why it was rejected.
        reason: String,
        /// The fix.
        help: String,
    },

    /// Two files claim the same version.
    #[error(
        "two migrations share the version `{version}`:\n  {first}\n  {second}\n\
         help: rename one of them — versions are timestamps precisely so that two branches do not \
         collide, so this is normally a bad merge"
    )]
    DuplicateVersion {
        /// The colliding version.
        version: Version,
        /// The first file's name.
        first: String,
        /// The second file's name.
        second: String,
    },

    /// An applied migration's file has changed since it ran.
    #[error(
        "`{name}` has changed since it was applied\n\
         recorded checksum: {recorded}\n\
         file checksum:     {actual}\n\
         help: an applied migration is history and cannot be edited — revert the file and write a \
         new migration for the change you wanted\n\
         help: if you are certain the edit is cosmetic, `moso db repair --version {version}` \
         rewrites the recorded checksum"
    )]
    ChecksumMismatch {
        /// The migration's version.
        version: Version,
        /// The migration's name.
        name: String,
        /// What the ledger says.
        recorded: String,
        /// What the file hashes to now.
        actual: String,
    },

    /// A migration is recorded as applied and its file is gone.
    #[error(
        "migration `{version}` is recorded as applied and its file is missing\n\
         help: check out the commit that contains `{version}_*.sql`, or `moso db repair \
         --forget {version}` if it was deliberately deleted"
    )]
    MissingFile {
        /// The version with no file.
        version: Version,
    },

    /// The database is in the dirty state left by a failed non-transactional
    /// migration.
    #[error(
        "migration `{version}` ({name}) failed part-way through and left the database dirty\n\
         it ran outside a transaction, so some of its statements may have taken effect\n\
         failed at statement {statement} of {total}: {sql}\n\
         help: inspect the database, finish or undo that statement by hand, then \
         `moso db repair --resolve {version}` to clear the flag\n\
         help: `moso db status` prints the same information at any time"
    )]
    Dirty {
        /// The dirty version.
        version: Version,
        /// Its name.
        name: String,
        /// The one-based index of the statement that failed.
        statement: usize,
        /// How many statements the migration has.
        total: usize,
        /// The statement itself.
        sql: String,
    },

    /// The migration contains destructive operations that nobody has
    /// acknowledged.
    #[error(
        "`{file}` contains {} destructive operation(s) that are still commented out:\n{}\n\
         help: uncomment them once you have confirmed no running version of the application \
         still uses what they remove\n\
         help: or run `moso db migrate --allow-destructive` to apply them as written",
        operations.len(),
        operations.iter().map(|op| format!("  {op}")).collect::<Vec<_>>().join("\n")
    )]
    Destructive {
        /// The migration file.
        file: String,
        /// The operations, as SQL.
        operations: Vec<String>,
    },

    /// A destructive block is a template a human has to finish, so no flag can
    /// apply it.
    ///
    /// `allow_destructive` says "run the statements as written". A block with
    /// no statements in it — the enum-rewrite template, which PostgreSQL cannot
    /// express as `ALTER TYPE` — has nothing to run, so honouring the flag would
    /// record the migration as applied having changed nothing.
    #[error(
        "`{file}` contains {} change(s) that `--allow-destructive` cannot apply:\n{}\n\
         help: each is a template, not a statement — Moso cannot write the SQL because the \
         replacement value for every affected row is a decision about your data\n\
         help: write the statements between `-- +migrate destructive` and `-- +migrate end` \
         without the leading `--`; that is what the runner runs",
        reasons.len(),
        reasons.iter().map(|reason| format!("  {reason}")).collect::<Vec<_>>().join("\n")
    )]
    ManualMigrationRequired {
        /// The migration file.
        file: String,
        /// One line per block, in the words the file uses.
        reasons: Vec<String>,
    },

    /// The migration lock was taken away while this process still held it.
    ///
    /// Only reachable on SQLite, where the lock is a leased row rather than a
    /// session-scoped advisory lock: a run that stalls past its lease has its
    /// row reaped, and another migrator may already be applying the same files.
    #[error(
        "the migration lock was reaped after {held_secs}s: this run stalled past its lease and \
         another process may now be migrating\n\
         help: check `moso db status` before running anything else — a second migrator may have \
         applied some of these files\n\
         help: raise the lease with `RunnerOptions::lock_lease(..)` in your `src/db.rs` for a \
         migration that is genuinely this slow"
    )]
    LockLost {
        /// How long this process had held the lock when it lost it.
        held_secs: u64,
    },

    /// A pending migration sorts before one that is already applied.
    #[error(
        "migration `{version}` sorts before `{applied}`, which is already applied\n\
         this usually means a branch was merged after another branch's migration ran\n\
         help: check that `{version}` does not conflict with what has already been applied, then \
         `moso db migrate --allow-out-of-order`"
    )]
    OutOfOrder {
        /// The pending migration.
        version: Version,
        /// The applied migration it sorts before.
        applied: Version,
    },

    /// The generator needs a human decision it was not given.
    #[error(
        "{question}\n\
         help: answer it non-interactively with `{flag}`\n\
         help: or run `moso db make-migration` from a terminal, where it will ask"
    )]
    NeedsAnswer {
        /// The question, phrased for a human.
        question: String,
        /// The flag that answers it.
        flag: String,
    },

    /// A change is not expressible on the target dialect at all.
    #[error(
        "{backend} cannot {operation}\n\
         help: {help}"
    )]
    Unsupported {
        /// The backend that cannot do it.
        backend: &'static str,
        /// The operation, in the user's words.
        operation: String,
        /// The alternative.
        help: String,
    },

    /// The live database does not match the snapshot.
    #[error("{0}")]
    Drift(Box<crate::check::Drift>),

    /// The database refused a statement, or the connection failed.
    #[error("{context}: {source}\nhelp: {help}")]
    Database {
        /// What was being done when it failed.
        context: String,
        /// The fix, when there is a generic one.
        help: String,
        /// The driver's error.
        #[source]
        source: Box<sqlx::Error>,
    },

    /// Another process holds the migration lock and did not release it in time.
    #[error(
        "another process has been migrating for {waited_secs}s and still holds the lock\n\
         help: that process is either slow or dead — `moso db status` on it, or wait\n\
         help: raise the wait with `RunnerOptions::lock_wait(..)` in your `src/db.rs`; on SQLite a \
         dead holder's row is reaped once its lease runs out"
    )]
    LockTimeout {
        /// How long this process waited.
        waited_secs: u64,
    },

    /// A destructive command was aimed at production.
    #[error(
        "`{command}` is refused in the `{profile}` profile\n\
         help: {help}"
    )]
    RefusedInProduction {
        /// The command.
        command: &'static str,
        /// The active profile.
        profile: String,
        /// The escape hatch, if there is one.
        help: &'static str,
    },

    /// A Rust migration returned an error of its own.
    #[error("migration `{version}` failed: {message}")]
    MigrationFailed {
        /// The version.
        version: Version,
        /// What the migration said.
        message: String,
    },
}

impl Error {
    /// Wraps an I/O failure with the path and the verb.
    ///
    /// ```
    /// use moso_migrate::Error;
    /// use std::io;
    ///
    /// let error = Error::io("reading", "migrations/.schema.json", "check the path", io::Error::other("nope"));
    /// assert!(error.to_string().contains(".schema.json"));
    /// ```
    #[must_use]
    pub fn io(
        action: &'static str,
        path: impl Into<PathBuf>,
        help: &'static str,
        source: std::io::Error,
    ) -> Self {
        Self::Io {
            action,
            path: path.into(),
            help,
            source,
        }
    }

    /// Wraps a driver failure with the context that makes it readable.
    ///
    /// ```
    /// use moso_migrate::Error;
    ///
    /// let error = Error::database("creating moso_migrations", "check the connection", sqlx::Error::PoolClosed);
    /// assert!(error.to_string().starts_with("creating moso_migrations"));
    /// ```
    #[must_use]
    pub fn database(
        context: impl Into<String>,
        help: impl Into<String>,
        source: sqlx::Error,
    ) -> Self {
        Self::Database {
            context: context.into(),
            help: help.into(),
            source: Box::new(source),
        }
    }

    /// Whether the error is the developer's mistake rather than the operator's.
    ///
    /// The CLI uses it to decide whether to print a stack of context or a
    /// single line: a malformed migration file is something you fix in an
    /// editor, a lock timeout is something you fix by waiting.
    ///
    /// ```
    /// use moso_migrate::Error;
    ///
    /// assert!(Error::LockTimeout { waited_secs: 30 }.is_operational());
    /// ```
    #[must_use]
    pub const fn is_operational(&self) -> bool {
        matches!(
            self,
            Self::Database { .. }
                | Self::LockTimeout { .. }
                | Self::LockLost { .. }
                | Self::Dirty { .. }
                | Self::Io { .. }
                | Self::RefusedInProduction { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destructive_names_the_flag() {
        let error = Error::Destructive {
            file: "0001_x.sql".to_owned(),
            operations: vec!["DROP TABLE users".to_owned()],
        };
        let text = error.to_string();
        assert!(text.contains("DROP TABLE users"), "{text}");
        assert!(text.contains("--allow-destructive"), "{text}");
    }

    #[test]
    fn dirty_names_the_failing_statement() {
        let error = Error::Dirty {
            version: Version::from_parts(2026, 7, 29, 10, 15, 0),
            name: "backfill".to_owned(),
            statement: 3,
            total: 7,
            sql: "CREATE INDEX CONCURRENTLY i ON t (c)".to_owned(),
        };
        let text = error.to_string();
        assert!(text.contains("statement 3 of 7"), "{text}");
        assert!(text.contains("repair --resolve"), "{text}");
    }

    #[test]
    fn every_message_offers_a_fix() {
        let cases: Vec<Error> = vec![
            Error::LockTimeout { waited_secs: 30 },
            Error::MissingFile {
                version: Version::from_parts(2026, 1, 1, 0, 0, 0),
            },
            Error::OutOfOrder {
                version: Version::from_parts(2026, 1, 1, 0, 0, 0),
                applied: Version::from_parts(2026, 2, 1, 0, 0, 0),
            },
            Error::NeedsAnswer {
                question: "renamed?".to_owned(),
                flag: "--rename a:b".to_owned(),
            },
        ];
        for case in cases {
            assert!(case.to_string().contains("help:"), "{case}");
        }
    }

    #[test]
    fn a_manual_block_says_the_flag_will_not_do() {
        let error = Error::ManualMigrationRequired {
            file: "20260729T101500_rewrite_user_role.sql".to_owned(),
            reasons: vec!["rewrite the type `user_role`".to_owned()],
        };
        let text = error.to_string();
        assert!(
            text.contains("`--allow-destructive` cannot apply"),
            "{text}"
        );
        assert!(text.contains("without the leading `--`"), "{text}");
    }

    #[test]
    fn a_reaped_lock_warns_that_someone_else_may_be_migrating() {
        let text = Error::LockLost { held_secs: 930 }.to_string();
        assert!(
            text.contains("another process may now be migrating"),
            "{text}"
        );
        assert!(text.contains("lock_lease"), "{text}");
    }

    #[test]
    fn operational_errors_are_classified() {
        assert!(Error::LockTimeout { waited_secs: 1 }.is_operational());
        assert!(
            !Error::DuplicateVersion {
                version: Version::from_parts(2026, 1, 1, 0, 0, 0),
                first: "a".to_owned(),
                second: "b".to_owned(),
            }
            .is_operational()
        );
    }
}
