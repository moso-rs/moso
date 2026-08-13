//! Per-test databases: three isolation strategies, automatic cleanup, and the
//! statement-count assertion.
//!
//! # The one idea
//!
//! **Every test gets its own database, and it costs about fifty milliseconds.**
//! Sharing a database between tests is the single most common source of a suite
//! that passes alone and fails in parallel, and every workaround for it —
//! truncating between tests, serialising the suite, prefixing every fixture with
//! the test name — is worse than the disease. `43-testing.md` makes the cost low
//! enough that nobody is tempted.
//!
//! ```no_run
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! use moso_test::db::TestDb;
//!
//! let db = TestDb::acquire().await?;
//! db.execute("create table widget (id int primary key)").await?;
//! assert_eq!(db.fetch_i64("select count(*) from widget").await?, 0);
//! db.close().await;
//! # Ok(())
//! # }
//! ```
//!
//! # The three strategies
//!
//! | [`Strategy`] | How | Cost | When |
//! | --- | --- | --- | --- |
//! | [`Template`](Strategy::Template) | migrate once into a template database, then `CREATE DATABASE … TEMPLATE …` per test | ~50 ms | the default: full isolation, real DDL, parallel-safe |
//! | [`Transaction`](Strategy::Transaction) | one pinned connection per test, everything inside a transaction that is rolled back | ~5 ms | fastest, but the code under test may not commit or use a second connection |
//! | [`Migrate`](Strategy::Migrate) | a fresh database with the whole migration chain replayed | ~2 s | proving the migration chain still applies from empty |
//!
//! # Where the database comes from
//!
//! [`TestDb::acquire`] reads `DATABASE_URL`. When it is unset every constructor
//! returns [`Error::NoDatabaseUrl`], whose message names the variable and the
//! command that starts the container — so a suite still *passes* on a machine
//! without Docker if its database tests are gated:
//!
//! ```
//! # async fn example() {
//! if !moso_test::db::database_is_available() {
//!     eprintln!("{}", moso_test::db::skip_reason());
//!     return;
//! }
//! # }
//! ```
//!
//! [`skip_without_database!`](crate::skip_without_database) is the one-line
//! form. SQLite needs no environment at all: [`TestDbBuilder::sqlite`] puts a
//! private database file in the temporary directory, so the harness is useful
//! on a laptop with no server running.
//!
//! # Cleanup
//!
//! Dropping a [`TestDb`] drops its database. The work is handed to a background
//! cleaner thread so that `Drop` does not block the test, which means a process
//! killed mid-run can leave databases behind: that is what
//! [`prune_test_databases`] is for, and what `moso db prune-test` calls.
//! [`TestDb::close`] is the deterministic form, and
//! [`TestDbBuilder::keep`] (or `MOSO_TEST_KEEP_DB=1`) keeps the database and
//! prints its URL so it can be inspected after a failure.
//!
//! # Counting statements
//!
//! [`assert_queries!`](crate::assert_queries) is the N+1 guard. It brackets a
//! block, counts the statements that ran inside it, and on a mismatch prints
//! **the statements themselves**, numbered, with repeats called out:
//!
//! ```text
//! ── moso-test: assert_queries! ─────────────────────────────────────────
//!   expected exactly 2 statements, 12 ran
//!   at crates/shop/tests/posts.rs:41:5
//!
//!   statements (12):
//!      1  select "posts"."id", "posts"."title" from "posts" limit $1
//!      2  select "users".* from "users" where "users"."id" = $1
//!     ...
//!
//!   10 more statements were identical to #2 — this is an N+1
//!   help: preload the relation: `Post::query().with(Post::AUTHOR)`
//! ──────────────────────────────────────────────────────────────────────
//! ```

use std::collections::HashMap;
use std::fmt;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use moso_orm::{Backend, DatabaseConfig, Db};
use sqlx::{AssertSqlSafe, Executor as _, Row as _};

use crate::logs::{LogAssertions, LogRecord};

// ---------------------------------------------------------------------------
// Environment
// ---------------------------------------------------------------------------

/// The variable [`TestDb::acquire`] reads the server URL from.
///
/// ```
/// assert_eq!(moso_test::db::DATABASE_URL_ENV, "DATABASE_URL");
/// ```
pub const DATABASE_URL_ENV: &str = "DATABASE_URL";

/// The variable that overrides the default [`Strategy`].
///
/// ```
/// assert_eq!(moso_test::db::STRATEGY_ENV, "MOSO_TEST_STRATEGY");
/// ```
pub const STRATEGY_ENV: &str = "MOSO_TEST_STRATEGY";

/// The variable that keeps every test database for inspection.
///
/// ```
/// assert_eq!(moso_test::db::KEEP_ENV, "MOSO_TEST_KEEP_DB");
/// ```
pub const KEEP_ENV: &str = "MOSO_TEST_KEEP_DB";

/// The variable that overrides the template database's name.
///
/// ```
/// assert_eq!(moso_test::db::TEMPLATE_ENV, "MOSO_TEST_TEMPLATE");
/// ```
pub const TEMPLATE_ENV: &str = "MOSO_TEST_TEMPLATE";

/// The prefix every generated test database name carries.
///
/// [`prune_test_databases`] will not touch a database whose name does not both
/// start with this and parse as a generated name, which is what keeps a stray
/// `prune-test` from deleting production data that happens to live on the same
/// server.
///
/// ```
/// assert_eq!(moso_test::db::NAME_PREFIX, "moso_test_");
/// ```
pub const NAME_PREFIX: &str = "moso_test_";

/// The default template database name, when `DATABASE_URL` names no database.
///
/// ```
/// assert_eq!(moso_test::db::DEFAULT_TEMPLATE, "moso_test_template");
/// ```
pub const DEFAULT_TEMPLATE: &str = "moso_test_template";

/// How long [`prune_test_databases`] leaves a database alone by default.
///
/// Anything younger than this may belong to a test that is still running.
///
/// ```
/// assert_eq!(moso_test::db::DEFAULT_PRUNE_AGE.as_secs(), 3600);
/// ```
pub const DEFAULT_PRUNE_AGE: Duration = Duration::from_secs(3600);

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

/// Everything that can go wrong setting a test database up.
///
/// ```
/// use moso_test::db::Error;
///
/// let error = Error::NoDatabaseUrl;
/// assert!(error.to_string().contains("DATABASE_URL"));
/// assert!(error.is_missing_database());
/// ```
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// `DATABASE_URL` is not set, so there is no server to create a database on.
    NoDatabaseUrl,
    /// The URL does not name a backend Moso can open.
    UnsupportedUrl {
        /// The URL, with any password already removed.
        url: String,
    },
    /// A name that would have been interpolated into DDL is not a safe
    /// identifier.
    InvalidName {
        /// The rejected name.
        name: String,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// The server refused, or the statement failed.
    Sql {
        /// What the harness was doing.
        context: String,
        /// The driver's message.
        message: String,
    },
    /// The migrator supplied by the test failed.
    Migration {
        /// The migrator's own message.
        message: String,
    },
    /// A filesystem operation on a SQLite database failed.
    Io {
        /// What the harness was doing.
        context: String,
        /// The operating system's message.
        message: String,
    },
    /// A strategy was asked for something the backend cannot do.
    Unsupported {
        /// What was asked for.
        what: String,
    },
}

impl Error {
    /// Whether this is the "no database configured" case a gated test skips on.
    ///
    /// ```
    /// assert!(moso_test::db::Error::NoDatabaseUrl.is_missing_database());
    /// ```
    #[must_use]
    pub const fn is_missing_database(&self) -> bool {
        matches!(self, Self::NoDatabaseUrl)
    }

    /// Builds an [`Error::Sql`] from a driver error and a description of what
    /// the harness was doing when it happened.
    fn sql(context: impl Into<String>, error: &sqlx::Error) -> Self {
        Self::Sql {
            context: context.into(),
            message: error.to_string(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoDatabaseUrl => write!(
                f,
                "{DATABASE_URL_ENV} is not set, so there is no server to create a test database \
                 on\n  help: start the test server and export the URL, for example\n         \
                 DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test\n  help: or gate \
                 the test: `moso_test::skip_without_database!();`\n  help: or use SQLite, which \
                 needs no server: `TestDb::builder().sqlite().acquire().await?`"
            ),
            Self::UnsupportedUrl { url } => write!(
                f,
                "`{url}` names no backend moso-test can open\n  help: the scheme must be \
                 `postgres://`, `postgresql://` or `sqlite://`"
            ),
            Self::InvalidName { name, reason } => write!(
                f,
                "`{name}` is not a usable database name: {reason}\n  help: use lower-case \
                 letters, digits and underscores, at most 63 bytes, not starting with a digit"
            ),
            Self::Sql { context, message } => write!(f, "{context}: {message}"),
            Self::Migration { message } => write!(
                f,
                "the migrator failed while preparing the test database: {message}"
            ),
            Self::Io { context, message } => write!(f, "{context}: {message}"),
            Self::Unsupported { what } => write!(f, "{what}"),
        }
    }
}

impl std::error::Error for Error {}

/// The result type every function in this module returns.
///
/// ```
/// fn ok() -> moso_test::db::Result<u8> {
///     Ok(1)
/// }
/// assert_eq!(ok().unwrap(), 1);
/// ```
pub type Result<T, E = Error> = core::result::Result<T, E>;

// ---------------------------------------------------------------------------
// Availability
// ---------------------------------------------------------------------------

/// Whether a Postgres URL is configured for this process.
///
/// ```
/// // True only when `DATABASE_URL` is exported.
/// let _: bool = moso_test::db::database_is_available();
/// ```
#[must_use]
pub fn database_is_available() -> bool {
    configured_url().is_some()
}

/// The message a gated test prints when it skips.
///
/// ```
/// assert!(moso_test::db::skip_reason().contains("DATABASE_URL"));
/// ```
#[must_use]
pub fn skip_reason() -> String {
    Error::NoDatabaseUrl.to_string()
}

/// The configured server URL, if any. Empty is treated as unset, because an
/// exported-but-empty variable is always an accident.
fn configured_url() -> Option<String> {
    std::env::var(DATABASE_URL_ENV)
        .ok()
        .filter(|url| !url.trim().is_empty())
}

// ---------------------------------------------------------------------------
// Strategy
// ---------------------------------------------------------------------------

/// How a test gets an isolated database.
///
/// ```
/// use moso_test::db::Strategy;
///
/// assert_eq!(Strategy::default(), Strategy::Template);
/// assert_eq!("transaction".parse::<Strategy>().unwrap(), Strategy::Transaction);
/// assert_eq!(Strategy::Migrate.as_str(), "migrate");
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Strategy {
    /// Migrate once into a template database, then copy it per test.
    #[default]
    Template,
    /// Pin one connection, open a transaction, roll it back at the end.
    Transaction,
    /// Create an empty database and replay the whole migration chain.
    Migrate,
}

impl Strategy {
    /// The spelling used in `moso.toml` and in [`STRATEGY_ENV`].
    ///
    /// ```
    /// assert_eq!(moso_test::db::Strategy::Template.as_str(), "template");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Transaction => "transaction",
            Self::Migrate => "migrate",
        }
    }

    /// Whether the strategy needs a database of its own.
    ///
    /// [`Strategy::Transaction`] does not: it shares the configured database
    /// and relies on the rollback for isolation.
    ///
    /// ```
    /// use moso_test::db::Strategy;
    ///
    /// assert!(Strategy::Template.creates_a_database());
    /// assert!(!Strategy::Transaction.creates_a_database());
    /// ```
    #[must_use]
    pub const fn creates_a_database(self) -> bool {
        matches!(self, Self::Template | Self::Migrate)
    }

    /// Whether the strategy needs a [`Migrator`] to have been supplied.
    ///
    /// ```
    /// use moso_test::db::Strategy;
    ///
    /// assert!(Strategy::Migrate.needs_a_migrator());
    /// assert!(!Strategy::Transaction.needs_a_migrator());
    /// ```
    #[must_use]
    pub const fn needs_a_migrator(self) -> bool {
        matches!(self, Self::Template | Self::Migrate)
    }

    /// The strategy [`STRATEGY_ENV`] selects, or the default.
    ///
    /// An unrecognised value is ignored rather than fatal: a typo in a shell
    /// profile should not fail a suite it has nothing to do with.
    ///
    /// ```
    /// let _: moso_test::db::Strategy = moso_test::db::Strategy::from_env();
    /// ```
    #[must_use]
    pub fn from_env() -> Self {
        std::env::var(STRATEGY_ENV)
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_default()
    }
}

impl fmt::Display for Strategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The error [`Strategy`]'s [`FromStr`](core::str::FromStr) returns.
///
/// ```
/// use moso_test::db::UnknownStrategy;
///
/// let error: UnknownStrategy = "templte".parse::<moso_test::db::Strategy>().unwrap_err();
/// assert!(error.to_string().contains("template"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnknownStrategy {
    /// What was written.
    given: String,
}

impl UnknownStrategy {
    /// What was written.
    ///
    /// ```
    /// let error = "nope".parse::<moso_test::db::Strategy>().unwrap_err();
    /// assert_eq!(error.given(), "nope");
    /// ```
    #[must_use]
    pub fn given(&self) -> &str {
        &self.given
    }
}

impl fmt::Display for UnknownStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "`{}` is not a test strategy\n  help: one of `template`, `transaction`, `migrate`",
            self.given
        )
    }
}

impl std::error::Error for UnknownStrategy {}

impl core::str::FromStr for Strategy {
    type Err = UnknownStrategy;

    fn from_str(s: &str) -> core::result::Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "template" => Ok(Self::Template),
            "transaction" | "tx" => Ok(Self::Transaction),
            "migrate" | "migration" | "migrations" => Ok(Self::Migrate),
            _ => Err(UnknownStrategy {
                given: s.to_owned(),
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Migrations
// ---------------------------------------------------------------------------

/// A boxed future, because [`Migrator`] has to be `dyn`-compatible (D4).
pub type BoxFuture<'a, T> = core::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Whatever a migrator wants to report when it fails.
pub type MigrationError = Box<dyn std::error::Error + Send + Sync>;

/// Brings an empty database up to the schema the tests expect.
///
/// The harness does not know how an application migrates — `moso-migrate` is one
/// answer, a directory of `.sql` files is another, and a test that builds its
/// schema inline is a perfectly good third. So the harness takes this trait and
/// calls it exactly once per template.
///
/// [`Migrator::fingerprint`] is what makes the template cache correct: when it
/// changes, the template is dropped and rebuilt, so an edited migration cannot
/// be tested against yesterday's schema.
///
/// ```
/// use moso_test::db::{BoxFuture, MigrationError, MigrationTarget, Migrator};
///
/// /// The two tables these tests need.
/// struct Schema;
///
/// impl Migrator for Schema {
///     fn fingerprint(&self) -> String {
///         "widgets-v1".to_owned()
///     }
///
///     fn migrate<'a>(&'a self, target: &'a MigrationTarget)
///         -> BoxFuture<'a, Result<(), MigrationError>>
///     {
///         Box::pin(async move {
///             target.execute("create table widget (id int primary key)").await?;
///             Ok(())
///         })
///     }
/// }
///
/// assert_eq!(Schema.fingerprint(), "widgets-v1");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot migrate a test database",
    label = "not a migrator",
    note = "the `template` and `migrate` strategies have to build the schema once, and only the \
            application knows how",
    note = "help: pass a directory of `.sql` files: `.migrator(SqlMigrator::from_dir(\"migrations\")?)`",
    note = "help: or implement `Migrator for {Self}` with `fingerprint` and `migrate`"
)]
pub trait Migrator: Send + Sync + 'static {
    /// A value that changes whenever the schema this migrator produces changes.
    ///
    /// A content hash of the migration files is the right answer;
    /// [`SqlMigrator`] computes one.
    ///
    /// ```
    /// # use moso_test::db::Migrator;
    /// fn tag(migrator: &dyn Migrator) -> String {
    ///     migrator.fingerprint()
    /// }
    /// ```
    fn fingerprint(&self) -> String;

    /// Brings `target` — an empty database — up to the current schema.
    ///
    /// # Errors
    ///
    /// Whatever the migrator wants to report; the harness renders it as
    /// [`Error::Migration`].
    ///
    /// ```
    /// # use moso_test::db::{BoxFuture, MigrationError, MigrationTarget, Migrator};
    /// fn run<'a>(m: &'a dyn Migrator, t: &'a MigrationTarget)
    ///     -> BoxFuture<'a, Result<(), MigrationError>>
    /// {
    ///     m.migrate(t)
    /// }
    /// ```
    fn migrate<'a>(
        &'a self,
        target: &'a MigrationTarget,
    ) -> BoxFuture<'a, Result<(), MigrationError>>;
}

/// The empty database a [`Migrator`] is handed.
///
/// It exposes the URL — for a migrator that wants to open its own pool — and a
/// statement runner, for one that does not.
///
/// ```no_run
/// # async fn example(target: &moso_test::db::MigrationTarget) -> Result<(), Box<dyn std::error::Error>> {
/// target.execute("create table widget (id int primary key)").await?;
/// assert!(target.url().starts_with("postgres"));
/// # Ok(())
/// # }
/// ```
pub struct MigrationTarget {
    url: String,
    backend: Backend,
    pool: Pool,
}

impl MigrationTarget {
    /// The URL of the database being migrated.
    ///
    /// ```no_run
    /// # fn example(target: &moso_test::db::MigrationTarget) -> String {
    /// target.url().to_owned()
    /// # }
    /// ```
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Which backend it is.
    ///
    /// ```no_run
    /// # fn example(target: &moso_test::db::MigrationTarget) -> moso_orm::Backend {
    /// target.backend()
    /// # }
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }

    /// Runs one statement.
    ///
    /// # Errors
    ///
    /// [`Error::Sql`] when the server refuses it.
    ///
    /// ```no_run
    /// # async fn example(t: &moso_test::db::MigrationTarget) -> moso_test::db::Result<()> {
    /// t.execute("create table widget (id int primary key)").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(&self, sql: &str) -> Result<u64> {
        self.pool.execute(sql).await
    }

    /// Runs a script: every statement in `sql`, in order, on one connection.
    ///
    /// # Errors
    ///
    /// [`Error::Sql`] when the server refuses any of it.
    ///
    /// ```no_run
    /// # async fn example(t: &moso_test::db::MigrationTarget) -> moso_test::db::Result<()> {
    /// t.execute_batch("create table a (id int); create table b (id int);").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_batch(&self, sql: &str) -> Result<()> {
        self.pool.execute_batch(sql).await
    }
}

impl fmt::Debug for MigrationTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MigrationTarget")
            .field("backend", &self.backend)
            .finish_non_exhaustive()
    }
}

/// A [`Migrator`] that runs SQL scripts, in name order.
///
/// This is the answer for a project whose migrations are a directory of `.sql`
/// files, and for the framework's own tests. `moso-migrate` supplies its own
/// [`Migrator`]; this one exists so that the harness is usable without it.
///
/// ```
/// use moso_test::db::{Migrator, SqlMigrator};
///
/// let migrator = SqlMigrator::new([
///     "create table widget (id int primary key)",
///     "create table gadget (id int primary key)",
/// ]);
/// assert_eq!(migrator.scripts().len(), 2);
///
/// // The fingerprint is a content hash, so an edited script rebuilds the template.
/// let edited = SqlMigrator::new(["create table widget (id bigint primary key)"]);
/// assert_ne!(migrator.fingerprint(), edited.fingerprint());
/// ```
#[derive(Clone, Debug, Default)]
pub struct SqlMigrator {
    scripts: Vec<String>,
}

impl SqlMigrator {
    /// A migrator that runs these scripts, in the order given.
    ///
    /// ```
    /// use moso_test::db::SqlMigrator;
    ///
    /// let migrator = SqlMigrator::new(["create table widget (id int primary key)"]);
    /// assert_eq!(migrator.scripts().len(), 1);
    /// ```
    #[must_use]
    pub fn new(scripts: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            scripts: scripts.into_iter().map(Into::into).collect(),
        }
    }

    /// A migrator that runs every `.sql` file in `directory`, sorted by name.
    ///
    /// Sorting by name is what makes `0001_users.sql`, `0002_posts.sql` apply in
    /// the intended order; the fingerprint covers both the names and the
    /// contents, so renaming a file rebuilds the template too.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory or one of the files cannot be read.
    ///
    /// ```no_run
    /// # fn example() -> moso_test::db::Result<()> {
    /// let migrator = moso_test::db::SqlMigrator::from_dir("migrations")?;
    /// # let _ = migrator;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_dir(directory: impl AsRef<Path>) -> Result<Self> {
        let directory = directory.as_ref();
        let entries = std::fs::read_dir(directory).map_err(|error| Error::Io {
            context: format!("reading the migration directory `{}`", directory.display()),
            message: error.to_string(),
        })?;
        let mut files: Vec<PathBuf> = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|error| Error::Io {
                context: format!("reading the migration directory `{}`", directory.display()),
                message: error.to_string(),
            })?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "sql") {
                files.push(path);
            }
        }
        files.sort();

        let mut scripts = Vec::with_capacity(files.len());
        for path in files {
            let body = std::fs::read_to_string(&path).map_err(|error| Error::Io {
                context: format!("reading the migration `{}`", path.display()),
                message: error.to_string(),
            })?;
            let name = path
                .file_name()
                .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
            // The name is folded in so that a rename alone changes the
            // fingerprint: two files with the same body but different ordinals
            // are a different schema.
            scripts.push(format!("-- {name}\n{body}"));
        }
        Ok(Self { scripts })
    }

    /// The scripts, in the order they will run.
    ///
    /// ```
    /// use moso_test::db::SqlMigrator;
    ///
    /// assert_eq!(SqlMigrator::new(["select 1"]).scripts(), ["select 1"]);
    /// ```
    #[must_use]
    pub fn scripts(&self) -> &[String] {
        &self.scripts
    }
}

impl Migrator for SqlMigrator {
    fn fingerprint(&self) -> String {
        let mut hash = Hasher::new();
        for script in &self.scripts {
            hash.write(script.as_bytes());
            hash.write(b"\0");
        }
        format!("sql-{:016x}-{}", hash.finish(), self.scripts.len())
    }

    fn migrate<'a>(
        &'a self,
        target: &'a MigrationTarget,
    ) -> BoxFuture<'a, core::result::Result<(), MigrationError>> {
        Box::pin(async move {
            for script in &self.scripts {
                target.execute_batch(script).await?;
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------

/// Rejects anything that must not be interpolated into DDL.
///
/// Database names reach `CREATE DATABASE` as identifiers, and an identifier
/// cannot be a bound parameter in any dialect. So the only safe thing to do is
/// to refuse everything that is not obviously a name.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::InvalidName {
            name: name.to_owned(),
            reason: "it is empty",
        });
    }
    if name.len() > 63 {
        return Err(Error::InvalidName {
            name: name.to_owned(),
            reason: "PostgreSQL truncates an identifier at 63 bytes, which would silently make \
                     two test databases the same one",
        });
    }
    if name.starts_with(|c: char| c.is_ascii_digit()) {
        return Err(Error::InvalidName {
            name: name.to_owned(),
            reason: "it starts with a digit",
        });
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        return Err(Error::InvalidName {
            name: name.to_owned(),
            reason: "it contains something other than a lower-case letter, a digit or an \
                     underscore",
        });
    }
    Ok(())
}

/// The process-wide counter that makes generated names unique within a run.
static NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Base-36, because a name has 63 bytes and three fields to fit in them.
fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_owned();
    }
    let mut out = Vec::new();
    while value > 0 {
        out.push(DIGITS[usize::try_from(value % 36).unwrap_or(0)]);
        value /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap_or_else(|_| "0".to_owned())
}

fn from_base36(text: &str) -> Option<u64> {
    if text.is_empty() {
        return None;
    }
    let mut value: u64 = 0;
    for byte in text.bytes() {
        let digit = match byte {
            b'0'..=b'9' => u64::from(byte - b'0'),
            b'a'..=b'z' => u64::from(byte - b'a') + 10,
            _ => return None,
        };
        value = value.checked_mul(36)?.checked_add(digit)?;
    }
    Some(value)
}

/// A fresh name, of the form `moso_test_{millis}_{pid}_{ordinal}`.
///
/// The creation time is *in the name* on purpose: [`prune_test_databases`] has
/// no other way to tell a database abandoned by yesterday's crashed run from one
/// belonging to a test that is running right now.
fn generate_name() -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let millis = u64::try_from(millis).unwrap_or(u64::MAX);
    let ordinal = NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!(
        "{NAME_PREFIX}{}_{}_{}",
        base36(millis),
        base36(u64::from(std::process::id())),
        base36(ordinal)
    )
}

/// When a generated name says it was created, or `None` if the name was not
/// generated by this module.
///
/// ```
/// use moso_test::db::created_at;
///
/// assert!(created_at("moso_test_l9k2j_1x_0").is_some());
/// assert!(created_at("moso_test_template").is_none());
/// assert!(created_at("production").is_none());
/// assert!(created_at("moso_test_not-base36_1_0").is_none());
/// ```
#[must_use]
pub fn created_at(name: &str) -> Option<SystemTime> {
    let rest = name.strip_prefix(NAME_PREFIX)?;
    let mut parts = rest.split('_');
    let millis = from_base36(parts.next()?)?;
    from_base36(parts.next()?)?;
    from_base36(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_millis(millis))
}

/// The process id encoded in a generated name.
///
/// ```
/// assert_eq!(moso_test::db::owning_process("moso_test_l9k2j_1x_0"), Some(69));
/// assert_eq!(moso_test::db::owning_process("production"), None);
/// ```
#[must_use]
pub fn owning_process(name: &str) -> Option<u64> {
    let rest = name.strip_prefix(NAME_PREFIX)?;
    let mut parts = rest.split('_');
    from_base36(parts.next()?)?;
    let pid = from_base36(parts.next()?)?;
    from_base36(parts.next()?)?;
    if parts.next().is_some() {
        return None;
    }
    Some(pid)
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

/// FNV-1a. Not cryptographic, and does not need to be: it fingerprints a schema
/// and keys an advisory lock. Hand-written so the harness does not pull a crate
/// in for eight lines.
#[derive(Clone, Copy, Debug)]
struct Hasher(u64);

impl Hasher {
    const fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x1000_0000_01b3);
        }
    }

    const fn finish(self) -> u64 {
        self.0
    }

    fn of(bytes: &[u8]) -> u64 {
        let mut hasher = Self::new();
        hasher.write(bytes);
        hasher.finish()
    }
}

// ---------------------------------------------------------------------------
// URLs
// ---------------------------------------------------------------------------

/// The pieces of a database URL the harness needs to rewrite.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UrlParts {
    /// Everything before the database name, including the trailing slash.
    prefix: String,
    /// The database name.
    database: String,
    /// The query string, including the leading `?`, or empty.
    suffix: String,
}

impl UrlParts {
    /// Splits a Postgres URL into "everything before the database name", the
    /// name, and the query string.
    fn parse(url: &str) -> Option<Self> {
        let scheme_end = url.find("://")? + 3;
        let (scheme, rest) = url.split_at(scheme_end);
        // The authority ends at the first `/`; everything after it is the
        // database name and then an optional query string.
        let slash = rest.find('/')?;
        let (authority, tail) = rest.split_at(slash + 1);
        let (database, suffix) = match tail.find(['?', '#']) {
            Some(index) => tail.split_at(index),
            None => (tail, ""),
        };
        Some(Self {
            prefix: format!("{scheme}{authority}"),
            database: database.to_owned(),
            suffix: suffix.to_owned(),
        })
    }

    /// The same URL, pointing at a different database.
    fn with_database(&self, database: &str) -> String {
        format!("{}{}{}", self.prefix, database, self.suffix)
    }
}

/// Which backend a URL names.
fn backend_of(url: &str) -> Result<Backend> {
    let lower = url.trim().to_ascii_lowercase();
    if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
        Ok(Backend::Postgres)
    } else if lower.starts_with("sqlite:") {
        Ok(Backend::Sqlite)
    } else {
        Err(Error::UnsupportedUrl {
            url: redact(url).into_owned(),
        })
    }
}

/// A URL with the password replaced, for a message a human will read.
///
/// ```
/// use moso_test::db::redact;
///
/// assert_eq!(redact("postgres://u:secret@h/db"), "postgres://u:***@h/db");
/// assert_eq!(redact("sqlite:///tmp/x.db"), "sqlite:///tmp/x.db");
/// ```
#[must_use]
pub fn redact(url: &str) -> std::borrow::Cow<'_, str> {
    let Some(scheme_end) = url.find("://") else {
        return std::borrow::Cow::Borrowed(url);
    };
    let rest = &url[scheme_end + 3..];
    let Some(at) = rest.find('@') else {
        return std::borrow::Cow::Borrowed(url);
    };
    let userinfo = &rest[..at];
    let Some(colon) = userinfo.find(':') else {
        return std::borrow::Cow::Borrowed(url);
    };
    std::borrow::Cow::Owned(format!(
        "{}{}:***{}",
        &url[..scheme_end + 3],
        &userinfo[..colon],
        &rest[at..]
    ))
}

// ---------------------------------------------------------------------------
// Pools
// ---------------------------------------------------------------------------

/// One backend's pool. Private: which driver is behind a [`TestDb`] is decided
/// by its URL, and widening this must not be a breaking change.
#[derive(Clone)]
enum Pool {
    Postgres(sqlx::PgPool),
    Sqlite(sqlx::SqlitePool),
}

impl Pool {
    /// Opens a small pool. `size` is deliberately tiny: a hundred parallel tests
    /// each holding a fat pool is how a suite runs out of `max_connections`.
    async fn open(url: &str, size: u32, minimum: u32) -> Result<Self> {
        match backend_of(url)? {
            Backend::Postgres => {
                let options: sqlx::postgres::PgConnectOptions =
                    url.parse().map_err(|error: sqlx::Error| {
                        Error::sql(format!("parsing `{}`", redact(url)), &error)
                    })?;
                let pool = sqlx::postgres::PgPoolOptions::new()
                    .max_connections(size)
                    .min_connections(minimum)
                    .acquire_timeout(Duration::from_secs(30))
                    .test_before_acquire(false)
                    .connect_with(options)
                    .await
                    .map_err(|error| {
                        Error::sql(format!("connecting to `{}`", redact(url)), &error)
                    })?;
                Ok(Self::Postgres(pool))
            }
            _ => {
                let options: sqlx::sqlite::SqliteConnectOptions = url
                    .parse()
                    .map_err(|error: sqlx::Error| Error::sql(format!("parsing `{url}`"), &error))?;
                let pool = sqlx::sqlite::SqlitePoolOptions::new()
                    .max_connections(size)
                    .min_connections(minimum.max(1))
                    .acquire_timeout(Duration::from_secs(30))
                    .test_before_acquire(false)
                    .connect_with(options.create_if_missing(true))
                    .await
                    .map_err(|error| Error::sql(format!("opening `{url}`"), &error))?;
                Ok(Self::Sqlite(pool))
            }
        }
    }

    /// Runs one statement — or several, since `CREATE DATABASE` and a migration
    /// both go through here and neither may be prepared.
    async fn execute(&self, sql: &str) -> Result<u64> {
        match self {
            Self::Postgres(pool) => pool
                .execute(sqlx::raw_sql(AssertSqlSafe(sql.to_owned())))
                .await
                .map(|done| done.rows_affected())
                .map_err(|error| Error::sql(statement_context(sql), &error)),
            Self::Sqlite(pool) => pool
                .execute(sqlx::raw_sql(AssertSqlSafe(sql.to_owned())))
                .await
                .map(|done| done.rows_affected())
                .map_err(|error| Error::sql(statement_context(sql), &error)),
        }
    }

    async fn execute_batch(&self, sql: &str) -> Result<()> {
        self.execute(sql).await.map(|_| ())
    }

    async fn fetch_i64(&self, sql: &str) -> Result<i64> {
        match self {
            Self::Postgres(pool) => sqlx::query(AssertSqlSafe(sql.to_owned()))
                .fetch_one(pool)
                .await
                .and_then(|row| row.try_get::<i64, _>(0))
                .map_err(|error| Error::sql(statement_context(sql), &error)),
            Self::Sqlite(pool) => sqlx::query(AssertSqlSafe(sql.to_owned()))
                .fetch_one(pool)
                .await
                .and_then(|row| row.try_get::<i64, _>(0))
                .map_err(|error| Error::sql(statement_context(sql), &error)),
        }
    }

    async fn fetch_text_column(&self, sql: &str) -> Result<Vec<String>> {
        match self {
            Self::Postgres(pool) => sqlx::query(AssertSqlSafe(sql.to_owned()))
                .fetch_all(pool)
                .await
                .map_err(|error| Error::sql(statement_context(sql), &error))?
                .into_iter()
                .map(|row| {
                    row.try_get::<String, _>(0)
                        .map_err(|error| Error::sql(statement_context(sql), &error))
                })
                .collect(),
            Self::Sqlite(pool) => sqlx::query(AssertSqlSafe(sql.to_owned()))
                .fetch_all(pool)
                .await
                .map_err(|error| Error::sql(statement_context(sql), &error))?
                .into_iter()
                .map(|row| {
                    row.try_get::<String, _>(0)
                        .map_err(|error| Error::sql(statement_context(sql), &error))
                })
                .collect(),
        }
    }

    async fn close(&self) {
        match self {
            Self::Postgres(pool) => pool.close().await,
            Self::Sqlite(pool) => pool.close().await,
        }
    }
}

/// The first line of a statement, for an error message that is readable when the
/// statement is a 200-line migration.
fn statement_context(sql: &str) -> String {
    let trimmed = sql.trim();
    let first = trimmed.lines().next().unwrap_or("").trim();
    if first.len() <= 72 && first.len() == trimmed.len() {
        format!("running `{first}`")
    } else {
        let mut head: String = first.chars().take(69).collect();
        head.push_str("...");
        format!("running `{head}`")
    }
}

// ---------------------------------------------------------------------------
// The builder
// ---------------------------------------------------------------------------

/// Configures a [`TestDb`].
///
/// ```no_run
/// # async fn example() -> moso_test::db::Result<()> {
/// use moso_test::db::{SqlMigrator, Strategy, TestDb};
///
/// let db = TestDb::builder()
///     .strategy(Strategy::Template)
///     .migrator(SqlMigrator::new(["create table widget (id int primary key)"]))
///     .acquire()
///     .await?;
/// # let _ = db;
/// # Ok(())
/// # }
/// ```
#[derive(Default)]
pub struct TestDbBuilder {
    url: Option<String>,
    strategy: Option<Strategy>,
    migrator: Option<Arc<dyn Migrator>>,
    template: Option<String>,
    keep: Option<bool>,
    pool_size: Option<u32>,
    sqlite: bool,
}

impl fmt::Debug for TestDbBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestDbBuilder")
            .field("url", &self.url.as_deref().map(redact))
            .field("strategy", &self.strategy)
            .field("has_migrator", &self.migrator.is_some())
            .field("template", &self.template)
            .field("keep", &self.keep)
            .finish_non_exhaustive()
    }
}

impl TestDbBuilder {
    /// Use this server instead of `DATABASE_URL`.
    ///
    /// ```
    /// # use moso_test::db::TestDb;
    /// let builder = TestDb::builder().url("postgres://moso:moso@localhost:55433/moso_test");
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self.sqlite = false;
        self
    }

    /// Use a private SQLite file, which needs no server and no environment.
    ///
    /// The file lives in the temporary directory and is deleted with the
    /// [`TestDb`]. The `template` strategy copies the file rather than issuing
    /// `CREATE DATABASE`, which is the same idea and rather faster.
    ///
    /// ```
    /// # use moso_test::db::TestDb;
    /// let builder = TestDb::builder().sqlite();
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn sqlite(mut self) -> Self {
        self.sqlite = true;
        self.url = None;
        self
    }

    /// Which isolation [`Strategy`] to use.
    ///
    /// ```
    /// # use moso_test::db::{Strategy, TestDb};
    /// let builder = TestDb::builder().strategy(Strategy::Transaction);
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn strategy(mut self, strategy: Strategy) -> Self {
        self.strategy = Some(strategy);
        self
    }

    /// How the schema is built.
    ///
    /// Required by [`Strategy::Template`] and [`Strategy::Migrate`]; ignored by
    /// [`Strategy::Transaction`], which inherits the schema already in the
    /// configured database.
    ///
    /// ```
    /// # use moso_test::db::{SqlMigrator, TestDb};
    /// let builder = TestDb::builder().migrator(SqlMigrator::new(["select 1"]));
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn migrator(mut self, migrator: impl Migrator) -> Self {
        self.migrator = Some(Arc::new(migrator));
        self
    }

    /// The same, for a migrator that is already behind an [`Arc`].
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_test::db::{Migrator, SqlMigrator, TestDb};
    /// let shared: Arc<dyn Migrator> = Arc::new(SqlMigrator::new(["select 1"]));
    /// let builder = TestDb::builder().shared_migrator(Arc::clone(&shared));
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn shared_migrator(mut self, migrator: Arc<dyn Migrator>) -> Self {
        self.migrator = Some(migrator);
        self
    }

    /// Override the template database's name.
    ///
    /// Two test suites on one server that share a schema should share a
    /// template; two that do not, must not.
    ///
    /// ```
    /// # use moso_test::db::TestDb;
    /// let builder = TestDb::builder().template("shop_test_template");
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn template(mut self, name: impl Into<String>) -> Self {
        self.template = Some(name.into());
        self
    }

    /// Keep the database when the [`TestDb`] is dropped, and print its URL.
    ///
    /// The `--keep-db` of `43-testing.md`. `MOSO_TEST_KEEP_DB=1` does the same
    /// for a whole run.
    ///
    /// ```
    /// # use moso_test::db::TestDb;
    /// let builder = TestDb::builder().keep();
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn keep(mut self) -> Self {
        self.keep = Some(true);
        self
    }

    /// How many connections the test's own pool may open. Default 2.
    ///
    /// Raise it only for a test that is *about* concurrency: a hundred parallel
    /// tests times a large pool is how a suite exhausts `max_connections`.
    ///
    /// ```
    /// # use moso_test::db::TestDb;
    /// let builder = TestDb::builder().pool_size(4);
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn pool_size(mut self, size: u32) -> Self {
        self.pool_size = Some(size.max(1));
        self
    }

    /// Creates the database.
    ///
    /// # Errors
    ///
    /// [`Error::NoDatabaseUrl`] when neither [`TestDbBuilder::url`] nor
    /// [`TestDbBuilder::sqlite`] was called and `DATABASE_URL` is unset, and
    /// anything in [`Error`] when the server refuses.
    ///
    /// ```no_run
    /// # async fn example() -> moso_test::db::Result<()> {
    /// let db = moso_test::db::TestDb::builder().sqlite().acquire().await?;
    /// # let _ = db;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acquire(self) -> Result<TestDb> {
        let strategy = self.strategy.unwrap_or_else(Strategy::from_env);
        let keep = self
            .keep
            .unwrap_or_else(|| std::env::var(KEEP_ENV).is_ok_and(|value| truthy(&value)));
        let pool_size = self.pool_size.unwrap_or(2);

        let base_url = if self.sqlite {
            sqlite_url(&generate_name())
        } else {
            match self.url.clone().or_else(configured_url) {
                Some(url) => url,
                None => return Err(Error::NoDatabaseUrl),
            }
        };
        let backend = backend_of(&base_url)?;

        match backend {
            Backend::Postgres => {
                self.acquire_postgres(base_url, strategy, keep, pool_size)
                    .await
            }
            // `backend_of` only ever produces the two, and `Backend` is
            // `#[non_exhaustive]`, so a third one lands here rather than
            // breaking the build when moso-orm gains it.
            _ => {
                self.acquire_sqlite(base_url, strategy, keep, pool_size)
                    .await
            }
        }
    }

    async fn acquire_postgres(
        self,
        base_url: String,
        strategy: Strategy,
        keep: bool,
        pool_size: u32,
    ) -> Result<TestDb> {
        let parts = UrlParts::parse(&base_url).ok_or_else(|| Error::UnsupportedUrl {
            url: redact(&base_url).into_owned(),
        })?;

        if strategy == Strategy::Transaction {
            // No database is created: isolation comes from the rollback, which
            // is the whole point of this strategy. The pool is pinned to one
            // connection so that every statement lands inside the same open
            // transaction.
            let pool = Pool::open(&base_url, 1, 1).await?;
            pool.execute("begin").await?;
            return Ok(TestDb::new(
                parts.database.clone(),
                base_url,
                Some(parts),
                strategy,
                Backend::Postgres,
                keep,
                pool,
                None,
            ));
        }

        let template = self.resolve_template(&parts);
        validate_name(&template)?;

        let migrator = self.migrator.clone().ok_or_else(|| Error::Unsupported {
            what: format!(
                "the `{strategy}` strategy needs a migrator, because it starts from an empty \
                 database\n  help: supply one: \
                 `TestDb::builder().migrator(SqlMigrator::from_dir(\"migrations\")?)`\n  help: or \
                 use `Strategy::Transaction`, which inherits the schema already in \
                 {DATABASE_URL_ENV}"
            ),
        })?;

        let name = generate_name();
        validate_name(&name)?;
        let url = parts.with_database(&name);

        match strategy {
            Strategy::Template => {
                ensure_template(&parts, &template, migrator.as_ref()).await?;
                create_from_template(&parts, &name, &template).await?;
            }
            Strategy::Migrate => {
                create_empty(&parts, &name).await?;
                run_migrator(&url, Backend::Postgres, migrator.as_ref()).await?;
            }
            Strategy::Transaction => unreachable!("handled above"),
        }

        let pool = Pool::open(&url, pool_size, 0).await?;
        Ok(TestDb::new(
            name,
            url,
            Some(parts),
            strategy,
            Backend::Postgres,
            keep,
            pool,
            None,
        ))
    }

    async fn acquire_sqlite(
        self,
        base_url: String,
        strategy: Strategy,
        keep: bool,
        pool_size: u32,
    ) -> Result<TestDb> {
        let name = generate_name();
        let url = sqlite_url(&name);
        let path = sqlite_path(&name);

        if strategy == Strategy::Transaction {
            let pool = Pool::open(&base_url, 1, 1).await?;
            pool.execute("begin").await?;
            return Ok(TestDb::new(
                name,
                base_url,
                None,
                strategy,
                Backend::Sqlite,
                keep,
                pool,
                None,
            ));
        }

        let migrator = self.migrator.clone().ok_or_else(|| Error::Unsupported {
            what: format!(
                "the `{strategy}` strategy needs a migrator, because it starts from an empty \
                 database\n  help: supply one: \
                 `TestDb::builder().migrator(SqlMigrator::from_dir(\"migrations\")?)`"
            ),
        })?;

        match strategy {
            Strategy::Template => {
                let template = self
                    .template
                    .clone()
                    .unwrap_or_else(|| DEFAULT_TEMPLATE.to_owned());
                let source = ensure_sqlite_template(&template, migrator.as_ref()).await?;
                std::fs::copy(&source, &path).map_err(|error| Error::Io {
                    context: format!(
                        "copying the SQLite template `{}` to `{}`",
                        source.display(),
                        path.display()
                    ),
                    message: error.to_string(),
                })?;
            }
            Strategy::Migrate => {
                run_migrator(&url, Backend::Sqlite, migrator.as_ref()).await?;
            }
            Strategy::Transaction => unreachable!("handled above"),
        }

        let pool = Pool::open(&url, pool_size, 1).await?;
        Ok(TestDb::new(
            name,
            url,
            None,
            strategy,
            Backend::Sqlite,
            keep,
            pool,
            Some(path),
        ))
    }

    /// The template name: explicit, then the environment, then `<database>_template`.
    fn resolve_template(&self, parts: &UrlParts) -> String {
        if let Some(name) = &self.template {
            return name.clone();
        }
        if let Ok(name) = std::env::var(TEMPLATE_ENV)
            && !name.trim().is_empty()
        {
            return name;
        }
        if parts.database.is_empty() {
            DEFAULT_TEMPLATE.to_owned()
        } else {
            format!("{}_template", parts.database)
        }
    }
}

/// `1`, `true`, `yes`, `on` — anything else is false.
fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn sqlite_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("{name}.sqlite"))
}

fn sqlite_url(name: &str) -> String {
    format!("sqlite://{}", sqlite_path(name).display())
}

// ---------------------------------------------------------------------------
// Template management
// ---------------------------------------------------------------------------

/// The table the template stamps its fingerprint into.
const FINGERPRINT_TABLE: &str = "_moso_test_template";

/// Templates already proven current in *this* process, so that the second test
/// does not pay for the check. The cross-process case is handled by the advisory
/// lock and the stamped fingerprint, which cost a few milliseconds.
fn template_cache() -> &'static Mutex<HashMap<String, String>> {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn template_is_cached(name: &str, fingerprint: &str) -> bool {
    template_cache()
        .lock()
        .ok()
        .is_some_and(|cache| cache.get(name).is_some_and(|seen| seen == fingerprint))
}

fn cache_template(name: &str, fingerprint: &str) {
    if let Ok(mut cache) = template_cache().lock() {
        cache.insert(name.to_owned(), fingerprint.to_owned());
    }
}

/// How many maintenance connections the harness will hold at once.
///
/// A hundred tests starting together would otherwise open a hundred
/// administrative connections on top of their own, and `max_connections` on a
/// stock PostgreSQL is a hundred. Serialising the *provisioning* costs nothing —
/// `CREATE DATABASE` takes tens of milliseconds — and removes a failure mode
/// that would look like flakiness.
const ADMIN_CONNECTION_LIMIT: usize = 16;

/// A maintenance connection, and the permit that limits how many exist.
struct AdminPool {
    pool: Pool,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl core::ops::Deref for AdminPool {
    type Target = Pool;

    fn deref(&self) -> &Pool {
        &self.pool
    }
}

impl AdminPool {
    async fn close(self) {
        self.pool.close().await;
    }
}

/// Opens a connection to the *maintenance* database.
///
/// `CREATE DATABASE` cannot run inside a transaction and cannot run on the
/// database being created, so every strategy that creates one needs a
/// connection somewhere else. `postgres` is the conventional home; `template1`
/// is the fallback for a server where it has been removed.
async fn admin_pool(parts: &UrlParts) -> Result<AdminPool> {
    let permit = Arc::clone(admin_permits_handle())
        .acquire_owned()
        .await
        .map_err(|_| Error::Unsupported {
            what: "the moso-test maintenance-connection semaphore was closed".to_owned(),
        })?;
    let mut last = None;
    for candidate in ["postgres", "template1"] {
        match Pool::open(&parts.with_database(candidate), 1, 1).await {
            Ok(pool) => {
                return Ok(AdminPool {
                    pool,
                    _permit: permit,
                });
            }
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or(Error::NoDatabaseUrl))
}

fn admin_permits_handle() -> &'static Arc<tokio::sync::Semaphore> {
    static HANDLE: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    HANDLE.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(ADMIN_CONNECTION_LIMIT)))
}

/// Serialises template preparation *within* this process.
///
/// The advisory lock and the file rename each make template building safe
/// between processes, but a hundred tasks in one process racing to discover that
/// the template is already there is a hundred round trips nobody needs — and, on
/// SQLite, a hundred migrators writing the same file. One await is cheaper than
/// any of the alternatives.
fn template_gate() -> &'static tokio::sync::Mutex<()> {
    static GATE: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| tokio::sync::Mutex::new(()))
}

/// Makes sure `template` exists and matches the migrator's fingerprint.
async fn ensure_template(parts: &UrlParts, template: &str, migrator: &dyn Migrator) -> Result<()> {
    let fingerprint = migrator.fingerprint();
    if template_is_cached(template, &fingerprint) {
        return Ok(());
    }

    let _gate = template_gate().lock().await;
    // Another task may have built it while this one waited for the gate.
    if template_is_cached(template, &fingerprint) {
        return Ok(());
    }

    let admin = admin_pool(parts).await?;
    // A session-level advisory lock, so that two `cargo test` processes racing
    // to build the same template take turns instead of both half-building it.
    // It is released when the connection closes, which covers a panic.
    let key = i64::from_ne_bytes(Hasher::of(template.as_bytes()).to_ne_bytes());
    admin
        .execute(&format!("select pg_advisory_lock({key})"))
        .await?;

    let outcome = ensure_template_locked(parts, template, migrator, &fingerprint, &admin).await;

    let _ = admin
        .execute(&format!("select pg_advisory_unlock({key})"))
        .await;
    admin.close().await;

    outcome?;
    cache_template(template, &fingerprint);
    Ok(())
}

async fn ensure_template_locked(
    parts: &UrlParts,
    template: &str,
    migrator: &dyn Migrator,
    fingerprint: &str,
    admin: &Pool,
) -> Result<()> {
    let exists = admin
        .fetch_i64(&format!(
            "select count(*) from pg_database where datname = {}",
            quote_literal(template)
        ))
        .await?
        > 0;

    if exists {
        if read_fingerprint(&parts.with_database(template)).await? == Some(fingerprint.to_owned()) {
            return Ok(());
        }
        drop_database(admin, template, true).await?;
    }

    admin
        .execute(&format!("create database {}", quote_ident(template)))
        .await?;

    let url = parts.with_database(template);
    run_migrator(&url, Backend::Postgres, migrator).await?;
    stamp_fingerprint(&url, fingerprint).await?;
    Ok(())
}

/// Reads the stamp, treating "the table is not there" as "not stamped".
async fn read_fingerprint(url: &str) -> Result<Option<String>> {
    let pool = Pool::open(url, 1, 1).await?;
    let found = pool
        .fetch_text_column(&format!(
            "select fingerprint from {FINGERPRINT_TABLE} limit 1"
        ))
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next());
    pool.close().await;
    Ok(found)
}

async fn stamp_fingerprint(url: &str, fingerprint: &str) -> Result<()> {
    let pool = Pool::open(url, 1, 1).await?;
    let result = async {
        pool.execute(&format!(
            "create table if not exists {FINGERPRINT_TABLE} (fingerprint text not null)"
        ))
        .await?;
        pool.execute(&format!("delete from {FINGERPRINT_TABLE}"))
            .await?;
        pool.execute(&format!(
            "insert into {FINGERPRINT_TABLE} (fingerprint) values ({})",
            quote_literal(fingerprint)
        ))
        .await?;
        Ok::<(), Error>(())
    }
    .await;
    pool.close().await;
    result
}

/// `CREATE DATABASE x TEMPLATE y`, retrying while the template is busy.
///
/// PostgreSQL refuses to copy a database that has a session connected to it
/// (SQLSTATE 55006). Under a hundred parallel tests that window is short and
/// rare, but it is not zero, so this backs off rather than failing a test for a
/// reason that has nothing to do with the test.
async fn create_from_template(parts: &UrlParts, name: &str, template: &str) -> Result<()> {
    let admin = admin_pool(parts).await?;
    let statement = format!(
        "create database {} template {}",
        quote_ident(name),
        quote_ident(template)
    );
    let mut delay = Duration::from_millis(20);
    let mut last;
    loop {
        match admin.execute(&statement).await {
            Ok(_) => {
                admin.close().await;
                return Ok(());
            }
            Err(error) => {
                let retryable = matches!(&error, Error::Sql { message, .. } if is_busy(message));
                last = error;
                if !retryable || delay > Duration::from_secs(4) {
                    break;
                }
                tokio::time::sleep(delay).await;
                delay *= 2;
            }
        }
    }
    admin.close().await;
    Err(last)
}

async fn create_empty(parts: &UrlParts, name: &str) -> Result<()> {
    let admin = admin_pool(parts).await?;
    let result = admin
        .execute(&format!("create database {}", quote_ident(name)))
        .await
        .map(|_| ());
    admin.close().await;
    result
}

fn is_busy(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("being accessed by other users")
        || lower.contains("source database")
        || lower.contains("55006")
}

async fn run_migrator(url: &str, backend: Backend, migrator: &dyn Migrator) -> Result<()> {
    let pool = Pool::open(url, 1, 1).await?;
    let target = MigrationTarget {
        url: url.to_owned(),
        backend,
        pool: pool.clone(),
    };
    let outcome = migrator.migrate(&target).await;
    // The pool has to be *closed*, not merely dropped: PostgreSQL will not copy
    // a template that still has a session connected to it, and dropping a pool
    // only asks its connections to go away.
    pool.close().await;
    outcome.map_err(|error| Error::Migration {
        message: error.to_string(),
    })
}

/// Keeps a user-supplied template name from escaping into a path.
///
/// A template name reaches PostgreSQL as an identifier and SQLite as a
/// *filename*, and the two have different dangerous characters. Anything that is
/// not obviously safe becomes an underscore.
fn sanitise_for_a_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if cleaned.is_empty() {
        "template".to_owned()
    } else {
        cleaned
    }
}

/// The SQLite template file, built once per process per fingerprint.
async fn ensure_sqlite_template(template: &str, migrator: &dyn Migrator) -> Result<PathBuf> {
    let fingerprint = migrator.fingerprint();
    // Deliberately *not* `NAME_PREFIX`: a template is not a per-test database,
    // and `prune_test_files` must not be able to mistake one for the other and
    // delete the thing every test is copying.
    let file = std::env::temp_dir().join(format!(
        "moso-test-template-{}-{:016x}.sqlite",
        sanitise_for_a_filename(template),
        Hasher::of(fingerprint.as_bytes())
    ));
    if template_is_cached(template, &fingerprint) && file.exists() {
        return Ok(file);
    }

    let _gate = template_gate().lock().await;
    if template_is_cached(template, &fingerprint) && file.exists() {
        return Ok(file);
    }

    // A partially built template is worse than none, and two tasks building the
    // same one at once is worse still: each build gets a private name and is
    // published by `rename`, which is atomic on every filesystem the harness
    // runs on.
    let building = std::env::temp_dir().join(format!("{}.building", generate_name()));
    let _ = std::fs::remove_file(&building);
    let url = format!("sqlite://{}", building.display());
    run_migrator(&url, Backend::Sqlite, migrator).await?;
    std::fs::rename(&building, &file).map_err(|error| Error::Io {
        context: format!("publishing the SQLite template `{}`", file.display()),
        message: error.to_string(),
    })?;
    cache_template(template, &fingerprint);
    Ok(file)
}

/// `"name"`, with embedded quotes doubled.
fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// `'text'`, with embedded quotes doubled.
fn quote_literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

async fn drop_database(admin: &Pool, name: &str, force: bool) -> Result<()> {
    if force {
        // Anything still connected is a leak from a crashed run, and leaving it
        // there means the next run inherits the failure.
        let _ = admin
            .execute(&format!(
                "select pg_terminate_backend(pid) from pg_stat_activity where datname = {} and \
                 pid <> pg_backend_pid()",
                quote_literal(name)
            ))
            .await;
    }
    admin
        .execute(&format!(
            "drop database if exists {} with (force)",
            quote_ident(name)
        ))
        .await
        .map(|_| ())
}

// ---------------------------------------------------------------------------
// TestDb
// ---------------------------------------------------------------------------

/// One test's private database.
///
/// ```no_run
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// use moso_test::db::TestDb;
///
/// let db = TestDb::builder().sqlite().migrator(
///     moso_test::db::SqlMigrator::new(["create table widget (id integer primary key)"]),
/// ).acquire().await?;
///
/// db.execute("insert into widget (id) values (1)").await?;
/// assert_eq!(db.fetch_i64("select count(*) from widget").await?, 1);
/// db.close().await;
/// # Ok(())
/// # }
/// ```
pub struct TestDb {
    name: String,
    url: String,
    parts: Option<UrlParts>,
    strategy: Strategy,
    backend: Backend,
    keep: AtomicBool,
    pool: Pool,
    /// The SQLite file to delete, when there is one.
    file: Option<PathBuf>,
    queries: Arc<QueryLog>,
    /// The `moso-orm` handle, opened on first use so that a test that never asks
    /// for one never pays for a second pool.
    orm: tokio::sync::OnceCell<Db>,
    /// Set by [`TestDb::close`] so that `Drop` does not clean up twice.
    closed: AtomicBool,
}

impl TestDb {
    #[allow(
        clippy::too_many_arguments,
        reason = "a private constructor for one caller"
    )]
    fn new(
        name: String,
        url: String,
        parts: Option<UrlParts>,
        strategy: Strategy,
        backend: Backend,
        keep: bool,
        pool: Pool,
        file: Option<PathBuf>,
    ) -> Self {
        Self {
            name,
            url,
            parts,
            strategy,
            backend,
            keep: AtomicBool::new(keep),
            pool,
            file,
            queries: Arc::new(QueryLog::new()),
            orm: tokio::sync::OnceCell::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// A private database with the default strategy, from `DATABASE_URL`.
    ///
    /// # Errors
    ///
    /// [`Error::NoDatabaseUrl`] when `DATABASE_URL` is unset — gate the test
    /// with [`skip_without_database!`](crate::skip_without_database) — and
    /// [`Error::Unsupported`] when the strategy needs a migrator and none was
    /// supplied.
    ///
    /// ```no_run
    /// # async fn example() -> moso_test::db::Result<()> {
    /// let db = moso_test::db::TestDb::acquire().await?;
    /// # let _ = db;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn acquire() -> Result<Self> {
        // With no migrator the only strategy that can work from nothing is
        // `migrate` on an empty schema; `template` would have nothing to stamp.
        // Defaulting to an empty migrator keeps `TestDb::acquire()` meaningful
        // for the many tests whose fixtures are their own DDL.
        Self::builder()
            .migrator(SqlMigrator::default())
            .acquire()
            .await
    }

    /// Configures one.
    ///
    /// ```
    /// let builder = moso_test::db::TestDb::builder();
    /// # let _ = builder;
    /// ```
    #[must_use]
    pub fn builder() -> TestDbBuilder {
        TestDbBuilder::default()
    }

    /// The database's name, or — for SQLite — the stem of its file.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> String {
    /// db.name().to_owned()
    /// # }
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The URL that reaches it. Hand this to the application's configuration.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> String {
    /// db.url().to_owned()
    /// # }
    /// ```
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Which backend it is.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> moso_orm::Backend {
    /// db.backend()
    /// # }
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }

    /// Which [`Strategy`] produced it.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> moso_test::db::Strategy {
    /// db.strategy()
    /// # }
    /// ```
    #[must_use]
    pub const fn strategy(&self) -> Strategy {
        self.strategy
    }

    /// The statements this handle has run, for
    /// [`assert_queries!`](crate::assert_queries).
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> usize {
    /// db.queries().len()
    /// # }
    /// ```
    #[must_use]
    pub fn queries(&self) -> &QueryLog {
        &self.queries
    }

    /// A `moso-orm` configuration pointing at this database.
    ///
    /// The pool is deliberately small, and pinned to a single connection under
    /// [`Strategy::Transaction`] so that the application lands in the same open
    /// transaction the harness will roll back.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> moso_orm::DatabaseConfig {
    /// db.config()
    /// # }
    /// ```
    #[must_use]
    pub fn config(&self) -> DatabaseConfig {
        let maximum = if self.strategy == Strategy::Transaction {
            1
        } else {
            2
        };
        DatabaseConfig::from_url(self.url.clone())
            .with_application_name(format!("moso-test {}", self.name))
            .with_acquire_timeout(Duration::from_secs(10))
            .with_max_connections(maximum)
            .with_min_connections(0)
    }

    /// A `moso-orm` handle on this database, opened once and shared.
    ///
    /// # Errors
    ///
    /// Whatever `moso_orm::Db::connect` returns, rendered as [`Error::Sql`].
    ///
    /// ```no_run
    /// # async fn example(db: &moso_test::db::TestDb) -> moso_test::db::Result<()> {
    /// let orm = db.orm().await?;
    /// assert_eq!(orm.backend(), db.backend());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn orm(&self) -> Result<&Db> {
        self.orm
            .get_or_try_init(|| async {
                Db::connect(&self.config())
                    .await
                    .map_err(|error| Error::Sql {
                        context: format!("opening a moso-orm pool on `{}`", self.name),
                        message: error.to_string(),
                    })
            })
            .await
    }

    /// Runs one statement, recording it in [`TestDb::queries`].
    ///
    /// This is the fixture and assertion hatch — non-negotiable N8 in test
    /// clothing. Query construction still belongs in the application.
    ///
    /// # Errors
    ///
    /// [`Error::Sql`] when the server refuses it.
    ///
    /// ```no_run
    /// # async fn example(db: &moso_test::db::TestDb) -> moso_test::db::Result<()> {
    /// db.execute("insert into widget (id) values (1)").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(&self, sql: &str) -> Result<u64> {
        let started = std::time::Instant::now();
        let outcome = self.pool.execute(sql).await;
        self.queries.record(RecordedStatement {
            sql: sql.trim().to_owned(),
            rows_affected: outcome.as_ref().ok().copied(),
            rows_returned: None,
            elapsed: Some(started.elapsed()),
        });
        outcome
    }

    /// Runs a script of statements on one connection.
    ///
    /// # Errors
    ///
    /// [`Error::Sql`] when the server refuses any of it.
    ///
    /// ```no_run
    /// # async fn example(db: &moso_test::db::TestDb) -> moso_test::db::Result<()> {
    /// db.execute_batch("create table a (id int); create table b (id int);").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute_batch(&self, sql: &str) -> Result<()> {
        let started = std::time::Instant::now();
        let outcome = self.pool.execute_batch(sql).await;
        self.queries.record(RecordedStatement {
            sql: sql.trim().to_owned(),
            rows_affected: None,
            rows_returned: None,
            elapsed: Some(started.elapsed()),
        });
        outcome
    }

    /// Reads one integer — a `count(*)`, an `id`, a `max(version)`.
    ///
    /// # Errors
    ///
    /// [`Error::Sql`] when the statement fails or returns no row.
    ///
    /// ```no_run
    /// # async fn example(db: &moso_test::db::TestDb) -> moso_test::db::Result<()> {
    /// assert_eq!(db.fetch_i64("select count(*) from widget").await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_i64(&self, sql: &str) -> Result<i64> {
        let started = std::time::Instant::now();
        let outcome = self.pool.fetch_i64(sql).await;
        self.queries.record(RecordedStatement {
            sql: sql.trim().to_owned(),
            rows_affected: None,
            rows_returned: Some(u64::from(outcome.is_ok())),
            elapsed: Some(started.elapsed()),
        });
        outcome
    }

    /// Reads the first column of every row as text.
    ///
    /// # Errors
    ///
    /// [`Error::Sql`] when the statement fails or the column is not text.
    ///
    /// ```no_run
    /// # async fn example(db: &moso_test::db::TestDb) -> moso_test::db::Result<()> {
    /// let names = db.fetch_text_column("select name from widget order by name").await?;
    /// assert!(names.is_empty());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_text_column(&self, sql: &str) -> Result<Vec<String>> {
        let started = std::time::Instant::now();
        let outcome = self.pool.fetch_text_column(sql).await;
        self.queries.record(RecordedStatement {
            sql: sql.trim().to_owned(),
            rows_affected: None,
            rows_returned: outcome
                .as_ref()
                .ok()
                .map(|rows| u64::try_from(rows.len()).unwrap_or(u64::MAX)),
            elapsed: Some(started.elapsed()),
        });
        outcome
    }

    /// How many rows are in `table`, by name.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidName`] for a name that is not an identifier, and
    /// [`Error::Sql`] when the table does not exist.
    ///
    /// ```no_run
    /// # async fn example(db: &moso_test::db::TestDb) -> moso_test::db::Result<()> {
    /// assert_eq!(db.count("widget").await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn count(&self, table: &str) -> Result<i64> {
        validate_name(table)?;
        self.fetch_i64(&format!("select count(*) from {}", quote_ident(table)))
            .await
    }

    /// Keep this database when the handle is dropped, and print its URL.
    ///
    /// Call it from a failing test — or set `MOSO_TEST_KEEP_DB=1` for the whole
    /// run — when the interesting thing is the state the failure left behind.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) {
    /// db.keep();
    /// # }
    /// ```
    pub fn keep(&self) {
        self.keep.store(true, Ordering::Relaxed);
    }

    /// Whether the database will survive the handle.
    ///
    /// ```no_run
    /// # fn example(db: &moso_test::db::TestDb) -> bool {
    /// db.is_kept()
    /// # }
    /// ```
    #[must_use]
    pub fn is_kept(&self) -> bool {
        self.keep.load(Ordering::Relaxed)
    }

    /// Closes the pool and drops the database, and waits for both.
    ///
    /// `Drop` does the same thing on a background thread. Call this when the
    /// test wants to be sure — for instance because it is about to assert that
    /// the database is gone.
    ///
    /// ```no_run
    /// # async fn example(db: moso_test::db::TestDb) {
    /// db.close().await;
    /// # }
    /// ```
    pub async fn close(self) {
        self.closed.store(true, Ordering::Relaxed);
        let task = self.cleanup_task();
        if let Some(orm) = self.orm.get() {
            orm.close().await;
        }
        match task {
            // The rollback has to run *on* the pinned connection, so this one
            // closes the pool itself, afterwards.
            Some(task @ CleanupTask::Rollback { .. }) => task.run().await,
            other => {
                // Everything else deletes the database the pool is connected
                // to, so the pool goes first.
                self.pool.close().await;
                if let Some(task) = other {
                    task.run().await;
                }
            }
        }
    }

    /// What has to happen when this handle goes away, or `None` when nothing
    /// does.
    fn cleanup_task(&self) -> Option<CleanupTask> {
        if self.is_kept() {
            eprintln!("moso-test: keeping {} — {}", self.name, redact(&self.url));
            return None;
        }
        match self.strategy {
            Strategy::Transaction => Some(CleanupTask::Rollback {
                pool: self.pool.clone(),
            }),
            Strategy::Template | Strategy::Migrate => match self.backend {
                Backend::Postgres => self.parts.clone().map(|parts| CleanupTask::DropDatabase {
                    parts,
                    name: self.name.clone(),
                }),
                _ => self
                    .file
                    .clone()
                    .map(|path| CleanupTask::DeleteFile { path }),
            },
        }
    }
}

impl fmt::Debug for TestDb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TestDb")
            .field("name", &self.name)
            .field("backend", &self.backend)
            .field("strategy", &self.strategy)
            .field("kept", &self.is_kept())
            .finish_non_exhaustive()
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        if self.closed.load(Ordering::Relaxed) {
            return;
        }
        if let Some(task) = self.cleanup_task() {
            cleaner().submit(task);
        }
    }
}

// ---------------------------------------------------------------------------
// The cleaner
// ---------------------------------------------------------------------------

/// One database to get rid of.
enum CleanupTask {
    /// Undo a `Strategy::Transaction` test.
    Rollback { pool: Pool },
    /// Drop a per-test PostgreSQL database.
    DropDatabase { parts: UrlParts, name: String },
    /// Delete a per-test SQLite file.
    DeleteFile { path: PathBuf },
}

impl CleanupTask {
    async fn run(self) {
        match self {
            Self::Rollback { pool } => {
                let _ = pool.execute("rollback").await;
                pool.close().await;
            }
            Self::DropDatabase { parts, name } => {
                let Ok(admin) = admin_pool(&parts).await else {
                    return;
                };
                let _ = drop_database(&admin, &name, true).await;
                admin.close().await;
            }
            Self::DeleteFile { path } => {
                let _ = std::fs::remove_file(&path);
                // SQLite's WAL and shared-memory companions.
                for extra in ["-wal", "-shm"] {
                    let mut companion = path.clone().into_os_string();
                    companion.push(extra);
                    let _ = std::fs::remove_file(PathBuf::from(companion));
                }
            }
        }
    }
}

/// Runs cleanup off the test thread.
///
/// `Drop` cannot await, and blocking a test for the fifty milliseconds a
/// `DROP DATABASE` takes — a hundred times over — is exactly the cost this whole
/// module exists to avoid. So the work is queued here and a dedicated thread
/// with its own runtime does it. A process killed before the queue drains leaks
/// databases, which is why [`prune_test_databases`] exists.
struct Cleaner {
    queue: Mutex<Vec<CleanupTask>>,
    signal: Condvar,
    outstanding: Mutex<usize>,
    drained: Condvar,
}

impl Cleaner {
    fn submit(&self, task: CleanupTask) {
        if let Ok(mut outstanding) = self.outstanding.lock() {
            *outstanding += 1;
        }
        if let Ok(mut queue) = self.queue.lock() {
            queue.push(task);
        }
        self.signal.notify_one();
    }

    /// Blocks until the queue is empty. Used by [`drain_cleanup`] and by the
    /// crate's own tests, which assert that a database really did go away.
    fn drain(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        let Ok(mut outstanding) = self.outstanding.lock() else {
            return false;
        };
        while *outstanding > 0 {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return false;
            }
            let (guard, result) = match self.drained.wait_timeout(outstanding, remaining) {
                Ok(pair) => pair,
                Err(_) => return false,
            };
            outstanding = guard;
            if result.timed_out() && *outstanding > 0 {
                return false;
            }
        }
        true
    }
}

fn cleaner() -> &'static Cleaner {
    static CLEANER: OnceLock<&'static Cleaner> = OnceLock::new();
    CLEANER.get_or_init(|| {
        let cleaner: &'static Cleaner = Box::leak(Box::new(Cleaner {
            queue: Mutex::new(Vec::new()),
            signal: Condvar::new(),
            outstanding: Mutex::new(0),
            drained: Condvar::new(),
        }));
        std::thread::Builder::new()
            .name("moso-test-db-cleaner".to_owned())
            .spawn(move || {
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return;
                };
                loop {
                    let batch = {
                        let Ok(mut queue) = cleaner.queue.lock() else {
                            return;
                        };
                        while queue.is_empty() {
                            let Ok(next) = cleaner.signal.wait(queue) else {
                                return;
                            };
                            queue = next;
                        }
                        core::mem::take(&mut *queue)
                    };
                    let count = batch.len();
                    runtime.block_on(async {
                        for task in batch {
                            task.run().await;
                        }
                    });
                    if let Ok(mut outstanding) = cleaner.outstanding.lock() {
                        *outstanding = outstanding.saturating_sub(count);
                        if *outstanding == 0 {
                            cleaner.drained.notify_all();
                        }
                    }
                }
            })
            .ok();
        cleaner
    })
}

/// Waits for every dropped [`TestDb`] to finish being cleaned up.
///
/// Cleanup normally happens on a background thread and nobody waits for it. A
/// test that asserts about the *absence* of a database has to.
///
/// Returns `false` if the queue was still not empty after `timeout`.
///
/// ```
/// use core::time::Duration;
///
/// // Nothing outstanding: returns immediately.
/// assert!(moso_test::db::drain_cleanup(Duration::from_secs(1)));
/// ```
#[must_use]
pub fn drain_cleanup(timeout: Duration) -> bool {
    cleaner().drain(timeout)
}

// ---------------------------------------------------------------------------
// Pruning
// ---------------------------------------------------------------------------

/// What [`prune_test_databases`] should get rid of.
///
/// ```
/// use core::time::Duration;
/// use moso_test::db::PruneOptions;
///
/// let options = PruneOptions::default().older_than(Duration::from_secs(0)).dry_run();
/// assert!(options.is_dry_run());
/// assert_eq!(options.age().as_secs(), 0);
/// ```
#[derive(Clone, Debug)]
pub struct PruneOptions {
    older_than: Duration,
    dry_run: bool,
    include_templates: bool,
    force: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            older_than: DEFAULT_PRUNE_AGE,
            dry_run: false,
            include_templates: false,
            force: false,
        }
    }
}

impl PruneOptions {
    /// Leave anything younger than this alone. Default one hour.
    ///
    /// ```
    /// # use core::time::Duration;
    /// let options = moso_test::db::PruneOptions::default().older_than(Duration::from_secs(60));
    /// assert_eq!(options.age().as_secs(), 60);
    /// ```
    #[must_use]
    pub const fn older_than(mut self, age: Duration) -> Self {
        self.older_than = age;
        self
    }

    /// Report what would go, and delete nothing.
    ///
    /// ```
    /// assert!(moso_test::db::PruneOptions::default().dry_run().is_dry_run());
    /// ```
    #[must_use]
    pub const fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Also drop the template databases, so the next run rebuilds them.
    ///
    /// ```
    /// assert!(moso_test::db::PruneOptions::default().with_templates().includes_templates());
    /// ```
    #[must_use]
    pub const fn with_templates(mut self) -> Self {
        self.include_templates = true;
        self
    }

    /// Disconnect anything still connected, instead of skipping it.
    ///
    /// ```
    /// assert!(moso_test::db::PruneOptions::default().force().is_forced());
    /// ```
    #[must_use]
    pub const fn force(mut self) -> Self {
        self.force = true;
        self
    }

    /// The configured age.
    ///
    /// ```
    /// assert_eq!(
    ///     moso_test::db::PruneOptions::default().age(),
    ///     moso_test::db::DEFAULT_PRUNE_AGE,
    /// );
    /// ```
    #[must_use]
    pub const fn age(&self) -> Duration {
        self.older_than
    }

    /// Whether nothing will actually be deleted.
    ///
    /// ```
    /// assert!(!moso_test::db::PruneOptions::default().is_dry_run());
    /// ```
    #[must_use]
    pub const fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    /// Whether templates are included.
    ///
    /// ```
    /// assert!(!moso_test::db::PruneOptions::default().includes_templates());
    /// ```
    #[must_use]
    pub const fn includes_templates(&self) -> bool {
        self.include_templates
    }

    /// Whether live sessions will be terminated.
    ///
    /// ```
    /// assert!(!moso_test::db::PruneOptions::default().is_forced());
    /// ```
    #[must_use]
    pub const fn is_forced(&self) -> bool {
        self.force
    }

    /// Whether a database named `name`, created at `created`, is in scope.
    ///
    /// This is the whole safety argument for `moso db prune-test` in one
    /// function. Three rules, and all three have to pass:
    ///
    /// 1. the name **parses** as one this module generated — prefix, creation
    ///    time, process id and ordinal, all in base 36. `moso_test_fixtures`
    ///    does not parse and is never touched;
    /// 2. it is older than [`PruneOptions::older_than`];
    /// 3. it does not belong to **this** process, unless
    ///    [`PruneOptions::force`] is set. Pruning from inside a running suite
    ///    must not delete the database the next test over is using — which is
    ///    otherwise exactly what an `older_than(ZERO)` prune does.
    ///
    /// ```
    /// use core::time::Duration;
    /// use std::time::SystemTime;
    /// use moso_test::db::PruneOptions;
    ///
    /// let options = PruneOptions::default().older_than(Duration::from_secs(0));
    /// let now = SystemTime::now();
    ///
    /// assert!(options.selects("moso_test_l9k2j_1x_0", now));
    /// assert!(!options.selects("moso_test_template", now));
    /// assert!(!options.selects("production", now));
    /// assert!(!options.selects("moso_testing", now));
    /// ```
    #[must_use]
    pub fn selects(&self, name: &str, now: SystemTime) -> bool {
        if self.include_templates && name.ends_with("_template") {
            return true;
        }
        if !self.force && owning_process(name) == Some(u64::from(std::process::id())) {
            return false;
        }
        let Some(created) = created_at(name) else {
            return false;
        };
        now.duration_since(created).unwrap_or_default() >= self.older_than
    }
}

/// What [`prune_test_databases`] did.
///
/// ```
/// use moso_test::db::Pruned;
///
/// let pruned = Pruned::default();
/// assert!(pruned.dropped().is_empty());
/// assert!(pruned.summary().contains('0'));
/// ```
#[derive(Clone, Debug, Default)]
pub struct Pruned {
    dropped: Vec<String>,
    skipped: Vec<(String, String)>,
}

impl Pruned {
    /// The databases that went, in the order they went.
    ///
    /// ```
    /// assert!(moso_test::db::Pruned::default().dropped().is_empty());
    /// ```
    #[must_use]
    pub fn dropped(&self) -> &[String] {
        &self.dropped
    }

    /// The ones that were left, each with the reason.
    ///
    /// ```
    /// assert!(moso_test::db::Pruned::default().skipped().is_empty());
    /// ```
    #[must_use]
    pub fn skipped(&self) -> &[(String, String)] {
        &self.skipped
    }

    /// A line for the CLI to print.
    ///
    /// ```
    /// assert_eq!(moso_test::db::Pruned::default().summary(), "dropped 0 test databases");
    /// ```
    #[must_use]
    pub fn summary(&self) -> String {
        let mut out = format!("dropped {} test databases", self.dropped.len());
        if !self.skipped.is_empty() {
            let _ = write!(out, ", left {} alone", self.skipped.len());
        }
        out
    }
}

/// Drops the test databases a crashed run left behind.
///
/// This is the library half of `moso db prune-test`. It is deliberately
/// conservative: a database is dropped only if its name *parses* as one this
/// module generated — prefix, creation time, process id and ordinal, all in
/// base 36 — and only if it is older than [`PruneOptions::older_than`]. A
/// database called `moso_test_fixtures` is not touched, because it does not
/// parse.
///
/// # Errors
///
/// [`Error::UnsupportedUrl`] for a URL that is not PostgreSQL — SQLite test
/// databases are files, and [`prune_test_files`] handles those — and
/// [`Error::Sql`] when the server refuses.
///
/// ```no_run
/// # async fn example() -> moso_test::db::Result<()> {
/// use moso_test::db::{PruneOptions, prune_test_databases};
///
/// let pruned = prune_test_databases(
///     "postgres://moso:moso@localhost:55433/moso_test",
///     &PruneOptions::default(),
/// ).await?;
/// println!("{}", pruned.summary());
/// # Ok(())
/// # }
/// ```
pub async fn prune_test_databases(url: &str, options: &PruneOptions) -> Result<Pruned> {
    if backend_of(url)? != Backend::Postgres {
        return Err(Error::Unsupported {
            what: format!(
                "`{}` is not a PostgreSQL server; SQLite test databases are files\n  help: call \
                 `prune_test_files` instead",
                redact(url)
            ),
        });
    }
    let parts = UrlParts::parse(url).ok_or_else(|| Error::UnsupportedUrl {
        url: redact(url).into_owned(),
    })?;
    let admin = admin_pool(&parts).await?;

    let candidates = admin
        .fetch_text_column(&format!(
            "select datname from pg_database where datistemplate = false and datname like {} \
             order by datname",
            quote_literal(&format!("{NAME_PREFIX}%"))
        ))
        .await?;

    let now = SystemTime::now();
    let mut pruned = Pruned::default();
    for name in candidates {
        if !options.selects(&name, now) {
            pruned
                .skipped
                .push((name, "not a stale generated name".to_owned()));
            continue;
        }
        if !options.force {
            let busy = admin
                .fetch_i64(&format!(
                    "select count(*) from pg_stat_activity where datname = {}",
                    quote_literal(&name)
                ))
                .await
                .unwrap_or(0);
            if busy > 0 {
                pruned
                    .skipped
                    .push((name, format!("{busy} session(s) still connected")));
                continue;
            }
        }
        if options.dry_run {
            pruned.dropped.push(name);
            continue;
        }
        match drop_database(&admin, &name, options.force).await {
            Ok(()) => pruned.dropped.push(name),
            Err(error) => pruned.skipped.push((name, error.to_string())),
        }
    }

    admin.close().await;
    Ok(pruned)
}

/// The same, for SQLite test databases, which are files in the temporary
/// directory.
///
/// # Errors
///
/// [`Error::Io`] when the temporary directory cannot be read.
///
/// ```no_run
/// # fn example() -> moso_test::db::Result<()> {
/// let pruned = moso_test::db::prune_test_files(&moso_test::db::PruneOptions::default())?;
/// println!("{}", pruned.summary());
/// # Ok(())
/// # }
/// ```
pub fn prune_test_files(options: &PruneOptions) -> Result<Pruned> {
    let directory = std::env::temp_dir();
    let entries = std::fs::read_dir(&directory).map_err(|error| Error::Io {
        context: format!("reading `{}`", directory.display()),
        message: error.to_string(),
    })?;
    let now = SystemTime::now();
    let mut pruned = Pruned::default();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
            continue;
        };
        if path
            .extension()
            .is_none_or(|ext| ext != "sqlite" && ext != "building")
        {
            continue;
        }
        if !options.selects(stem, now) {
            continue;
        }
        if options.dry_run {
            pruned.dropped.push(stem.to_owned());
            continue;
        }
        match std::fs::remove_file(&path) {
            Ok(()) => pruned.dropped.push(stem.to_owned()),
            Err(error) => pruned.skipped.push((stem.to_owned(), error.to_string())),
        }
    }
    Ok(pruned)
}

// ---------------------------------------------------------------------------
// Statement counting
// ---------------------------------------------------------------------------

/// One statement that ran.
///
/// ```
/// use moso_test::db::RecordedStatement;
///
/// let statement = RecordedStatement::sql("select 1");
/// assert_eq!(statement.sql_text(), "select 1");
/// assert!(statement.is_transaction_control() == false);
/// ```
#[derive(Clone, Debug)]
pub struct RecordedStatement {
    sql: String,
    rows_affected: Option<u64>,
    rows_returned: Option<u64>,
    elapsed: Option<Duration>,
}

impl RecordedStatement {
    /// A statement with nothing but its text.
    ///
    /// ```
    /// assert_eq!(moso_test::db::RecordedStatement::sql("select 1").sql_text(), "select 1");
    /// ```
    #[must_use]
    pub fn sql(text: impl Into<String>) -> Self {
        Self {
            sql: text.into().trim().to_owned(),
            rows_affected: None,
            rows_returned: None,
            elapsed: None,
        }
    }

    /// The text, as it was sent.
    ///
    /// ```
    /// # use moso_test::db::RecordedStatement;
    /// assert_eq!(RecordedStatement::sql(" select 1 ").sql_text(), "select 1");
    /// ```
    #[must_use]
    pub fn sql_text(&self) -> &str {
        &self.sql
    }

    /// How many rows it changed, when the driver said.
    ///
    /// ```
    /// # use moso_test::db::RecordedStatement;
    /// assert!(RecordedStatement::sql("select 1").rows_affected().is_none());
    /// ```
    #[must_use]
    pub const fn rows_affected(&self) -> Option<u64> {
        self.rows_affected
    }

    /// How many rows it returned, when the driver said.
    ///
    /// ```
    /// # use moso_test::db::RecordedStatement;
    /// assert!(RecordedStatement::sql("select 1").rows_returned().is_none());
    /// ```
    #[must_use]
    pub const fn rows_returned(&self) -> Option<u64> {
        self.rows_returned
    }

    /// How long it took, when the recorder measured it.
    ///
    /// ```
    /// # use moso_test::db::RecordedStatement;
    /// assert!(RecordedStatement::sql("select 1").elapsed().is_none());
    /// ```
    #[must_use]
    pub const fn elapsed(&self) -> Option<Duration> {
        self.elapsed
    }

    /// Whether this is `begin`, `commit`, `rollback`, `savepoint` or `release`.
    ///
    /// [`assert_queries!`](crate::assert_queries) ignores these by default: a
    /// count that changes because the pool decided to open a transaction is a
    /// count nobody can assert on.
    ///
    /// ```
    /// use moso_test::db::RecordedStatement;
    ///
    /// assert!(RecordedStatement::sql("BEGIN").is_transaction_control());
    /// assert!(RecordedStatement::sql("release savepoint s1").is_transaction_control());
    /// assert!(!RecordedStatement::sql("select 1").is_transaction_control());
    /// ```
    #[must_use]
    pub fn is_transaction_control(&self) -> bool {
        let head = self
            .sql
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches(';')
            .to_ascii_lowercase();
        matches!(
            head.as_str(),
            "begin" | "commit" | "rollback" | "savepoint" | "release" | "start" | "end"
        )
    }

    /// The statement, shortened for a report.
    ///
    /// ```
    /// use moso_test::db::RecordedStatement;
    ///
    /// let long = RecordedStatement::sql("select 1, ".repeat(40));
    /// assert!(long.summary().len() <= 100);
    /// ```
    #[must_use]
    pub fn summary(&self) -> String {
        let flat = self.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        if flat.chars().count() <= 96 {
            flat
        } else {
            let mut head: String = flat.chars().take(93).collect();
            head.push_str("...");
            head
        }
    }
}

impl fmt::Display for RecordedStatement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.summary())
    }
}

/// A reading of a [`QueryLog`], for comparing against a later one.
///
/// ```
/// use moso_test::db::QueryLog;
///
/// let log = QueryLog::new();
/// let mark = log.mark();
/// assert_eq!(mark.value(), 0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueryMark(usize);

impl QueryMark {
    /// How many statements had been recorded when the mark was taken.
    ///
    /// ```
    /// assert_eq!(moso_test::db::QueryLog::new().mark().value(), 0);
    /// ```
    #[must_use]
    pub const fn value(self) -> usize {
        self.0
    }
}

/// The statements a [`TestDb`] has run, in order.
///
/// ```
/// use moso_test::db::{QueryLog, RecordedStatement};
///
/// let log = QueryLog::new();
/// let mark = log.mark();
/// log.record(RecordedStatement::sql("select 1"));
/// log.record(RecordedStatement::sql("select 2"));
///
/// assert_eq!(log.count_since(mark), 2);
/// assert_eq!(log.since(mark)[0].sql_text(), "select 1");
/// ```
#[derive(Debug)]
pub struct QueryLog {
    statements: Mutex<Vec<RecordedStatement>>,
    limit: usize,
    /// How many were dropped to stay under the limit, so that a mark taken
    /// before an eviction still means something.
    evicted: AtomicU64,
}

impl Default for QueryLog {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryLog {
    /// How many statements a log keeps before it forgets the oldest.
    ///
    /// ```
    /// assert_eq!(moso_test::db::QueryLog::DEFAULT_LIMIT, 4096);
    /// ```
    pub const DEFAULT_LIMIT: usize = 4096;

    /// An empty log.
    ///
    /// ```
    /// assert!(moso_test::db::QueryLog::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::with_limit(Self::DEFAULT_LIMIT)
    }

    /// An empty log that keeps at most `limit` statements.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, RecordedStatement};
    ///
    /// let log = QueryLog::with_limit(2);
    /// for sql in ["a", "b", "c"] {
    ///     log.record(RecordedStatement::sql(sql));
    /// }
    /// assert_eq!(log.len(), 2);
    /// ```
    #[must_use]
    pub fn with_limit(limit: usize) -> Self {
        Self {
            statements: Mutex::new(Vec::new()),
            limit: limit.max(1),
            evicted: AtomicU64::new(0),
        }
    }

    /// Records one.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, RecordedStatement};
    ///
    /// let log = QueryLog::new();
    /// log.record(RecordedStatement::sql("select 1"));
    /// assert_eq!(log.len(), 1);
    /// ```
    pub fn record(&self, statement: RecordedStatement) {
        if let Ok(mut statements) = self.statements.lock() {
            statements.push(statement);
            if statements.len() > self.limit {
                let excess = statements.len() - self.limit;
                statements.drain(..excess);
                self.evicted
                    .fetch_add(u64::try_from(excess).unwrap_or(0), Ordering::Relaxed);
            }
        }
    }

    /// Records one from its text.
    ///
    /// ```
    /// use moso_test::db::QueryLog;
    ///
    /// let log = QueryLog::new();
    /// log.record_sql("select 1");
    /// assert_eq!(log.len(), 1);
    /// ```
    pub fn record_sql(&self, sql: impl Into<String>) {
        self.record(RecordedStatement::sql(sql));
    }

    /// A reading to compare a later one against.
    ///
    /// ```
    /// assert_eq!(moso_test::db::QueryLog::new().mark().value(), 0);
    /// ```
    #[must_use]
    pub fn mark(&self) -> QueryMark {
        QueryMark(self.total())
    }

    /// How many statements have been recorded, ever.
    ///
    /// ```
    /// assert_eq!(moso_test::db::QueryLog::new().total(), 0);
    /// ```
    #[must_use]
    pub fn total(&self) -> usize {
        let kept = self
            .statements
            .lock()
            .map_or(0, |statements| statements.len());
        kept + usize::try_from(self.evicted.load(Ordering::Relaxed)).unwrap_or(usize::MAX)
    }

    /// How many are being kept.
    ///
    /// ```
    /// assert_eq!(moso_test::db::QueryLog::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.statements
            .lock()
            .map_or(0, |statements| statements.len())
    }

    /// Whether any are.
    ///
    /// ```
    /// assert!(moso_test::db::QueryLog::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The statements recorded since `mark`.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, RecordedStatement};
    ///
    /// let log = QueryLog::new();
    /// log.record(RecordedStatement::sql("before"));
    /// let mark = log.mark();
    /// log.record(RecordedStatement::sql("after"));
    ///
    /// assert_eq!(log.since(mark).len(), 1);
    /// assert_eq!(log.since(mark)[0].sql_text(), "after");
    /// ```
    #[must_use]
    pub fn since(&self, mark: QueryMark) -> Vec<RecordedStatement> {
        let evicted = usize::try_from(self.evicted.load(Ordering::Relaxed)).unwrap_or(usize::MAX);
        let Ok(statements) = self.statements.lock() else {
            return Vec::new();
        };
        let start = mark.0.saturating_sub(evicted).min(statements.len());
        statements[start..].to_vec()
    }

    /// How many statements have run since `mark`.
    ///
    /// ```
    /// use moso_test::db::QueryLog;
    ///
    /// let log = QueryLog::new();
    /// let mark = log.mark();
    /// log.record_sql("select 1");
    /// assert_eq!(log.count_since(mark), 1);
    /// ```
    #[must_use]
    pub fn count_since(&self, mark: QueryMark) -> usize {
        self.total().saturating_sub(mark.0)
    }

    /// Forgets everything.
    ///
    /// ```
    /// use moso_test::db::QueryLog;
    ///
    /// let log = QueryLog::new();
    /// log.record_sql("select 1");
    /// log.clear();
    /// assert!(log.is_empty());
    /// ```
    pub fn clear(&self) {
        if let Ok(mut statements) = self.statements.lock() {
            statements.clear();
        }
    }

    /// Adds every `sqlx::query` line in `records` that is not already here.
    ///
    /// This is how statements the *application* ran become visible to
    /// [`assert_queries!`](crate::assert_queries): `moso-test` captures the
    /// server's `tracing` output already, and `sqlx` logs every statement it
    /// executes under the target `sqlx::query`.
    ///
    /// Returns how many were added.
    ///
    /// ```
    /// use moso_test::db::QueryLog;
    ///
    /// let log = QueryLog::new();
    /// assert_eq!(log.absorb(&[]), 0);
    /// ```
    pub fn absorb(&self, records: &[LogRecord]) -> usize {
        let mut added = 0;
        for record in records {
            if let Some(statement) = statement_from_log(record) {
                self.record(statement);
                added += 1;
            }
        }
        added
    }
}

/// The `tracing` target `sqlx` logs every executed statement under.
///
/// ```
/// assert_eq!(moso_test::db::SQLX_QUERY_TARGET, "sqlx::query");
/// ```
pub const SQLX_QUERY_TARGET: &str = "sqlx::query";

/// Turns one captured log line into a statement, when it is one.
///
/// `sqlx` records the first four words under `summary` and, only when the
/// statement is longer than that, the whole text under `db.statement`.
fn statement_from_log(record: &LogRecord) -> Option<RecordedStatement> {
    if record.target != SQLX_QUERY_TARGET {
        return None;
    }
    let field = |name: &str| {
        record
            .fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.trim())
            .filter(|value| !value.is_empty())
    };
    let sql = field("db.statement")
        .or_else(|| field("summary"))
        .unwrap_or_default();
    if sql.is_empty() {
        return None;
    }
    Some(RecordedStatement {
        sql: sql.trim_end_matches(['…', ' ']).trim().to_owned(),
        rows_affected: field("rows_affected").and_then(|value| value.parse().ok()),
        rows_returned: field("rows_returned").and_then(|value| value.parse().ok()),
        elapsed: None,
    })
}

/// Something [`assert_queries!`](crate::assert_queries) can count statements on.
///
/// Implemented for [`TestDb`], [`QueryLog`] and
/// [`TestApp`](crate::TestApp) — the last of which reads the statements the
/// *server* ran, out of the log lines the harness already captures.
///
/// ```
/// use moso_test::db::{QueryLog, QuerySource};
///
/// let log = QueryLog::new();
/// let scope = QuerySource::begin_queries(&log);
/// log.record_sql("select 1");
/// assert_eq!(scope.finish().count(), 1);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot count statements",
    label = "not a statement source",
    note = "`assert_queries!` takes a `&TestDb`, a `&QueryLog` or a `&TestApp`",
    note = "help: inside a handler test, pass the app: `assert_queries!(&app, 2, {{ .. }})`",
    note = "help: for statements the test itself runs, pass the database: \
            `assert_queries!(&db, 2, {{ .. }})`"
)]
pub trait QuerySource {
    /// Opens a counting window.
    ///
    /// ```
    /// # use moso_test::db::{QueryLog, QuerySource};
    /// fn open(log: &QueryLog) -> moso_test::db::QueryScope<'_> {
    ///     log.begin_queries()
    /// }
    /// ```
    fn begin_queries(&self) -> QueryScope<'_>;
}

/// Where a [`QueryScope`] reads its statements from.
enum ScopeOrigin<'a> {
    /// A log the harness owns.
    Log(&'a QueryLog),
    /// The captured server logs of a test app.
    Captured(&'a LogAssertions),
}

/// An open counting window. Produced by [`QuerySource::begin_queries`].
///
/// ```
/// use moso_test::db::{QueryLog, QuerySource};
///
/// let log = QueryLog::new();
/// let scope = log.begin_queries();
/// log.record_sql("select 1");
/// assert_eq!(scope.finish().count(), 1);
/// ```
pub struct QueryScope<'a> {
    origin: ScopeOrigin<'a>,
    mark: usize,
    include_transaction_control: bool,
}

impl fmt::Debug for QueryScope<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryScope")
            .field("mark", &self.mark)
            .finish_non_exhaustive()
    }
}

impl<'a> QueryScope<'a> {
    /// Count `begin`/`commit`/`rollback` too, which the window ignores by
    /// default.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, QuerySource};
    ///
    /// let log = QueryLog::new();
    /// let scope = log.begin_queries().including_transaction_control();
    /// log.record_sql("begin");
    /// log.record_sql("select 1");
    /// assert_eq!(scope.finish().count(), 2);
    /// ```
    #[must_use]
    pub const fn including_transaction_control(mut self) -> Self {
        self.include_transaction_control = true;
        self
    }

    /// Closes the window.
    ///
    /// ```
    /// # use moso_test::db::{QueryLog, QuerySource};
    /// let log = QueryLog::new();
    /// assert_eq!(log.begin_queries().finish().count(), 0);
    /// ```
    #[must_use]
    pub fn finish(self) -> QueryReport {
        let statements = match self.origin {
            ScopeOrigin::Log(log) => log.since(QueryMark(self.mark)),
            ScopeOrigin::Captured(logs) => {
                let records = logs.records();
                records
                    .iter()
                    .skip(self.mark)
                    .filter_map(statement_from_log)
                    .collect()
            }
        };
        let capture_available = match self.origin {
            ScopeOrigin::Log(_) => true,
            ScopeOrigin::Captured(logs) => logs.is_capturing(),
        };
        let statements = if self.include_transaction_control {
            statements
        } else {
            statements
                .into_iter()
                .filter(|statement| !statement.is_transaction_control())
                .collect()
        };
        QueryReport {
            statements,
            capture_available,
        }
    }
}

impl QuerySource for QueryLog {
    fn begin_queries(&self) -> QueryScope<'_> {
        QueryScope {
            origin: ScopeOrigin::Log(self),
            mark: self.total(),
            include_transaction_control: false,
        }
    }
}

impl QuerySource for TestDb {
    fn begin_queries(&self) -> QueryScope<'_> {
        self.queries.begin_queries()
    }
}

impl QuerySource for crate::TestApp {
    fn begin_queries(&self) -> QueryScope<'_> {
        let logs = self.logs();
        QueryScope {
            origin: ScopeOrigin::Captured(logs),
            mark: logs.len(),
            include_transaction_control: false,
        }
    }
}

#[diagnostic::do_not_recommend]
impl<T: QuerySource + ?Sized> QuerySource for &T {
    fn begin_queries(&self) -> QueryScope<'_> {
        (**self).begin_queries()
    }
}

#[diagnostic::do_not_recommend]
impl<T: QuerySource + ?Sized> QuerySource for Arc<T> {
    fn begin_queries(&self) -> QueryScope<'_> {
        (**self).begin_queries()
    }
}

/// What ran inside an [`assert_queries!`](crate::assert_queries) block.
///
/// ```
/// use moso_test::db::{QueryLog, QuerySource};
///
/// let log = QueryLog::new();
/// let scope = log.begin_queries();
/// log.record_sql("select 1");
/// log.record_sql("select 1");
///
/// let report = scope.finish();
/// assert_eq!(report.count(), 2);
/// assert_eq!(report.most_repeated().unwrap().1, 2);
/// ```
#[derive(Clone, Debug)]
pub struct QueryReport {
    statements: Vec<RecordedStatement>,
    capture_available: bool,
}

impl QueryReport {
    /// How many statements ran.
    ///
    /// ```
    /// # use moso_test::db::{QueryLog, QuerySource};
    /// assert_eq!(QueryLog::new().begin_queries().finish().count(), 0);
    /// ```
    #[must_use]
    pub fn count(&self) -> usize {
        self.statements.len()
    }

    /// The statements themselves, in order.
    ///
    /// ```
    /// # use moso_test::db::{QueryLog, QuerySource};
    /// assert!(QueryLog::new().begin_queries().finish().statements().is_empty());
    /// ```
    #[must_use]
    pub fn statements(&self) -> &[RecordedStatement] {
        &self.statements
    }

    /// The statement that ran most often, and how often, when it ran more than
    /// once.
    ///
    /// A block whose count is wrong is nearly always an N+1, and the repeated
    /// statement is the whole diagnosis.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, QuerySource};
    ///
    /// let log = QueryLog::new();
    /// let scope = log.begin_queries();
    /// log.record_sql("select * from users where id = $1");
    /// log.record_sql("select * from users where id = $1");
    /// log.record_sql("select * from posts");
    ///
    /// let (sql, times) = scope.finish().most_repeated().unwrap();
    /// assert_eq!(times, 2);
    /// assert!(sql.contains("users"));
    /// ```
    #[must_use]
    pub fn most_repeated(&self) -> Option<(String, usize)> {
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for statement in &self.statements {
            *counts.entry(statement.sql_text()).or_default() += 1;
        }
        counts
            .into_iter()
            .filter(|(_, times)| *times > 1)
            .max_by_key(|(sql, times)| (*times, sql.len()))
            .map(|(sql, times)| (sql.to_owned(), times))
    }

    /// Whether the statements could be seen at all.
    ///
    /// `false` means the source was a [`TestApp`](crate::TestApp) whose log
    /// capture lost the race for the process's `tracing` subscriber, so the
    /// count is zero for a reason that has nothing to do with the code under
    /// test. The rendered report says so rather than failing quietly.
    ///
    /// ```
    /// # use moso_test::db::{QueryLog, QuerySource};
    /// assert!(QueryLog::new().begin_queries().finish().capture_available());
    /// ```
    #[must_use]
    pub const fn capture_available(&self) -> bool {
        self.capture_available
    }

    /// The report [`assert_queries!`](crate::assert_queries) prints.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, QuerySource};
    ///
    /// let log = QueryLog::new();
    /// let scope = log.begin_queries();
    /// log.record_sql("select 1");
    ///
    /// let rendered = scope.finish().render(3, "tests/x.rs", 10, 5);
    /// assert!(rendered.contains("expected exactly 3 statements, 1 ran"));
    /// assert!(rendered.contains("tests/x.rs:10:5"));
    /// ```
    #[must_use]
    pub fn render(&self, expected: usize, file: &str, line: u32, column: u32) -> String {
        let mut out = String::new();
        out.push_str("\n── moso-test: assert_queries! ─────────────────────────────────────────\n");
        let _ = writeln!(
            out,
            "  expected exactly {expected} statement{}, {} ran",
            if expected == 1 { "" } else { "s" },
            self.count()
        );
        let _ = writeln!(out, "  at {file}:{line}:{column}");

        if self.statements.is_empty() {
            out.push_str("\n  no statements were recorded\n");
            if !self.capture_available {
                out.push_str(
                    "  the harness could not capture the server's log lines — another global \
                     `tracing`\n  subscriber was installed first, so this count is not \
                     meaningful\n  help: remove the `tracing_subscriber::fmt().init()` from the \
                     test binary\n",
                );
            }
        } else {
            let _ = writeln!(out, "\n  statements ({}):", self.count());
            for (index, statement) in self.statements.iter().enumerate() {
                let _ = writeln!(out, "  {:>4}  {}", index + 1, statement.summary());
            }
        }

        if let Some((sql, times)) = self.most_repeated()
            && self.count() > expected
        {
            let _ = writeln!(
                out,
                "\n  {times} of them were identical — this is an N+1:\n          {sql}\n  help: \
                 preload the relation instead of touching it in a loop:\n         \
                 `Post::query().with(Post::AUTHOR)`"
            );
        }
        out.push_str("──────────────────────────────────────────────────────────────────────\n");
        out
    }

    /// Fails the test unless exactly `expected` statements ran.
    ///
    /// # Panics
    ///
    /// With the rendered report when the count is wrong.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, QuerySource};
    ///
    /// let log = QueryLog::new();
    /// let scope = log.begin_queries();
    /// log.record_sql("select 1");
    /// scope.finish().assert_exactly(1, "tests/x.rs", 1, 1);
    /// ```
    pub fn assert_exactly(&self, expected: usize, file: &str, line: u32, column: u32) {
        assert!(
            self.count() == expected,
            "{}",
            self.render(expected, file, line, column)
        );
    }

    /// Fails the test unless at most `budget` statements ran.
    ///
    /// The form to reach for when the exact number is an implementation detail
    /// but "one per row" is a bug.
    ///
    /// # Panics
    ///
    /// With the rendered report when the count is over budget.
    ///
    /// ```
    /// use moso_test::db::{QueryLog, QuerySource};
    ///
    /// let log = QueryLog::new();
    /// let scope = log.begin_queries();
    /// log.record_sql("select 1");
    /// scope.finish().assert_at_most(4, "tests/x.rs", 1, 1);
    /// ```
    pub fn assert_at_most(&self, budget: usize, file: &str, line: u32, column: u32) {
        assert!(
            self.count() <= budget,
            "{}",
            self.render(budget, file, line, column)
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- names ------------------------------------------------------------

    #[test]
    fn a_generated_name_is_a_legal_identifier_and_round_trips() {
        for _ in 0..64 {
            let name = generate_name();
            validate_name(&name).expect("generated names must be usable in DDL");
            assert!(name.starts_with(NAME_PREFIX));
            assert!(
                created_at(&name).is_some(),
                "{name} must carry its birthday"
            );
            assert_eq!(owning_process(&name), Some(u64::from(std::process::id())));
        }
    }

    #[test]
    fn generated_names_are_unique_within_a_process() {
        let names: std::collections::HashSet<String> = (0..1000).map(|_| generate_name()).collect();
        assert_eq!(names.len(), 1000, "the ordinal makes every name distinct");
    }

    #[test]
    fn a_name_that_was_not_generated_here_never_parses() {
        for name in [
            "postgres",
            "moso_test",
            "moso_test_template",
            "moso_testing",
            "moso_test_fixtures",
            "moso_test_a_b",
            "moso_test_a_b_c_d",
            "moso_test_A_1_0",
        ] {
            assert!(created_at(name).is_none(), "{name} must not parse");
        }
    }

    #[test]
    fn validate_name_refuses_everything_that_could_be_injected() {
        for bad in [
            "",
            "has space",
            "quote\"",
            "semi;colon",
            "UPPER",
            "1leading",
            "drop--",
        ] {
            assert!(validate_name(bad).is_err(), "{bad} must be refused");
        }
        assert!(
            validate_name(&"a".repeat(64)).is_err(),
            "63 bytes is the cap"
        );
        assert!(validate_name(&"a".repeat(63)).is_ok());
    }

    #[test]
    fn base36_round_trips() {
        for value in [0_u64, 1, 35, 36, 1234, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(from_base36(&base36(value)), Some(value), "{value}");
        }
        assert_eq!(from_base36(""), None);
        assert_eq!(from_base36("Z"), None);
    }

    // -- URLs -------------------------------------------------------------

    #[test]
    fn a_url_can_be_repointed_at_another_database() {
        let parts =
            UrlParts::parse("postgres://moso:moso@localhost:55433/moso_test").expect("a valid URL");
        assert_eq!(parts.database, "moso_test");
        assert_eq!(
            parts.with_database("postgres"),
            "postgres://moso:moso@localhost:55433/postgres"
        );
    }

    #[test]
    fn a_query_string_survives_repointing() {
        let parts = UrlParts::parse("postgres://h/app?sslmode=require&x=1").expect("a valid URL");
        assert_eq!(parts.database, "app");
        assert_eq!(
            parts.with_database("moso_test_1_2_3"),
            "postgres://h/moso_test_1_2_3?sslmode=require&x=1"
        );
    }

    #[test]
    fn the_backend_comes_from_the_scheme() {
        assert_eq!(backend_of("postgres://h/db").unwrap(), Backend::Postgres);
        assert_eq!(backend_of("postgresql://h/db").unwrap(), Backend::Postgres);
        assert_eq!(backend_of("sqlite:///tmp/x.db").unwrap(), Backend::Sqlite);
        assert!(backend_of("mysql://h/db").is_err());
    }

    #[test]
    fn a_password_never_reaches_a_message() {
        assert_eq!(
            redact("postgres://moso:hunter2@localhost:55433/moso_test"),
            "postgres://moso:***@localhost:55433/moso_test"
        );
        // Nothing to hide, nothing changed — and no allocation.
        assert!(matches!(
            redact("postgres://localhost/db"),
            std::borrow::Cow::Borrowed(_)
        ));
    }

    // -- quoting ----------------------------------------------------------

    #[test]
    fn quoting_closes_the_only_hole_ddl_leaves_open() {
        assert_eq!(quote_ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_literal("it's"), "'it''s'");
    }

    // -- strategy ---------------------------------------------------------

    #[test]
    fn the_default_strategy_is_the_documented_one() {
        assert_eq!(Strategy::default(), Strategy::Template);
        assert!(Strategy::Template.creates_a_database());
        assert!(Strategy::Migrate.creates_a_database());
        assert!(!Strategy::Transaction.creates_a_database());
        assert!(!Strategy::Transaction.needs_a_migrator());
    }

    #[test]
    fn strategy_parsing_accepts_the_spellings_a_human_writes() {
        assert_eq!("TEMPLATE".parse::<Strategy>().unwrap(), Strategy::Template);
        assert_eq!(" tx ".parse::<Strategy>().unwrap(), Strategy::Transaction);
        assert_eq!("migrations".parse::<Strategy>().unwrap(), Strategy::Migrate);
        let error = "templte".parse::<Strategy>().unwrap_err();
        assert_eq!(error.given(), "templte");
        assert!(error.to_string().contains("`template`"));
    }

    // -- migrators --------------------------------------------------------

    #[test]
    fn a_fingerprint_changes_when_a_script_does() {
        let a = SqlMigrator::new(["create table t (id int)"]);
        let b = SqlMigrator::new(["create table t (id bigint)"]);
        let c = SqlMigrator::new(["create table t (id int)", "create table u (id int)"]);
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_ne!(a.fingerprint(), c.fingerprint());
        assert_eq!(
            a.fingerprint(),
            SqlMigrator::new(["create table t (id int)"]).fingerprint(),
            "the same scripts must produce the same template"
        );
    }

    #[test]
    fn an_empty_migrator_still_has_a_stable_fingerprint() {
        assert_eq!(
            SqlMigrator::default().fingerprint(),
            SqlMigrator::default().fingerprint()
        );
    }

    #[test]
    fn from_dir_reads_sql_files_in_name_order() {
        let dir = std::env::temp_dir().join(format!("moso-test-migrations-{}", generate_name()));
        std::fs::create_dir_all(&dir).expect("a temporary directory");
        std::fs::write(dir.join("0002_second.sql"), "select 2").expect("write");
        std::fs::write(dir.join("0001_first.sql"), "select 1").expect("write");
        std::fs::write(dir.join("notes.txt"), "ignored").expect("write");

        let migrator = SqlMigrator::from_dir(&dir).expect("a readable directory");
        assert_eq!(migrator.scripts().len(), 2, "only the .sql files");
        assert!(migrator.scripts()[0].contains("0001_first"));
        assert!(migrator.scripts()[1].contains("0002_second"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn from_dir_names_the_directory_when_it_is_not_there() {
        let error = SqlMigrator::from_dir("/nonexistent/moso/migrations").unwrap_err();
        assert!(error.to_string().contains("/nonexistent/moso/migrations"));
    }

    // -- errors -----------------------------------------------------------

    #[test]
    fn the_missing_url_error_names_the_variable_and_the_two_ways_out() {
        let rendered = Error::NoDatabaseUrl.to_string();
        assert!(rendered.contains("DATABASE_URL"));
        assert!(rendered.contains("skip_without_database"));
        assert!(rendered.contains("sqlite"));
        assert!(Error::NoDatabaseUrl.is_missing_database());
    }

    #[test]
    fn an_invalid_name_says_what_the_rule_is() {
        let error = validate_name("Bad Name").unwrap_err();
        let rendered = error.to_string();
        assert!(rendered.contains("Bad Name"));
        assert!(rendered.contains("lower-case"));
    }

    #[test]
    fn statement_context_shortens_a_migration_but_keeps_a_short_statement_whole() {
        assert_eq!(statement_context("select 1"), "running `select 1`");
        let long = statement_context(&format!("create table t (\n{})", "x int,\n".repeat(40)));
        assert!(long.ends_with("...`"));
        assert!(long.len() < 100);
    }

    // -- pruning ----------------------------------------------------------

    #[test]
    fn pruning_only_ever_selects_names_this_module_generated() {
        // `force` because a name generated *here* belongs to this process, and
        // the third rule protects those.
        let options = PruneOptions::default().older_than(Duration::ZERO).force();
        let now = SystemTime::now();
        let generated = generate_name();

        assert!(options.selects(&generated, now));
        for safe in [
            "postgres",
            "template1",
            "moso_test",
            "moso_test_template",
            "shop_production",
            "moso_test_fixtures",
        ] {
            assert!(!options.selects(safe, now), "{safe} must survive a prune");
        }
    }

    #[test]
    fn pruning_never_deletes_a_database_this_process_is_using() {
        // The failure this rule prevents: a suite that calls `prune-test` while
        // its own tests are running, deleting the database of the test in the
        // next thread.
        let mine = generate_name();
        let options = PruneOptions::default().older_than(Duration::ZERO);
        assert_eq!(owning_process(&mine), Some(u64::from(std::process::id())));
        assert!(
            !options.selects(&mine, SystemTime::now()),
            "a live database of this very process must survive a prune"
        );
        assert!(
            options.clone().force().selects(&mine, SystemTime::now()),
            "`--force` is how a caller says it means it"
        );

        // Another process's stale database is fair game.
        let theirs = format!(
            "{NAME_PREFIX}{}_{}_{}",
            base36(1),
            base36(u64::from(std::process::id()) + 1),
            base36(0)
        );
        assert!(options.selects(&theirs, SystemTime::now()));
    }

    #[test]
    fn pruning_leaves_a_young_database_alone() {
        let options = PruneOptions::default().older_than(Duration::from_secs(3600));
        assert!(
            !options.selects(&generate_name(), SystemTime::now()),
            "a database created a moment ago may belong to a running test"
        );
    }

    #[test]
    fn pruning_with_templates_selects_the_template() {
        let options = PruneOptions::default().with_templates();
        assert!(options.selects("moso_test_template", SystemTime::now()));
        assert!(options.selects("shop_test_template", SystemTime::now()));
        assert!(!options.selects("shop_production", SystemTime::now()));
    }

    #[test]
    fn a_prune_summary_reads_like_a_sentence() {
        let mut pruned = Pruned::default();
        pruned.dropped.push("moso_test_a_b_c".to_owned());
        pruned
            .skipped
            .push(("moso_test_d_e_f".to_owned(), "busy".to_owned()));
        assert_eq!(pruned.summary(), "dropped 1 test databases, left 1 alone");
        assert_eq!(pruned.dropped().len(), 1);
        assert_eq!(pruned.skipped()[0].1, "busy");
    }

    // -- the statement log ------------------------------------------------

    #[test]
    fn a_mark_measures_a_block_and_ignores_what_came_before() {
        let log = QueryLog::new();
        log.record_sql("before");
        let mark = log.mark();
        log.record_sql("a");
        log.record_sql("b");

        assert_eq!(log.count_since(mark), 2);
        assert_eq!(
            log.since(mark)
                .iter()
                .map(RecordedStatement::sql_text)
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(log.total(), 3);
    }

    #[test]
    fn a_mark_survives_eviction_without_lying() {
        let log = QueryLog::with_limit(2);
        let mark = log.mark();
        for sql in ["a", "b", "c", "d"] {
            log.record_sql(sql);
        }
        assert_eq!(log.count_since(mark), 4, "the count is still exact");
        assert_eq!(log.len(), 2, "but only two texts survive");
        assert_eq!(log.since(mark).len(), 2);
    }

    #[test]
    fn transaction_control_is_recognised_in_any_case() {
        for sql in [
            "BEGIN",
            "begin;",
            "commit",
            "ROLLBACK",
            "savepoint s1",
            "release savepoint s1",
            "start transaction",
            "end",
        ] {
            assert!(
                RecordedStatement::sql(sql).is_transaction_control(),
                "{sql} is transaction control"
            );
        }
        for sql in ["select 1", "insert into t values (1)", "beginning"] {
            assert!(
                !RecordedStatement::sql(sql).is_transaction_control(),
                "{sql}"
            );
        }
    }

    #[test]
    fn a_window_ignores_transaction_control_unless_asked() {
        let log = QueryLog::new();
        let scope = log.begin_queries();
        log.record_sql("begin");
        log.record_sql("select 1");
        log.record_sql("commit");
        assert_eq!(scope.finish().count(), 1, "only the real statement counts");

        let scope = log.begin_queries().including_transaction_control();
        log.record_sql("begin");
        log.record_sql("select 1");
        assert_eq!(scope.finish().count(), 2);
    }

    #[test]
    fn the_report_names_the_repeated_statement_because_that_is_the_diagnosis() {
        let log = QueryLog::new();
        let scope = log.begin_queries();
        log.record_sql("select id, title from posts");
        for _ in 0..10 {
            log.record_sql("select * from users where id = $1");
        }

        let report = scope.finish();
        assert_eq!(report.count(), 11);
        let (sql, times) = report.most_repeated().expect("a repeat");
        assert_eq!(times, 10);
        assert!(sql.contains("users"));

        let rendered = report.render(2, "tests/posts.rs", 41, 5);
        assert!(rendered.contains("expected exactly 2 statements, 11 ran"));
        assert!(rendered.contains("tests/posts.rs:41:5"));
        assert!(rendered.contains("this is an N+1"));
        assert!(rendered.contains("select * from users where id = $1"));
        assert!(rendered.contains("   1  select id, title from posts"));
    }

    #[test]
    fn the_report_is_quiet_about_n_plus_one_when_the_count_is_too_low() {
        let log = QueryLog::new();
        let scope = log.begin_queries();
        log.record_sql("select 1");
        log.record_sql("select 1");
        let rendered = scope.finish().render(5, "x.rs", 1, 1);
        assert!(
            !rendered.contains("N+1"),
            "fewer statements than expected is not an N+1"
        );
    }

    #[test]
    fn an_empty_window_says_so_rather_than_printing_an_empty_list() {
        let log = QueryLog::new();
        let rendered = log.begin_queries().finish().render(1, "x.rs", 1, 1);
        assert!(rendered.contains("no statements were recorded"));
    }

    #[test]
    #[should_panic(expected = "expected exactly 2 statements, 1 ran")]
    fn assert_exactly_fails_with_the_report() {
        let log = QueryLog::new();
        let scope = log.begin_queries();
        log.record_sql("select 1");
        scope.finish().assert_exactly(2, "x.rs", 1, 1);
    }

    #[test]
    fn assert_at_most_passes_under_budget_and_fails_over_it() {
        let log = QueryLog::new();
        let scope = log.begin_queries();
        log.record_sql("select 1");
        scope.finish().assert_at_most(3, "x.rs", 1, 1);

        let scope = log.begin_queries();
        for _ in 0..4 {
            log.record_sql("select 1");
        }
        let report = scope.finish();
        assert!(report.count() > 3);
    }

    #[test]
    fn a_summary_never_runs_off_the_side_of_a_terminal() {
        let statement = RecordedStatement::sql("select ".to_owned() + &"a, ".repeat(200));
        assert!(statement.summary().chars().count() <= 96);
        assert!(statement.summary().ends_with("..."));
    }

    #[test]
    fn absorbing_a_captured_line_recovers_the_statement_the_server_ran() {
        let record = LogRecord {
            level: crate::Level::DEBUG,
            target: SQLX_QUERY_TARGET.to_owned(),
            message: String::new(),
            fields: vec![
                (
                    "summary".to_owned(),
                    "select \"posts\" . \"id\" …".to_owned(),
                ),
                (
                    "db.statement".to_owned(),
                    "\n\nselect \"posts\".\"id\" from \"posts\"\n".to_owned(),
                ),
                ("rows_returned".to_owned(), "3".to_owned()),
            ],
            request_id: None,
            span: None,
        };
        let log = QueryLog::new();
        assert_eq!(log.absorb(std::slice::from_ref(&record)), 1);
        assert_eq!(
            log.since(QueryMark(0))[0].sql_text(),
            "select \"posts\".\"id\" from \"posts\""
        );
        assert_eq!(log.since(QueryMark(0))[0].rows_returned(), Some(3));
    }

    #[test]
    fn absorbing_falls_back_to_the_summary_for_a_short_statement() {
        // sqlx leaves `db.statement` empty when the whole statement fits in the
        // summary, which is the common case for `select 1`.
        let record = LogRecord {
            level: crate::Level::DEBUG,
            target: SQLX_QUERY_TARGET.to_owned(),
            message: String::new(),
            fields: vec![
                ("summary".to_owned(), "select 1".to_owned()),
                ("db.statement".to_owned(), String::new()),
            ],
            request_id: None,
            span: None,
        };
        let log = QueryLog::new();
        assert_eq!(log.absorb(&[record]), 1);
        assert_eq!(log.since(QueryMark(0))[0].sql_text(), "select 1");
    }

    #[test]
    fn absorbing_ignores_every_other_log_line() {
        let record = LogRecord {
            level: crate::Level::INFO,
            target: "moso::http".to_owned(),
            message: "200 GET /posts".to_owned(),
            fields: Vec::new(),
            request_id: None,
            span: None,
        };
        assert_eq!(QueryLog::new().absorb(&[record]), 0);
    }

    #[test]
    fn a_query_source_works_through_a_reference_and_an_arc() {
        let log = Arc::new(QueryLog::new());
        let scope = QuerySource::begin_queries(&log);
        log.record_sql("select 1");
        assert_eq!(scope.finish().count(), 1);

        let borrowed: &QueryLog = &log;
        let scope = QuerySource::begin_queries(&borrowed);
        log.record_sql("select 2");
        assert_eq!(scope.finish().count(), 1);
    }

    // -- cleanup ----------------------------------------------------------

    #[test]
    fn draining_an_empty_cleanup_queue_returns_immediately() {
        assert!(drain_cleanup(Duration::from_secs(5)));
    }

    #[test]
    fn a_template_filename_can_never_escape_the_temporary_directory() {
        assert_eq!(sanitise_for_a_filename("shop_test"), "shop_test");
        let escaped = sanitise_for_a_filename("../../etc/passwd");
        assert!(!escaped.contains('/'), "{escaped}");
        assert!(!escaped.contains('.'), "{escaped}");
        assert!(escaped.ends_with("etc_passwd"), "{escaped}");
        assert_eq!(sanitise_for_a_filename(""), "template");
        assert!(!sanitise_for_a_filename(&"x".repeat(200)).contains('/'));
        assert_eq!(sanitise_for_a_filename(&"x".repeat(200)).len(), 48);
    }

    #[test]
    fn a_sqlite_template_file_is_never_mistaken_for_a_test_database() {
        // `prune_test_files` deletes generated *databases*; deleting the
        // template every test copies would turn one stale run into a suite-wide
        // failure.
        let stem = format!(
            "moso-test-template-{}-{:016x}",
            sanitise_for_a_filename("shop_test_template"),
            0x1234_u64
        );
        assert!(created_at(&stem).is_none());
        assert!(
            !PruneOptions::default()
                .older_than(Duration::ZERO)
                .selects(&stem, SystemTime::now())
        );
    }

    #[test]
    fn truthy_accepts_what_a_shell_writes() {
        for yes in ["1", "true", "TRUE", " yes ", "on"] {
            assert!(truthy(yes), "{yes}");
        }
        for no in ["0", "false", "", "no", "off"] {
            assert!(!truthy(no), "{no}");
        }
    }
}

/// Tests that need a real server.
///
/// Every one of these gates on `DATABASE_URL` and skips with a message rather
/// than failing, so the suite still passes on a machine without Docker. A
/// mocked data-layer test proves nothing: the whole point of the template
/// strategy is what PostgreSQL does with `CREATE DATABASE … TEMPLATE`.
#[cfg(test)]
mod postgres_tests {
    use super::*;

    /// The schema the template strategy's tests are built on.
    fn widgets() -> SqlMigrator {
        SqlMigrator::new([
            "create table widget (id bigserial primary key, name text not null)",
            "create table gadget (id bigserial primary key)",
        ])
    }

    macro_rules! require_postgres {
        () => {
            if !database_is_available() {
                eprintln!("moso-test: skipping — {}", skip_reason());
                return;
            }
        };
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_template_database_gives_every_test_the_whole_schema() {
        require_postgres!();
        let db = TestDb::builder()
            .strategy(Strategy::Template)
            .template("moso_test_widgets_template")
            .migrator(widgets())
            .acquire()
            .await
            .expect("a template database");

        assert_eq!(db.strategy(), Strategy::Template);
        assert_eq!(db.backend(), Backend::Postgres);
        assert!(db.name().starts_with(NAME_PREFIX));
        assert_eq!(db.count("widget").await.expect("the table exists"), 0);
        assert_eq!(db.count("gadget").await.expect("the table exists"), 0);
        db.close().await;
    }

    /// The acceptance criterion of `43-testing.md`: **a hundred tests in
    /// parallel, fully isolated.**
    ///
    /// It is written so that sharing a database could not possibly pass. Each
    /// task creates a table of its own name — a second task in the same database
    /// would fail with `already exists` — writes exactly one row, and then
    /// asserts that the table it can see contains exactly its own row and
    /// nothing else. If the hundred tasks shared one database the count would be
    /// a hundred.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn one_hundred_tests_in_parallel_are_fully_isolated() {
        require_postgres!();

        let tasks: Vec<_> = (0..100_i64)
            .map(|index| {
                tokio::spawn(async move {
                    let db = TestDb::builder()
                        .strategy(Strategy::Template)
                        .template("moso_test_isolation_template")
                        .migrator(widgets())
                        .acquire()
                        .await
                        .unwrap_or_else(|error| panic!("task {index}: {error}"));

                    // A table named after this task. In a shared database the
                    // second task to reach this line fails outright.
                    db.execute(&format!("create table task_{index} (n bigint not null)"))
                        .await
                        .unwrap_or_else(|error| panic!("task {index} create: {error}"));

                    db.execute(&format!(
                        "insert into widget (name) values ('task {index}')"
                    ))
                    .await
                    .unwrap_or_else(|error| panic!("task {index} insert: {error}"));

                    let rows = db
                        .fetch_i64("select count(*) from widget")
                        .await
                        .unwrap_or_else(|error| panic!("task {index} count: {error}"));
                    assert_eq!(
                        rows, 1,
                        "task {index} saw {rows} rows in `widget`; with shared state it would \
                         see up to 100"
                    );

                    let names = db
                        .fetch_text_column("select name from widget")
                        .await
                        .unwrap_or_else(|error| panic!("task {index} select: {error}"));
                    assert_eq!(names, [format!("task {index}")]);

                    // And nobody else's table is visible either.
                    let tables = db
                        .fetch_i64(
                            "select count(*) from information_schema.tables where table_schema \
                             = 'public' and table_name like 'task\\_%'",
                        )
                        .await
                        .unwrap_or_else(|error| panic!("task {index} tables: {error}"));
                    assert_eq!(tables, 1, "task {index} can see another task's table");

                    let name = db.name().to_owned();
                    db.close().await;
                    name
                })
            })
            .collect();

        let mut names = std::collections::HashSet::new();
        for task in tasks {
            names.insert(task.await.expect("no task may panic"));
        }
        assert_eq!(names.len(), 100, "every test had a database of its own");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropping_the_handle_drops_the_database() {
        require_postgres!();
        let url = configured_url().expect("checked above");
        let parts = UrlParts::parse(&url).expect("a valid URL");

        let name = {
            let db = TestDb::builder()
                .template("moso_test_widgets_template")
                .migrator(widgets())
                .acquire()
                .await
                .expect("a database");
            db.name().to_owned()
        };

        assert!(
            drain_cleanup(Duration::from_secs(30)),
            "the cleaner must finish"
        );

        let admin = admin_pool(&parts).await.expect("a maintenance connection");
        let remaining = admin
            .fetch_i64(&format!(
                "select count(*) from pg_database where datname = {}",
                quote_literal(&name)
            ))
            .await
            .expect("a readable catalogue");
        admin.close().await;
        assert_eq!(remaining, 0, "`{name}` outlived its handle");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn keeping_a_database_really_keeps_it() {
        require_postgres!();
        let url = configured_url().expect("checked above");
        let parts = UrlParts::parse(&url).expect("a valid URL");

        let name = {
            let db = TestDb::builder()
                .template("moso_test_widgets_template")
                .migrator(widgets())
                .keep()
                .acquire()
                .await
                .expect("a database");
            assert!(db.is_kept());
            db.name().to_owned()
        };
        assert!(drain_cleanup(Duration::from_secs(30)));

        let admin = admin_pool(&parts).await.expect("a maintenance connection");
        let remaining = admin
            .fetch_i64(&format!(
                "select count(*) from pg_database where datname = {}",
                quote_literal(&name)
            ))
            .await
            .expect("a readable catalogue");
        // Clean up after ourselves, since the whole point was not to.
        let _ = drop_database(&admin, &name, true).await;
        admin.close().await;
        assert_eq!(remaining, 1, "`--keep-db` must keep the database");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn editing_a_migration_rebuilds_the_template() {
        require_postgres!();
        const TEMPLATE: &str = "moso_test_fingerprint_template";

        let first = TestDb::builder()
            .template(TEMPLATE)
            .migrator(SqlMigrator::new(["create table v1 (id int primary key)"]))
            .acquire()
            .await
            .expect("a database");
        assert_eq!(first.count("v1").await.expect("v1 exists"), 0);
        first.close().await;

        // A different migrator, so a different fingerprint: the cached template
        // must not be reused.
        let second = TestDb::builder()
            .template(TEMPLATE)
            .migrator(SqlMigrator::new(["create table v2 (id int primary key)"]))
            .acquire()
            .await
            .expect("a rebuilt database");
        assert_eq!(second.count("v2").await.expect("v2 exists"), 0);
        assert!(
            second.count("v1").await.is_err(),
            "the stale table must be gone, or yesterday's schema is still being tested"
        );
        second.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_migrate_strategy_replays_the_whole_chain_into_an_empty_database() {
        require_postgres!();
        let db = TestDb::builder()
            .strategy(Strategy::Migrate)
            .migrator(widgets())
            .acquire()
            .await
            .expect("a migrated database");
        assert_eq!(db.strategy(), Strategy::Migrate);
        assert_eq!(db.count("widget").await.expect("widget exists"), 0);
        assert!(
            db.fetch_i64(&format!("select count(*) from {FINGERPRINT_TABLE}"))
                .await
                .is_err(),
            "`migrate` starts from empty and stamps nothing"
        );
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn the_transaction_strategy_isolates_and_then_rolls_back() {
        require_postgres!();
        // A shared table for the transaction strategy to write into and undo.
        let setup = TestDb::builder()
            .url(configured_url().expect("checked above"))
            .strategy(Strategy::Transaction)
            .acquire()
            .await
            .expect("a pinned connection");
        setup
            .execute("create table if not exists tx_probe (n bigint not null)")
            .await
            .expect("ddl");
        setup.execute("commit").await.expect("publish the table");
        setup.execute("begin").await.expect("reopen");
        setup.execute("delete from tx_probe").await.expect("clean");
        setup.execute("commit").await.expect("publish");
        setup.close().await;

        let a = TestDb::builder()
            .strategy(Strategy::Transaction)
            .acquire()
            .await
            .expect("a pinned connection");
        let b = TestDb::builder()
            .strategy(Strategy::Transaction)
            .acquire()
            .await
            .expect("a second pinned connection");

        a.execute("insert into tx_probe (n) values (1)")
            .await
            .expect("insert");
        b.execute("insert into tx_probe (n) values (2)")
            .await
            .expect("insert");

        assert_eq!(
            a.fetch_i64("select count(*) from tx_probe")
                .await
                .expect("count"),
            1,
            "an uncommitted row in another transaction must be invisible"
        );
        assert_eq!(
            b.fetch_i64("select count(*) from tx_probe")
                .await
                .expect("count"),
            1
        );

        a.close().await;
        b.close().await;

        let after = TestDb::builder()
            .strategy(Strategy::Transaction)
            .acquire()
            .await
            .expect("a pinned connection");
        assert_eq!(
            after
                .fetch_i64("select count(*) from tx_probe")
                .await
                .expect("count"),
            0,
            "both transactions must have been rolled back"
        );
        after.execute("commit").await.expect("end");
        after.execute("begin").await.expect("reopen");
        after.execute("drop table tx_probe").await.expect("cleanup");
        after.execute("commit").await.expect("publish");
        after.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pruning_removes_stale_databases_and_nothing_else() {
        require_postgres!();
        let url = configured_url().expect("checked above");
        let parts = UrlParts::parse(&url).expect("a valid URL");
        let admin = admin_pool(&parts).await.expect("a maintenance connection");

        // A database whose name says it was created a week ago by a *different*
        // process — which is what a crashed run leaves behind, and the only
        // thing a prune is allowed to remove.
        let week_ago = SystemTime::now() - Duration::from_secs(7 * 24 * 3600);
        let millis = week_ago
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_millis();
        let stale = format!(
            "{NAME_PREFIX}{}_{}_{}",
            base36(u64::try_from(millis).unwrap_or(0)),
            base36(u64::from(std::process::id()) + 1),
            base36(999_999)
        );
        // And one that only looks like one.
        let decoy = "moso_test_fixtures";

        admin
            .execute(&format!("create database {}", quote_ident(&stale)))
            .await
            .expect("create the stale database");
        let _ = admin
            .execute(&format!("create database {}", quote_ident(decoy)))
            .await;

        let pruned = prune_test_databases(&url, &PruneOptions::default())
            .await
            .expect("a prune");

        assert!(
            pruned.dropped().contains(&stale),
            "the week-old database should have gone: {:?}",
            pruned.dropped()
        );
        assert!(
            !pruned.dropped().iter().any(|name| name == decoy),
            "`{decoy}` was not generated by moso-test and must never be dropped"
        );
        assert!(
            !pruned.dropped().iter().any(|name| name == &parts.database),
            "the configured database must never be dropped"
        );

        let _ = drop_database(&admin, decoy, true).await;
        admin.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dry_run_prune_deletes_nothing() {
        require_postgres!();
        let url = configured_url().expect("checked above");
        let pruned = prune_test_databases(&url, &PruneOptions::default().dry_run())
            .await
            .expect("a prune");
        // Whatever it selected is still there, because nothing ran.
        let parts = UrlParts::parse(&url).expect("a valid URL");
        let admin = admin_pool(&parts).await.expect("a maintenance connection");
        for name in pruned.dropped() {
            let present = admin
                .fetch_i64(&format!(
                    "select count(*) from pg_database where datname = {}",
                    quote_literal(name)
                ))
                .await
                .expect("a readable catalogue");
            assert_eq!(present, 1, "a dry run dropped `{name}`");
        }
        admin.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn assert_queries_counts_statements_that_really_ran() {
        require_postgres!();
        let db = TestDb::builder()
            .template("moso_test_widgets_template")
            .migrator(widgets())
            .acquire()
            .await
            .expect("a database");

        let scope = db.begin_queries();
        db.execute("insert into widget (name) values ('a')")
            .await
            .expect("insert");
        db.execute("insert into widget (name) values ('b')")
            .await
            .expect("insert");
        let report = scope.finish();

        assert_eq!(report.count(), 2);
        assert!(report.statements()[0].sql_text().contains("insert"));
        assert_eq!(report.statements()[0].rows_affected(), Some(1));
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_missing_migrator_is_an_error_that_says_what_to_do() {
        require_postgres!();
        let error = TestDb::builder()
            .strategy(Strategy::Migrate)
            .acquire()
            .await
            .expect_err("a migrate strategy without a migrator cannot work");
        let rendered = error.to_string();
        assert!(rendered.contains("migrator"), "{rendered}");
        assert!(rendered.contains("SqlMigrator"), "{rendered}");
    }
}

/// SQLite, which needs no server and therefore always runs.
#[cfg(test)]
mod sqlite_tests {
    use super::*;

    fn widgets() -> SqlMigrator {
        SqlMigrator::new(["create table widget (id integer primary key, name text not null)"])
    }

    #[tokio::test]
    async fn a_sqlite_test_database_needs_no_environment_at_all() {
        let db = TestDb::builder()
            .sqlite()
            .migrator(widgets())
            .acquire()
            .await
            .expect("a SQLite database");

        assert_eq!(db.backend(), Backend::Sqlite);
        assert!(db.url().starts_with("sqlite://"));
        db.execute("insert into widget (name) values ('a')")
            .await
            .expect("insert");
        assert_eq!(db.count("widget").await.expect("count"), 1);
        db.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn twenty_parallel_sqlite_databases_are_isolated() {
        let tasks: Vec<_> = (0..20_i64)
            .map(|index| {
                tokio::spawn(async move {
                    let db = TestDb::builder()
                        .sqlite()
                        .template("sqlite_isolation")
                        .migrator(widgets())
                        .acquire()
                        .await
                        .unwrap_or_else(|error| panic!("task {index}: {error}"));
                    db.execute(&format!("insert into widget (name) values ('t{index}')"))
                        .await
                        .unwrap_or_else(|error| panic!("task {index} insert: {error}"));
                    let names = db
                        .fetch_text_column("select name from widget")
                        .await
                        .unwrap_or_else(|error| panic!("task {index} select: {error}"));
                    assert_eq!(names, [format!("t{index}")]);
                    let path = db.file.clone();
                    db.close().await;
                    path
                })
            })
            .collect();

        for task in tasks {
            let path = task.await.expect("no task may panic");
            if let Some(path) = path {
                assert!(!path.exists(), "`{}` outlived its handle", path.display());
            }
        }
    }

    #[tokio::test]
    async fn the_sqlite_migrate_strategy_starts_from_nothing() {
        let db = TestDb::builder()
            .sqlite()
            .strategy(Strategy::Migrate)
            .migrator(widgets())
            .acquire()
            .await
            .expect("a migrated SQLite database");
        assert_eq!(db.count("widget").await.expect("count"), 0);
        db.close().await;
    }

    #[tokio::test]
    async fn a_sqlite_template_is_copied_rather_than_re_migrated() {
        // Two databases from one template: the second must have the schema
        // without the migrator having run again, which is what the file copy
        // buys.
        let migrator = Arc::new(widgets());
        let first = TestDb::builder()
            .sqlite()
            .template("sqlite_copy")
            .shared_migrator(Arc::clone(&migrator) as Arc<dyn Migrator>)
            .acquire()
            .await
            .expect("the first");
        let second = TestDb::builder()
            .sqlite()
            .template("sqlite_copy")
            .shared_migrator(Arc::clone(&migrator) as Arc<dyn Migrator>)
            .acquire()
            .await
            .expect("the second");

        assert_ne!(first.name(), second.name());
        first
            .execute("insert into widget (name) values ('a')")
            .await
            .expect("insert");
        assert_eq!(first.count("widget").await.expect("count"), 1);
        assert_eq!(
            second.count("widget").await.expect("count"),
            0,
            "the copies must not share rows"
        );
        first.close().await;
        second.close().await;
    }

    #[tokio::test]
    async fn pruning_files_leaves_everything_that_is_not_ours_alone() {
        let directory = std::env::temp_dir();
        let decoy = directory.join("moso_test_fixtures.sqlite");
        std::fs::write(&decoy, b"not ours").expect("write the decoy");

        // `older_than(ZERO)` plus a real delete would sweep the *shared*
        // temporary directory, and the harness runs one process per test: the
        // `.building` file `ensure_sqlite_template` is mid-way through
        // migrating in a sibling process carries this module's prefix and is
        // zero seconds old, so it is selected and removed, and that process
        // fails its `rename` with `ENOENT`. `dry_run` exercises the identical
        // selection rule — `Pruned::dropped` still lists every entry that would
        // go — and is the only thing this test asserts about, so nothing is
        // lost by not deleting.
        let pruned =
            prune_test_files(&PruneOptions::default().older_than(Duration::ZERO).dry_run())
                .expect("a prune");
        assert!(
            !pruned
                .dropped()
                .iter()
                .any(|name| name == "moso_test_fixtures"),
            "a name that was not generated must survive"
        );
        assert!(decoy.exists());
        std::fs::remove_file(&decoy).ok();
    }
}
