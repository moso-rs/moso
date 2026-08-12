//! The single connection a migration run holds.
//!
//! # Why not a pool
//!
//! Three reasons, all of them the difference between working and appearing to.
//!
//! 1. **`pg_advisory_lock` is per session.** Take it on one pooled connection
//!    and run the next statement on another, and the lock is protecting
//!    nothing.
//! 2. **`SET lock_timeout` is session state.** Leave it on a pooled connection
//!    and the next borrower — a request handler — inherits it.
//! 3. **Some statements cannot be in a transaction.** `CREATE INDEX
//!    CONCURRENTLY` needs a connection the runner controls entirely.
//!
//! So `moso-migrate` opens its own connection, uses it, and closes it.

use std::time::Duration;

use moso_orm::Backend;
use sqlx::{AssertSqlSafe, Connection as _, Executor as _, Row as _};

use crate::error::{Error, Result};

/// One database connection, on either backend.
///
/// ```no_run
/// use moso_migrate::conn::Connection;
///
/// # async fn example() -> moso_migrate::Result<()> {
/// let mut connection = Connection::open("postgres://moso:moso@localhost/moso_test").await?;
/// assert_eq!(connection.backend(), moso_orm::Backend::Postgres);
/// connection.close().await;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Connection {
    backend: Backend,
    url: String,
    inner: Inner,
}

#[derive(Debug)]
enum Inner {
    Postgres(Box<sqlx::PgConnection>),
    Sqlite(Box<sqlx::SqliteConnection>),
}

impl Connection {
    /// Opens a connection from a `DATABASE_URL`.
    ///
    /// SQLite connections are opened with foreign keys **off**, because the
    /// 12-step table rebuild requires it and `PRAGMA foreign_keys` is silently
    /// ignored inside a transaction. Every rebuild ends with
    /// `PRAGMA foreign_key_check`, which the runner treats as a failure when it
    /// returns rows — so the check is not lost, it is moved to where it works.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] with the URL's host in the context, so that "cannot
    /// connect" says which database it could not connect to.
    ///
    /// ```no_run
    /// # async fn example() -> moso_migrate::Result<()> {
    /// let connection = moso_migrate::conn::Connection::open("sqlite://app.db").await?;
    /// # let _ = connection;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn open(url: &str) -> Result<Self> {
        let backend = backend_of(url)?;
        let inner = match backend {
            Backend::Sqlite => {
                use std::str::FromStr as _;
                let options = sqlx::sqlite::SqliteConnectOptions::from_str(url)
                    .map_err(|source| {
                        Error::database(
                            format!("parsing `{}`", redact(url)),
                            "a SQLite URL looks like `sqlite://app.db` or `sqlite::memory:`",
                            source,
                        )
                    })?
                    .create_if_missing(true)
                    .foreign_keys(false)
                    .busy_timeout(Duration::from_secs(30));
                Inner::Sqlite(Box::new(
                    sqlx::SqliteConnection::connect_with(&options)
                        .await
                        .map_err(|source| connect_error(url, source))?,
                ))
            }
            _ => Inner::Postgres(Box::new(
                sqlx::PgConnection::connect(url)
                    .await
                    .map_err(|source| connect_error(url, source))?,
            )),
        };
        Ok(Self {
            backend,
            url: url.to_owned(),
            inner,
        })
    }

    /// Which backend this is.
    ///
    /// ```no_run
    /// # async fn example() -> moso_migrate::Result<()> {
    /// let connection = moso_migrate::conn::Connection::open("sqlite://app.db").await?;
    /// assert_eq!(connection.backend(), moso_orm::Backend::Sqlite);
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }

    /// The URL it was opened with, with any password removed.
    ///
    /// ```no_run
    /// # async fn example() -> moso_migrate::Result<()> {
    /// let connection = moso_migrate::conn::Connection::open("sqlite://app.db").await?;
    /// assert_eq!(connection.url(), "sqlite://app.db");
    /// # Ok(())
    /// # }
    /// ```
    #[must_use]
    pub fn url(&self) -> String {
        redact(&self.url)
    }

    /// Runs one statement and returns the number of rows it affected.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] carrying the statement, because a migration failure
    /// whose message does not say which statement failed is a very long
    /// evening.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// connection.execute("CREATE TABLE t (id integer)").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(&mut self, sql: &str) -> Result<u64> {
        let owned = sql.to_owned();
        match &mut self.inner {
            Inner::Postgres(connection) => connection
                .execute(AssertSqlSafe(owned))
                .await
                .map(|done| done.rows_affected()),
            Inner::Sqlite(connection) => connection
                .execute(AssertSqlSafe(owned))
                .await
                .map(|done| done.rows_affected()),
        }
        .map_err(|source| statement_error(sql, source))
    }

    /// Runs a statement and returns how many rows came back.
    ///
    /// Used for `PRAGMA foreign_key_check`, which reports violations as rows
    /// rather than as an error, and for the advisory-lock probes.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] carrying the statement.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let violations = connection.count_rows("PRAGMA foreign_key_check").await?;
    /// assert_eq!(violations, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn count_rows(&mut self, sql: &str) -> Result<usize> {
        let owned = sql.to_owned();
        match &mut self.inner {
            Inner::Postgres(connection) => sqlx::query(AssertSqlSafe(owned))
                .fetch_all(&mut **connection)
                .await
                .map(|rows| rows.len()),
            Inner::Sqlite(connection) => sqlx::query(AssertSqlSafe(owned))
                .fetch_all(&mut **connection)
                .await
                .map(|rows| rows.len()),
        }
        .map_err(|source| statement_error(sql, source))
    }

    /// Runs a statement and returns the first column of the first row as a
    /// boolean, for `pg_try_advisory_lock`.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] carrying the statement.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let got_it = connection.fetch_bool("SELECT pg_try_advisory_lock(1)").await?;
    /// # let _ = got_it;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_bool(&mut self, sql: &str) -> Result<bool> {
        let owned = sql.to_owned();
        match &mut self.inner {
            Inner::Postgres(connection) => sqlx::query(AssertSqlSafe(owned))
                .fetch_one(&mut **connection)
                .await
                .and_then(|row| row.try_get::<bool, _>(0)),
            Inner::Sqlite(connection) => sqlx::query(AssertSqlSafe(owned))
                .fetch_one(&mut **connection)
                .await
                .and_then(|row| row.try_get::<bool, _>(0)),
        }
        .map_err(|source| statement_error(sql, source))
    }

    /// Runs a statement and returns every row's columns as strings.
    ///
    /// This is how introspection reads a catalogue: every value it wants is
    /// either text already or has a text representation the database will
    /// produce with a cast, and going through one shape keeps the two
    /// backends' introspectors readable.
    ///
    /// # Errors
    ///
    /// [`Error::Database`] carrying the statement.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) -> moso_migrate::Result<()> {
    /// let rows = connection.fetch_text("SELECT 'a', 'b'").await?;
    /// assert_eq!(rows[0][0].as_deref(), Some("a"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch_text(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let owned = sql.to_owned();
        match &mut self.inner {
            Inner::Postgres(connection) => {
                let rows = sqlx::query(AssertSqlSafe(owned))
                    .fetch_all(&mut **connection)
                    .await
                    .map_err(|source| statement_error(sql, source))?;
                Ok(rows
                    .iter()
                    .map(|row| {
                        (0..row.len())
                            .map(|index| row.try_get::<Option<String>, _>(index).unwrap_or(None))
                            .collect()
                    })
                    .collect())
            }
            Inner::Sqlite(connection) => {
                let rows = sqlx::query(AssertSqlSafe(owned))
                    .fetch_all(&mut **connection)
                    .await
                    .map_err(|source| statement_error(sql, source))?;
                Ok(rows
                    .iter()
                    .map(|row| {
                        (0..row.len())
                            .map(|index| {
                                row.try_get::<Option<String>, _>(index).unwrap_or_else(|_| {
                                    // SQLite columns are dynamically typed:
                                    // `pragma table_info` returns integers for
                                    // `notnull` and `pk`. Reading them as i64
                                    // and formatting keeps one code path.
                                    row.try_get::<Option<i64>, _>(index)
                                        .ok()
                                        .flatten()
                                        .map(|value| value.to_string())
                                })
                            })
                            .collect()
                    })
                    .collect())
            }
        }
    }

    /// Closes the connection, ignoring a failure to say goodbye politely.
    ///
    /// ```no_run
    /// # async fn example(connection: moso_migrate::conn::Connection) {
    /// connection.close().await;
    /// # }
    /// ```
    pub async fn close(self) {
        let _ = match self.inner {
            Inner::Postgres(connection) => connection.close().await,
            Inner::Sqlite(connection) => connection.close().await,
        };
    }
}

/// Which backend a URL names.
///
/// [`Backend::from_url`] splits on `://` and therefore does not recognise
/// SQLite's two-colon spellings — `sqlite::memory:` and `sqlite:app.db` — which
/// are exactly the ones a test and a quick script use. This is the same
/// classification with those included, and it defers to `moso-orm` for the
/// error message so that the two crates say the same thing about a URL neither
/// can open.
///
/// ```
/// use moso_migrate::conn::backend_of;
/// use moso_orm::Backend;
///
/// assert_eq!(backend_of("sqlite::memory:")?, Backend::Sqlite);
/// assert_eq!(backend_of("postgres://h/db")?, Backend::Postgres);
/// assert!(backend_of("mysql://h/db").is_err());
/// # Ok::<(), moso_migrate::Error>(())
/// ```
///
/// # Errors
///
/// [`Error::Unsupported`] naming the schemes that are supported.
pub fn backend_of(url: &str) -> Result<Backend> {
    if url.starts_with("sqlite:") {
        return Ok(Backend::Sqlite);
    }
    if url.starts_with("postgres:") || url.starts_with("postgresql:") {
        return Ok(Backend::Postgres);
    }
    Backend::from_url(url).map_err(|error| Error::Unsupported {
        backend: "moso",
        operation: format!("open `{}`", redact(url)),
        help: error.to_string(),
    })
}

fn connect_error(url: &str, source: sqlx::Error) -> Error {
    Error::database(
        format!("connecting to `{}`", redact(url)),
        "check DATABASE_URL, and that the server is up and accepting connections",
        source,
    )
}

fn statement_error(sql: &str, source: sqlx::Error) -> Error {
    Error::database(
        format!("running `{}`", first_line(sql)),
        "the statement above is the one the database refused; the rest of the migration did not \
         run",
        source,
    )
}

fn first_line(sql: &str) -> String {
    let compact = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= 120 {
        return compact;
    }
    let truncated: String = compact.chars().take(117).collect();
    format!("{truncated}...")
}

/// Removes the password from a connection URL, so it can appear in a log line.
///
/// ```
/// assert_eq!(
///     moso_migrate::conn::redact("postgres://moso:secret@localhost/db"),
///     "postgres://moso:***@localhost/db",
/// );
/// ```
#[must_use]
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let Some((authority, path)) = rest.split_once('/') else {
        return redact_authority(scheme, rest, None);
    };
    redact_authority(scheme, authority, Some(path))
}

fn redact_authority(scheme: &str, authority: &str, path: Option<&str>) -> String {
    let authority = match authority.split_once('@') {
        Some((credentials, host)) => match credentials.split_once(':') {
            Some((user, _)) => format!("{user}:***@{host}"),
            None => format!("{credentials}@{host}"),
        },
        None => authority.to_owned(),
    };
    match path {
        Some(path) => format!("{scheme}://{authority}/{path}"),
        None => format!("{scheme}://{authority}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passwords_never_reach_a_log_line() {
        assert_eq!(
            redact("postgres://moso:hunter2@localhost:5432/app"),
            "postgres://moso:***@localhost:5432/app"
        );
        assert_eq!(redact("sqlite://app.db"), "sqlite://app.db");
        assert_eq!(
            redact("postgres://localhost/app"),
            "postgres://localhost/app"
        );
        assert_eq!(redact("not a url"), "not a url");
        assert_eq!(
            redact("postgres://moso:p@localhost"),
            "postgres://moso:***@localhost"
        );
    }

    #[test]
    fn statement_errors_name_the_statement_and_stay_short() {
        let long = format!("SELECT {}", "x".repeat(500));
        assert!(first_line(&long).ends_with("..."));
        assert_eq!(first_line("SELECT\n  1"), "SELECT 1");
    }

    #[tokio::test]
    async fn an_unsupported_url_names_the_schemes() {
        let error = Connection::open("mysql://localhost/app")
            .await
            .expect_err("mysql is not in this build");
        assert!(error.to_string().contains("postgres://"), "{error}");
    }

    #[tokio::test]
    async fn sqlite_in_memory_round_trips() {
        let mut connection = Connection::open("sqlite::memory:")
            .await
            .expect("sqlite is bundled");
        assert_eq!(connection.backend(), Backend::Sqlite);
        connection
            .execute("CREATE TABLE t (id integer primary key)")
            .await
            .expect("creates");
        connection
            .execute("INSERT INTO t (id) VALUES (1)")
            .await
            .expect("inserts");
        assert_eq!(
            connection
                .count_rows("SELECT * FROM t")
                .await
                .expect("counts"),
            1
        );
        let rows = connection
            .fetch_text("SELECT id FROM t")
            .await
            .expect("fetches");
        assert_eq!(rows[0][0].as_deref(), Some("1"));
        connection.close().await;
    }

    #[tokio::test]
    async fn a_failing_statement_says_which_one() {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        let error = connection
            .execute("SELECT * FROM nope")
            .await
            .expect_err("no such table");
        assert!(error.to_string().contains("SELECT * FROM nope"), "{error}");
        connection.close().await;
    }

    #[tokio::test]
    async fn sqlite_opens_with_foreign_keys_off() {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        let rows = connection
            .fetch_text("PRAGMA foreign_keys")
            .await
            .expect("pragma");
        assert_eq!(
            rows[0][0].as_deref(),
            Some("0"),
            "the rebuild needs them off"
        );
        connection.close().await;
    }
}
