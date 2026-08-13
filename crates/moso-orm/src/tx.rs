//! Transactions: [`Tx`], [`TxOptions`], savepoints, and the request-scoped
//! form [`RequestTx`].
//!
//! # Why the retrying form takes a closure
//!
//! [`Db::transaction`](crate::Db::transaction) retries a serialisation failure
//! (`SQLSTATE 40001`) and a deadlock (`40P01`), and a retry has to be able to
//! **re-run the body**. A handle-based API cannot: by the time the failure is
//! observed the caller's statements have already been issued and their results
//! consumed. That is the whole reason the ergonomic-looking `let tx =
//! db.begin()` form ([`Db::begin`](crate::Db::begin)) does not retry and says
//! so.
//!
//! # What rolls a transaction back
//!
//! Returning `Err`, calling [`Tx::rollback`], **panicking**, and dropping the
//! handle. The last two are the same mechanism: the driver's transaction is
//! owned by the `Tx`, so unwinding past it issues the rollback and returns the
//! connection to the pool unpoisoned.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use core::time::Duration;
use std::sync::Arc;

use crate::db::{AdvisoryKey, Backend, Db};
use crate::error::{Error, Result};

// `RequestTx`, the middleware that commits it, and the `moso_orm::Error` →
// `moso_core::Error` mapping live in their own file and are re-exported here,
// because `moso_orm::tx::RequestTx` is the path the crate root re-exports.
#[path = "request_tx.rs"]
mod request_tx;

pub use crate::tx::request_tx::{RequestTx, RequestTxLayer};

/// An open transaction.
///
/// Statements run on it through [`Executor`](crate::Executor), which is
/// implemented for `&Tx` and `&mut Tx` — so a service function written against
/// `impl Executor<'_>` takes either, and the same function works outside a
/// transaction with `&db`.
///
/// Dropping an uncommitted `Tx` rolls it back.
///
/// ```no_run
/// # use moso_orm::{Db, Result};
/// # async fn example(db: &Db) -> Result<()> {
/// let tx = db.begin().await?;
/// // … statements on `&tx` …
/// tx.commit().await
/// # }
/// ```
pub struct Tx {
    db: Db,
    options: TxOptions,
    depth: u32,
    state: Arc<TxState>,
}

/// What a transaction and its savepoints share.
///
/// The driver transaction is behind an async mutex because `&Tx` is an
/// [`Executor`](crate::Executor): two statements on the same transaction are
/// serialised rather than racing for the connection, which is the only correct
/// answer — a connection can run one statement at a time.
struct TxState {
    handle: tokio::sync::Mutex<Option<TxHandle>>,
    closed: AtomicBool,
    savepoints: AtomicU32,
}

/// The driver transaction behind a [`Tx`].
pub(crate) enum TxHandle {
    /// A PostgreSQL transaction.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::Transaction<'static, sqlx::Postgres>),
    /// A SQLite transaction.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::Transaction<'static, sqlx::Sqlite>),
    /// Present only when no database backend is compiled in, so the enum is
    /// never zero-variant — which would make every `match` over it
    /// non-exhaustive under `cargo hack --each-feature`. Uninhabited: it holds
    /// no value and no runtime build ever constructs it.
    #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
    Unbacked(core::convert::Infallible),
}

/// The driver transaction, locked for the duration of one statement.
pub(crate) struct TxGuard<'a> {
    inner: tokio::sync::MutexGuard<'a, Option<TxHandle>>,
}

impl TxGuard<'_> {
    /// The PostgreSQL connection this transaction runs on.
    #[cfg(feature = "postgres")]
    pub(crate) fn postgres(&mut self) -> Result<&mut sqlx::PgConnection> {
        match self.inner.as_mut() {
            Some(TxHandle::Postgres(transaction)) => Ok(transaction),
            #[cfg(feature = "sqlite")]
            Some(TxHandle::Sqlite(_)) => Err(closed_or_wrong_backend(Backend::Postgres)),
            None => Err(already_finished()),
        }
    }

    /// The SQLite connection this transaction runs on.
    #[cfg(feature = "sqlite")]
    pub(crate) fn sqlite(&mut self) -> Result<&mut sqlx::SqliteConnection> {
        match self.inner.as_mut() {
            Some(TxHandle::Sqlite(transaction)) => Ok(transaction),
            #[cfg(feature = "postgres")]
            Some(TxHandle::Postgres(_)) => Err(closed_or_wrong_backend(Backend::Sqlite)),
            None => Err(already_finished()),
        }
    }
}

/// A statement was sent to a transaction that has already ended.
fn already_finished() -> Error {
    Error::Connection {
        detail: String::from(
            "this transaction has already been committed or rolled back\n  \
             help: open a new one with `db.begin()`, or move the statement inside the \
             `db.transaction(..)` closure",
        ),
    }
}

/// A handle disagreed with its transaction about which driver it holds. Only
/// reachable by constructing a `Tx` for one backend and a `Db` for the other,
/// which the crate does not do — and only *expressible* on a build that has
/// both drivers.
#[cfg(all(feature = "postgres", feature = "sqlite"))]
fn closed_or_wrong_backend(expected: Backend) -> Error {
    Error::Connection {
        detail: format!("this transaction is not a {expected} transaction"),
    }
}

impl Tx {
    /// Opens a transaction on `db`.
    ///
    /// The `BEGIN` carries the isolation level and the access mode, rather than
    /// a `SET TRANSACTION` after the fact: one round trip instead of two, and
    /// no window in which the transaction is open at the wrong level.
    pub(crate) async fn open(db: Db, options: TxOptions) -> Result<Self> {
        let begin = options.begin_statement(db.backend());
        let handle = db.begin_driver_transaction(&begin).await?;

        let tx = Self {
            db,
            options,
            depth: 0,
            state: Arc::new(TxState {
                handle: tokio::sync::Mutex::new(Some(handle)),
                closed: AtomicBool::new(false),
                savepoints: AtomicU32::new(0),
            }),
        };
        tx.apply_local_timeouts().await?;
        Ok(tx)
    }

    /// The handle this transaction was opened on.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Tx};
    /// fn pool_of(tx: &Tx) -> &Db {
    ///     tx.db()
    /// }
    /// ```
    #[must_use]
    pub const fn db(&self) -> &Db {
        &self.db
    }

    /// The options it was opened with.
    ///
    /// ```no_run
    /// # use moso_orm::{Isolation, Tx};
    /// fn level(tx: &Tx) -> Isolation {
    ///     tx.options().isolation
    /// }
    /// ```
    #[must_use]
    pub const fn options(&self) -> &TxOptions {
        &self.options
    }

    /// How many savepoints deep this handle is. Zero is the outermost
    /// transaction.
    ///
    /// ```no_run
    /// # use moso_orm::Tx;
    /// fn nested(tx: &Tx) -> bool {
    ///     tx.depth() > 0
    /// }
    /// ```
    #[must_use]
    pub const fn depth(&self) -> u32 {
        self.depth
    }

    /// Commits.
    ///
    /// # Errors
    ///
    /// [`Error::Serialization`] or [`Error::Deadlock`] when the server refuses
    /// the commit, and any constraint violation deferred to commit time.
    ///
    /// ```no_run
    /// # use moso_orm::{Result, Tx};
    /// # async fn finish(tx: Tx) -> Result<()> {
    /// tx.commit().await
    /// # }
    /// ```
    pub async fn commit(self) -> Result<()> {
        self.finish(true).await
    }

    /// Rolls back.
    ///
    /// Dropping does the same; this exists so that the intent is written down
    /// and so that the error is visible.
    ///
    /// # Errors
    ///
    /// [`Error::Connection`] when the rollback itself cannot be sent.
    ///
    /// ```no_run
    /// # use moso_orm::{Result, Tx};
    /// # async fn abandon(tx: Tx) -> Result<()> {
    /// tx.rollback().await
    /// # }
    /// ```
    pub async fn rollback(self) -> Result<()> {
        self.finish(false).await
    }

    /// Runs `operation` inside a savepoint, committing it on `Ok` and rolling
    /// back to the savepoint on `Err`.
    ///
    /// Nesting works: a savepoint inside a savepoint is another `SAVEPOINT`.
    ///
    /// # Errors
    ///
    /// Whatever `operation` returns.
    ///
    /// ```no_run
    /// # use moso_orm::{Result, Tx};
    /// # async fn example(tx: &Tx) -> Result<()> {
    /// tx.savepoint(async |sp| {
    ///     let _ = sp;
    ///     Ok(())
    /// })
    /// .await
    /// # }
    /// ```
    pub async fn savepoint<F, T>(&self, operation: F) -> Result<T>
    where
        F: AsyncFnOnce(&Tx) -> Result<T>,
    {
        // The counter is on the shared state, so two sibling savepoints never
        // pick the same name even though their depths are equal.
        let ordinal = self.state.savepoints.fetch_add(1, Ordering::Relaxed) + 1;
        let name = format!("moso_sp_{ordinal}");
        self.run_control(&format!("savepoint {name}")).await?;

        let nested = Self {
            db: self.db.clone(),
            options: self.options.clone(),
            depth: self.depth + 1,
            state: Arc::clone(&self.state),
        };

        // A panic here unwinds without releasing the savepoint, and the outer
        // transaction is rolled back whole when its `Tx` drops — which is the
        // conservative answer, and the same one PostgreSQL would reach.
        match operation(&nested).await {
            Ok(value) => {
                drop(nested);
                self.run_control(&format!("release savepoint {name}"))
                    .await?;
                Ok(value)
            }
            Err(error) => {
                drop(nested);
                // The rollback failing is not the error worth surfacing: the
                // reason the body failed is. It is logged rather than lost.
                if let Err(rollback) = self
                    .run_control(&format!("rollback to savepoint {name}"))
                    .await
                {
                    tracing::error!("db: rolling back to {name} failed: {rollback}");
                }
                Err(error)
            }
        }
    }

    /// Whether the transaction has been committed or rolled back.
    ///
    /// ```no_run
    /// # use moso_orm::Tx;
    /// fn done(tx: &Tx) -> bool {
    ///     tx.is_closed()
    /// }
    /// ```
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.state.closed.load(Ordering::Relaxed)
    }

    /// Takes a PostgreSQL advisory lock for the rest of this transaction.
    ///
    /// The lock is released by the commit or the rollback, so there is no guard
    /// to drop and no way to leak one — which is why this is the form to reach
    /// for. [`Db::advisory_lock`](crate::Db::advisory_lock) is the session-level
    /// alternative, for work that outlives a transaction.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] on SQLite, which has no advisory locks.
    ///
    /// ```no_run
    /// # use moso_orm::{Result, Tx};
    /// # use moso_orm::db::AdvisoryKey;
    /// # async fn example(tx: &Tx) -> Result<()> {
    /// tx.advisory_lock(AdvisoryKey::hashed("rebuild-search-index")).await
    /// # }
    /// ```
    pub async fn advisory_lock(&self, key: AdvisoryKey) -> Result<()> {
        self.take_advisory_lock(key, true).await.map(|_| ())
    }

    /// Takes a transaction-scoped advisory lock, or reports that someone else
    /// has it.
    ///
    /// # Errors
    ///
    /// As [`Tx::advisory_lock`].
    ///
    /// ```no_run
    /// # use moso_orm::{Result, Tx};
    /// # use moso_orm::db::AdvisoryKey;
    /// # async fn example(tx: &Tx) -> Result<bool> {
    /// tx.try_advisory_lock(AdvisoryKey::of(1)).await
    /// # }
    /// ```
    pub async fn try_advisory_lock(&self, key: AdvisoryKey) -> Result<bool> {
        self.take_advisory_lock(key, false).await
    }

    // ── crate internals ───────────────────────────────────────────────────

    /// The driver transaction, locked for one statement.
    pub(crate) async fn guard(&self) -> TxGuard<'_> {
        TxGuard {
            inner: self.state.handle.lock().await,
        }
    }

    /// [`Tx::commit`] through a shared reference.
    ///
    /// [`RequestTx`] holds an `Arc<Tx>` that the handler may still have a clone
    /// of when the response is written, so the layer cannot take ownership to
    /// call `commit`. Both spellings do the same thing: the driver transaction
    /// lives behind the mutex, and ending it is taking it out.
    ///
    /// # Errors
    ///
    /// As [`Tx::commit`].
    pub(crate) async fn commit_shared(&self) -> Result<()> {
        self.finish(true).await
    }

    /// [`Tx::rollback`] through a shared reference. See
    /// [`Tx::commit_shared`].
    ///
    /// # Errors
    ///
    /// As [`Tx::rollback`].
    pub(crate) async fn rollback_shared(&self) -> Result<()> {
        self.finish(false).await
    }

    /// Commits or rolls back, and marks the handle closed either way.
    async fn finish(&self, commit: bool) -> Result<()> {
        if self.depth > 0 {
            // A `Tx` handed to a savepoint closure is not the transaction; the
            // savepoint's own `RELEASE`/`ROLLBACK TO` is issued by
            // `Tx::savepoint`, which owns that decision.
            return Err(Error::Connection {
                detail: String::from(
                    "a savepoint handle cannot commit or roll back the whole transaction\n  \
                     help: return `Ok` from the `savepoint(..)` closure to release it, or `Err` \
                     to roll back to it",
                ),
            });
        }
        let taken = self.state.handle.lock().await.take();
        let Some(handle) = taken else {
            return Ok(());
        };
        self.state.closed.store(true, Ordering::Relaxed);

        // Annotated so a backend-less build, where only the diverging
        // `Unbacked` arm survives, still knows the result type.
        let outcome: core::result::Result<(), sqlx::Error> = match handle {
            #[cfg(feature = "postgres")]
            TxHandle::Postgres(transaction) => {
                if commit {
                    transaction.commit().await
                } else {
                    transaction.rollback().await
                }
            }
            #[cfg(feature = "sqlite")]
            TxHandle::Sqlite(transaction) => {
                if commit {
                    transaction.commit().await
                } else {
                    transaction.rollback().await
                }
            }
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            TxHandle::Unbacked(never) => match never {},
        };
        outcome.map_err(|error| {
            self.db
                .translate(error, if commit { "commit" } else { "rollback" })
        })
    }

    /// Runs a transaction-control statement — a savepoint, a `SET LOCAL` — on
    /// this transaction's connection.
    async fn run_control(&self, statement: &str) -> Result<()> {
        let mut guard = self.guard().await;
        #[cfg(feature = "postgres")]
        if self.db.backend() == Backend::Postgres {
            use sqlx::Executor as _;
            let connection = guard.postgres()?;
            connection
                .execute(sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_owned())))
                .await
                .map_err(|error| self.db.translate(error, statement))?;
            return Ok(());
        }
        #[cfg(feature = "sqlite")]
        if self.db.backend() == Backend::Sqlite {
            use sqlx::Executor as _;
            let connection = guard.sqlite()?;
            connection
                .execute(sqlx::raw_sql(sqlx::AssertSqlSafe(statement.to_owned())))
                .await
                .map_err(|error| self.db.translate(error, statement))?;
            return Ok(());
        }
        Err(Error::Unsupported {
            feature: "transaction control statements",
            backend: self.db.backend(),
        })
    }

    /// Applies the per-transaction timeouts, which are `SET LOCAL` so that they
    /// revert with the transaction and survive a pooled connection.
    async fn apply_local_timeouts(&self) -> Result<()> {
        if self.db.backend() != Backend::Postgres {
            // SQLite's equivalent is the busy timeout, which is a connection
            // setting rather than a transaction one and is applied at connect.
            return Ok(());
        }
        if let Some(timeout) = self.options.statement_timeout {
            self.run_control(&format!(
                "set local statement_timeout = {}",
                timeout.as_millis()
            ))
            .await?;
        }
        if let Some(timeout) = self.options.lock_timeout {
            self.run_control(&format!("set local lock_timeout = {}", timeout.as_millis()))
                .await?;
        }
        Ok(())
    }

    /// The shared implementation of the two transaction-scoped advisory locks.
    #[cfg(feature = "postgres")]
    async fn take_advisory_lock(&self, key: AdvisoryKey, blocking: bool) -> Result<bool> {
        if self.db.backend() != Backend::Postgres {
            return Err(Error::Unsupported {
                feature: "advisory locks",
                backend: self.db.backend(),
            });
        }
        let mut guard = self.guard().await;
        let connection = guard.postgres()?;
        if blocking {
            let statement = "select pg_advisory_xact_lock($1)";
            sqlx::query(statement)
                .bind(key.as_i64())
                .execute(&mut *connection)
                .await
                .map_err(|error| self.db.translate(error, statement))?;
            return Ok(true);
        }
        let statement = "select pg_try_advisory_xact_lock($1)";
        sqlx::query_scalar::<_, bool>(statement)
            .bind(key.as_i64())
            .fetch_one(&mut *connection)
            .await
            .map_err(|error| self.db.translate(error, statement))
    }

    /// Without the PostgreSQL driver there are no advisory locks to take.
    #[cfg(not(feature = "postgres"))]
    #[expect(
        clippy::unused_async,
        reason = "the signature matches the PostgreSQL build"
    )]
    async fn take_advisory_lock(&self, key: AdvisoryKey, blocking: bool) -> Result<bool> {
        let _ = (key, blocking);
        Err(Error::Unsupported {
            feature: "advisory locks",
            backend: self.db.backend(),
        })
    }
}

impl fmt::Debug for Tx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tx")
            .field("isolation", &self.options.isolation)
            .field("depth", &self.depth)
            .finish_non_exhaustive()
    }
}

/// How a transaction is opened.
///
/// ```
/// use core::time::Duration;
/// use moso_orm::{Isolation, TxOptions};
///
/// let strict = TxOptions::new()
///     .isolation(Isolation::Serializable)
///     .max_retries(5)
///     .statement_timeout(Duration::from_secs(5));
///
/// assert_eq!(strict.isolation, Isolation::Serializable);
/// assert_eq!(strict.max_retries, 5);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct TxOptions {
    /// The isolation level. `ReadCommitted` by default, which is PostgreSQL's.
    pub isolation: Isolation,
    /// Whether the transaction refuses writes.
    pub read_only: bool,
    /// `DEFERRABLE`, which only means anything for a read-only serialisable
    /// transaction, where it trades latency for never being aborted.
    pub deferrable: bool,
    /// How many times a serialisation failure or a deadlock is retried.
    /// Default 3.
    pub max_retries: u32,
    /// A `statement_timeout` for this transaction only.
    pub statement_timeout: Option<Duration>,
    /// A `lock_timeout` for this transaction only.
    pub lock_timeout: Option<Duration>,
}

impl TxOptions {
    /// The defaults: read-committed, writable, three retries.
    ///
    /// ```
    /// use moso_orm::{Isolation, TxOptions};
    ///
    /// assert_eq!(TxOptions::new().isolation, Isolation::ReadCommitted);
    /// assert_eq!(TxOptions::new().max_retries, 3);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            isolation: Isolation::ReadCommitted,
            read_only: false,
            deferrable: false,
            max_retries: 3,
            statement_timeout: None,
            lock_timeout: None,
        }
    }

    /// Sets the isolation level.
    ///
    /// ```
    /// use moso_orm::{Isolation, TxOptions};
    ///
    /// let o = TxOptions::new().isolation(Isolation::RepeatableRead);
    /// assert_eq!(o.isolation, Isolation::RepeatableRead);
    /// ```
    #[must_use]
    pub const fn isolation(mut self, isolation: Isolation) -> Self {
        self.isolation = isolation;
        self
    }

    /// Makes the transaction refuse writes, which also lets it run on a
    /// replica.
    ///
    /// ```
    /// use moso_orm::TxOptions;
    ///
    /// assert!(TxOptions::new().read_only().read_only);
    /// ```
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// Marks the transaction `DEFERRABLE`.
    ///
    /// ```
    /// use moso_orm::TxOptions;
    ///
    /// assert!(TxOptions::new().deferrable().deferrable);
    /// ```
    #[must_use]
    pub const fn deferrable(mut self) -> Self {
        self.deferrable = true;
        self
    }

    /// Sets how many times a transient failure is retried.
    ///
    /// Zero disables retrying, which is what [`RequestTx`] does: the request
    /// body may already have been consumed.
    ///
    /// ```
    /// use moso_orm::TxOptions;
    ///
    /// assert_eq!(TxOptions::new().max_retries(0).max_retries, 0);
    /// ```
    #[must_use]
    pub const fn max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Sets a `statement_timeout` for this transaction.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::TxOptions;
    ///
    /// let o = TxOptions::new().statement_timeout(Duration::from_secs(2));
    /// assert_eq!(o.statement_timeout, Some(Duration::from_secs(2)));
    /// ```
    #[must_use]
    pub const fn statement_timeout(mut self, timeout: Duration) -> Self {
        self.statement_timeout = Some(timeout);
        self
    }

    /// Sets a `lock_timeout` for this transaction.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::TxOptions;
    ///
    /// let o = TxOptions::new().lock_timeout(Duration::from_millis(500));
    /// assert_eq!(o.lock_timeout, Some(Duration::from_millis(500)));
    /// ```
    #[must_use]
    pub const fn lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = Some(timeout);
        self
    }

    /// Whether this transaction retries at all.
    ///
    /// ```
    /// use moso_orm::TxOptions;
    ///
    /// assert!(TxOptions::new().retries());
    /// assert!(!TxOptions::new().max_retries(0).retries());
    /// ```
    #[must_use]
    pub const fn retries(&self) -> bool {
        self.max_retries > 0
    }

    /// How long to wait before retry number `attempt`, before jitter.
    ///
    /// Exponential from 20 ms, capped at a second, and monotonic — which is why
    /// it is the *base* rather than the wait itself.
    /// [`TxOptions::backoff_jittered`] is what the retry loop sleeps for, and
    /// it is the one that spreads competing transactions out; without the
    /// spread, every loser of a serialisation race retries at the same instant
    /// and collides again.
    ///
    /// ```
    /// use moso_orm::TxOptions;
    ///
    /// let first = TxOptions::new().backoff(1);
    /// let third = TxOptions::new().backoff(3);
    /// assert!(third >= first);
    /// ```
    #[must_use]
    pub fn backoff(&self, attempt: u32) -> Duration {
        let base = Duration::from_millis(20).saturating_mul(1_u32 << attempt.min(6));
        base.min(Duration::from_secs(1))
    }

    /// [`TxOptions::backoff`] spread over ±25%, which is what the retry loop
    /// actually sleeps for.
    ///
    /// The spread is narrow enough that the sequence still grows — attempt 3 is
    /// never shorter than attempt 1 — and wide enough that two transactions
    /// that collided do not collide again at the same microsecond.
    ///
    /// ```
    /// use moso_orm::TxOptions;
    ///
    /// let options = TxOptions::new();
    /// let base = options.backoff(2);
    /// let wait = options.backoff_jittered(2);
    /// assert!(wait >= base * 3 / 4 && wait <= base);
    /// ```
    #[must_use]
    pub fn backoff_jittered(&self, attempt: u32) -> Duration {
        let base = self.backoff(attempt);
        // `Uuid::v4` is the workspace's only source of operating-system
        // randomness; a hash of the clock would correlate across the very
        // tasks the jitter exists to separate.
        let entropy = uuid::Uuid::new_v4().as_u128() as u64;
        let fraction = (entropy % 1024) as f64 / 1024.0;
        base.mul_f64(0.75 + fraction / 4.0)
    }

    /// The `BEGIN` this transaction opens with.
    ///
    /// PostgreSQL takes the isolation level and access mode on `BEGIN` itself,
    /// so there is no window in which the transaction is open at the wrong
    /// level. SQLite has one isolation level and takes `IMMEDIATE` instead,
    /// which acquires the write lock up front rather than discovering halfway
    /// through that another writer has it.
    ///
    /// ```
    /// use moso_orm::{Backend, Isolation, TxOptions};
    ///
    /// let options = TxOptions::new().isolation(Isolation::Serializable).read_only();
    /// assert_eq!(
    ///     options.begin_statement(Backend::Postgres),
    ///     "begin isolation level serializable read only",
    /// );
    /// assert_eq!(TxOptions::new().begin_statement(Backend::Sqlite), "begin immediate");
    /// ```
    #[must_use]
    pub fn begin_statement(&self, backend: Backend) -> String {
        match backend {
            Backend::Sqlite => {
                if self.read_only {
                    String::from("begin")
                } else {
                    String::from("begin immediate")
                }
            }
            Backend::Postgres => {
                let mut statement = String::from("begin isolation level ");
                statement.push_str(self.isolation.as_sql_lowercase());
                statement.push_str(if self.read_only {
                    " read only"
                } else {
                    " read write"
                });
                // `DEFERRABLE` is only meaningful — and only accepted as
                // anything but a no-op — for a read-only serialisable
                // transaction, so it is not passed on where it would be noise.
                if self.deferrable && self.read_only && self.isolation == Isolation::Serializable {
                    statement.push_str(" deferrable");
                }
                statement
            }
        }
    }
}

impl Default for TxOptions {
    fn default() -> Self {
        Self::new()
    }
}

/// What a transaction may see of concurrent ones.
///
/// ```
/// use moso_orm::Isolation;
///
/// assert!(Isolation::Serializable.can_fail_to_serialise());
/// assert!(!Isolation::ReadCommitted.can_fail_to_serialise());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Isolation {
    /// Each statement sees rows committed before it started. PostgreSQL's
    /// default, and Moso's.
    #[default]
    ReadCommitted,
    /// Every statement sees the same snapshot. Can abort with `40001`.
    RepeatableRead,
    /// As if the transactions had run one after another. Can abort with
    /// `40001`, and that is the point.
    Serializable,
}

impl Isolation {
    /// Whether this level can abort with a serialisation failure, and therefore
    /// needs the retry loop.
    ///
    /// ```
    /// use moso_orm::Isolation;
    ///
    /// assert!(Isolation::RepeatableRead.can_fail_to_serialise());
    /// ```
    #[must_use]
    pub const fn can_fail_to_serialise(self) -> bool {
        matches!(self, Self::RepeatableRead | Self::Serializable)
    }

    /// The `SET TRANSACTION ISOLATION LEVEL` clause.
    ///
    /// ```
    /// use moso_orm::Isolation;
    ///
    /// assert_eq!(Isolation::Serializable.as_sql(), "SERIALIZABLE");
    /// ```
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::ReadCommitted => "READ COMMITTED",
            Self::RepeatableRead => "REPEATABLE READ",
            Self::Serializable => "SERIALIZABLE",
        }
    }

    /// The same clause in the lower case the rest of Moso's generated SQL uses,
    /// so that a statement log reads consistently.
    ///
    /// ```
    /// use moso_orm::Isolation;
    ///
    /// assert_eq!(Isolation::RepeatableRead.as_sql_lowercase(), "repeatable read");
    /// ```
    #[must_use]
    pub const fn as_sql_lowercase(self) -> &'static str {
        match self {
            Self::ReadCommitted => "read committed",
            Self::RepeatableRead => "repeatable read",
            Self::Serializable => "serializable",
        }
    }
}

/// Whether re-running the same transaction body has a real chance of working.
///
/// Deliberately **narrower** than [`Error::is_retryable`]: that one includes
/// [`Error::PoolTimeout`], which a transaction retry loop must not treat as a
/// conflict. A pool timeout means the process is out of connections, and
/// retrying immediately makes that worse rather than better; it is reported so
/// the caller — usually the HTTP layer, as a `503` with a `Retry-After` — can
/// shed the load instead.
///
/// ```
/// use moso_orm::Error;
///
/// let lost = Error::Serialization { code: "40001".into() };
/// assert!(lost.is_retryable());
/// ```
pub(crate) const fn is_transient_conflict(error: &Error) -> bool {
    matches!(error, Error::Serialization { .. } | Error::Deadlock { .. })
}

impl Db {
    /// Opens the driver transaction with an explicit `BEGIN`.
    ///
    /// Lives here rather than in `db.rs` because [`TxHandle`] is this module's
    /// type; `Db` is the one that owns the pool.
    pub(crate) async fn begin_driver_transaction(&self, begin: &str) -> Result<TxHandle> {
        let started = std::time::Instant::now();
        match self.pool_handle() {
            #[cfg(feature = "postgres")]
            crate::db::PoolHandle::Postgres(pool) => pool
                .begin_with(sqlx::AssertSqlSafe(begin.to_owned()))
                .await
                .map(TxHandle::Postgres)
                .map_err(|error| self.map_acquire_error(error, begin, started.elapsed())),
            #[cfg(feature = "sqlite")]
            crate::db::PoolHandle::Sqlite(pool) => pool
                .begin_with(sqlx::AssertSqlSafe(begin.to_owned()))
                .await
                .map(TxHandle::Sqlite)
                .map_err(|error| self.map_acquire_error(error, begin, started.elapsed())),
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            crate::db::PoolHandle::Unbacked(never) => match *never {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_read_committed_with_three_retries() {
        let options = TxOptions::default();
        assert_eq!(options.isolation, Isolation::ReadCommitted);
        assert!(!options.read_only);
        assert_eq!(options.max_retries, 3);
        assert!(options.retries());
    }

    #[test]
    fn only_the_snapshot_levels_can_fail_to_serialise() {
        assert!(!Isolation::ReadCommitted.can_fail_to_serialise());
        assert!(Isolation::RepeatableRead.can_fail_to_serialise());
        assert!(Isolation::Serializable.can_fail_to_serialise());
    }

    #[test]
    fn the_isolation_clauses_are_the_sql_spellings() {
        assert_eq!(Isolation::ReadCommitted.as_sql(), "READ COMMITTED");
        assert_eq!(Isolation::RepeatableRead.as_sql(), "REPEATABLE READ");
        assert_eq!(Isolation::Serializable.as_sql(), "SERIALIZABLE");
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let options = TxOptions::new();
        let mut previous = Duration::ZERO;
        for attempt in 1..=8 {
            let wait = options.backoff(attempt);
            assert!(wait >= previous, "attempt {attempt} went backwards");
            assert!(
                wait <= Duration::from_secs(1),
                "attempt {attempt} unbounded"
            );
            previous = wait;
        }
    }

    #[test]
    fn the_jitter_spreads_without_reordering_the_attempts() {
        let options = TxOptions::new();
        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let wait = options.backoff_jittered(3);
            let base = options.backoff(3);
            assert!(wait <= base, "the jitter only ever shortens the base");
            assert!(wait >= base * 3 / 4, "and never by more than a quarter");
            seen.insert(wait.as_micros());
        }
        assert!(
            seen.len() > 20,
            "the backoff is not actually jittered: {} distinct waits",
            seen.len()
        );

        // The spread is narrow enough that a later attempt still waits longer.
        assert!(options.backoff_jittered(4) > options.backoff_jittered(1));
    }

    #[test]
    fn the_begin_statement_carries_the_level_and_the_access_mode() {
        assert_eq!(
            TxOptions::new().begin_statement(Backend::Postgres),
            "begin isolation level read committed read write"
        );
        assert_eq!(
            TxOptions::new()
                .isolation(Isolation::Serializable)
                .read_only()
                .deferrable()
                .begin_statement(Backend::Postgres),
            "begin isolation level serializable read only deferrable"
        );
        assert_eq!(
            TxOptions::new()
                .deferrable()
                .begin_statement(Backend::Postgres),
            "begin isolation level read committed read write",
            "DEFERRABLE means nothing outside a read-only serialisable transaction"
        );
        assert_eq!(
            TxOptions::new().begin_statement(Backend::Sqlite),
            "begin immediate",
            "a writer takes the lock up front rather than upgrading mid-transaction"
        );
        assert_eq!(
            TxOptions::new()
                .read_only()
                .begin_statement(Backend::Sqlite),
            "begin"
        );
    }

    #[test]
    fn a_request_transaction_never_retries() {
        // Recorded as a test because it is a semantic decision, not an
        // implementation detail: the body may already have been consumed.
        let options = TxOptions::new().max_retries(0);
        assert!(!options.retries());
    }
}

#[cfg(test)]
mod real_database {
    use super::*;
    use crate::db::test_support::{postgres, sqlite, unique_table};
    use crate::db::{AdvisoryKey, DatabaseConfig};
    use crate::executor::Executor as _;
    use moso_sql::Sql;
    use std::sync::atomic::AtomicU32;

    /// Runs SQL with no parameters on anything that is an executor.
    async fn run<'e>(executor: impl crate::Executor<'e>, sql: &str) -> Result<u64> {
        executor
            .handle()
            .execute_sql(Sql::new(sql.to_owned(), []))
            .await
    }

    /// How many rows a `select count(*)` finds, without needing the row
    /// decoders: a `delete` that matches the same predicate reports the count.
    async fn count_via_delete(db: &Db, table: &str, predicate: &str) -> u64 {
        run(db, &format!("delete from {table} where {predicate}"))
            .await
            .expect("delete")
    }

    #[tokio::test]
    async fn a_commit_persists_and_a_rollback_does_not() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key, v integer)")
            .await
            .expect("create");

        let tx = db.begin().await.expect("begin");
        run(&tx, "insert into t values (1, 10)")
            .await
            .expect("insert");
        tx.commit().await.expect("commit");

        let tx = db.begin().await.expect("begin");
        run(&tx, "insert into t values (2, 20)")
            .await
            .expect("insert");
        tx.rollback().await.expect("rollback");

        assert_eq!(
            count_via_delete(&db, "t", "id = 1").await,
            1,
            "the commit stuck"
        );
        assert_eq!(
            count_via_delete(&db, "t", "id = 2").await,
            0,
            "the rollback did not"
        );
    }

    #[tokio::test]
    async fn dropping_an_open_transaction_rolls_it_back() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        {
            let tx = db.begin().await.expect("begin");
            run(&tx, "insert into t values (1)").await.expect("insert");
            // No commit, no rollback: the handle simply goes out of scope.
        }

        assert_eq!(count_via_delete(&db, "t", "id = 1").await, 0);
        db.ping()
            .await
            .expect("and the connection came back usable");
    }

    /// Acceptance criterion 3: a panic rolls back and does not poison the pool.
    #[tokio::test]
    async fn a_panic_rolls_back_and_leaves_the_pool_usable() {
        use futures_util::FutureExt as _;

        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        let panicked = std::panic::AssertUnwindSafe(async {
            let outcome: Result<()> = db
                .transaction(async |tx| {
                    run(tx, "insert into t values (1)").await?;
                    panic!("the handler blew up half way through")
                })
                .await;
            outcome
        })
        .catch_unwind()
        .await;
        assert!(
            panicked.is_err(),
            "the panic propagates rather than being eaten"
        );

        assert_eq!(
            count_via_delete(&db, "t", "id = 1").await,
            0,
            "the row the panicking body wrote was rolled back"
        );
        db.ping().await.expect("the connection is not poisoned");
        run(&db, "insert into t values (2)")
            .await
            .expect("and the pool still works");
    }

    #[tokio::test]
    async fn a_savepoint_releases_on_ok_and_rolls_back_on_err() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        db.transaction(async |tx| {
            run(tx, "insert into t values (1)").await?;

            tx.savepoint(async |sp| {
                run(sp, "insert into t values (2)").await?;
                Ok(())
            })
            .await?;

            // A savepoint whose body fails takes its own writes with it and
            // leaves the outer transaction alive.
            let failed: Result<()> = tx
                .savepoint(async |sp| {
                    run(sp, "insert into t values (3)").await?;
                    Err(Error::not_found("deliberate"))
                })
                .await;
            assert!(failed.is_err());

            // Nesting works, and the outer transaction is still writable.
            tx.savepoint(async |sp| {
                sp.savepoint(async |inner| {
                    run(inner, "insert into t values (4)").await?;
                    Ok(())
                })
                .await
            })
            .await?;

            run(tx, "insert into t values (5)").await?;
            Ok(())
        })
        .await
        .expect("the transaction as a whole succeeded");

        for (id, expected) in [(1, 1), (2, 1), (3, 0), (4, 1), (5, 1)] {
            assert_eq!(
                count_via_delete(&db, "t", &format!("id = {id}")).await,
                expected,
                "row {id}"
            );
        }
    }

    #[tokio::test]
    async fn a_savepoint_handle_cannot_end_the_whole_transaction() {
        let db = sqlite().await;
        db.transaction(async |tx| {
            let error = tx
                .savepoint(async |sp| {
                    // `finish` is what `commit`/`rollback` call; a savepoint
                    // handle refuses, because releasing it is the closure's
                    // return value and nothing else.
                    Ok(sp
                        .commit_shared()
                        .await
                        .expect_err("a savepoint cannot commit"))
                })
                .await?;
            assert!(error.to_string().contains("savepoint"), "{error}");
            Ok(())
        })
        .await
        .expect("transaction");
    }

    #[tokio::test]
    async fn a_read_only_transaction_refuses_a_write_before_it_is_sent() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        let tx = db
            .begin_with(TxOptions::new().read_only())
            .await
            .expect("begin read only");

        let statement =
            moso_sql::Statement::Raw(moso_sql::RawStatement::new("insert into t values (1)"));
        let error = tx
            .handle()
            .execute(&statement)
            .await
            .expect_err("a read-only transaction refuses a write");
        assert!(error.to_string().contains("read-only"), "{error}");
        tx.rollback().await.expect("rollback");
    }

    #[tokio::test]
    async fn a_transaction_scoped_advisory_lock_is_released_by_the_commit() {
        let Some(db) = postgres().await else { return };
        let key = AdvisoryKey::hashed(&unique_table("moso_xact_lock"));

        db.transaction(async |tx| {
            tx.advisory_lock(key).await?;
            // The same transaction may take it again; PostgreSQL advisory locks
            // are re-entrant within a session.
            assert!(tx.try_advisory_lock(key).await?);
            Ok(())
        })
        .await
        .expect("transaction");

        // Committing released it, with no guard to remember to drop.
        let after = db.try_advisory_lock(key).await.expect("ask");
        assert!(after.is_some(), "the commit released the transaction lock");
        drop(after);
        db.close().await;
    }

    /// Acceptance criterion 2, first half: a serialisation failure retries and
    /// eventually succeeds.
    ///
    /// The conflict is forced rather than hoped for: both transactions take
    /// their snapshot before either writes, which is exactly the write skew
    /// `SERIALIZABLE` exists to catch.
    #[tokio::test]
    async fn a_serialisation_failure_retries_and_then_succeeds() {
        let Some(db) = postgres().await else { return };
        let table = unique_table("moso_ser");
        run(
            &db,
            &format!("create table {table} (id int primary key, v int)"),
        )
        .await
        .expect("create");
        run(&db, &format!("insert into {table} values (1, 0), (2, 0)"))
            .await
            .expect("seed");

        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let options = TxOptions::new()
            .isolation(Isolation::Serializable)
            .max_retries(5);

        /// One half of the write skew. Only the *first* attempt waits at the
        /// gate; a retry must not, because its partner is not coming back.
        async fn skew(
            db: &Db,
            options: TxOptions,
            table: &str,
            read: i32,
            write: i32,
            gate: Arc<tokio::sync::Barrier>,
        ) -> Result<()> {
            let attempts = AtomicU32::new(0);
            db.transaction_with(options, async |tx| {
                let first = attempts.fetch_add(1, Ordering::Relaxed) == 0;
                run(tx, &format!("select v from {table} where id = {read}")).await?;
                if first {
                    gate.wait().await;
                }
                run(tx, &format!("update {table} set v = 1 where id = {write}")).await?;
                Ok(())
            })
            .await
        }

        let (a, b) = tokio::join!(
            skew(&db, options.clone(), &table, 1, 2, Arc::clone(&gate)),
            skew(&db, options, &table, 2, 1, Arc::clone(&gate)),
        );
        a.expect("the first half retried its way through");
        b.expect("and so did the second");

        run(&db, &format!("drop table {table}"))
            .await
            .expect("drop");
        db.close().await;
    }

    /// The same race with retrying turned off: exactly one of the two loses,
    /// and the error is the one a client can act on.
    #[tokio::test]
    async fn without_retries_one_half_of_the_race_reports_the_conflict() {
        let Some(db) = postgres().await else { return };
        let table = unique_table("moso_ser_noretry");
        run(
            &db,
            &format!("create table {table} (id int primary key, v int)"),
        )
        .await
        .expect("create");
        run(&db, &format!("insert into {table} values (1, 0), (2, 0)"))
            .await
            .expect("seed");

        let gate = Arc::new(tokio::sync::Barrier::new(2));
        let options = TxOptions::new()
            .isolation(Isolation::Serializable)
            .max_retries(0);

        async fn skew(
            db: &Db,
            options: TxOptions,
            table: &str,
            read: i32,
            write: i32,
            gate: Arc<tokio::sync::Barrier>,
        ) -> Result<()> {
            db.transaction_with(options, async |tx| {
                run(tx, &format!("select v from {table} where id = {read}")).await?;
                gate.wait().await;
                run(tx, &format!("update {table} set v = 1 where id = {write}")).await?;
                Ok(())
            })
            .await
        }

        let (a, b) = tokio::join!(
            skew(&db, options.clone(), &table, 1, 2, Arc::clone(&gate)),
            skew(&db, options, &table, 2, 1, Arc::clone(&gate)),
        );

        let losers: Vec<Error> = [a, b].into_iter().filter_map(Result::err).collect();
        assert_eq!(losers.len(), 1, "exactly one transaction loses the race");
        let error = &losers[0];
        assert!(
            matches!(error, Error::Serialization { .. }),
            "a lost serialisation race must be reported as one, not as a 500: {error:?}"
        );
        assert!(error.is_retryable());
        assert_eq!(error.sqlstate(), Some("40001"));

        run(&db, &format!("drop table {table}"))
            .await
            .expect("drop");
        db.close().await;
    }

    /// Acceptance criterion 2, second half: a unique violation is **not**
    /// retried, because re-running it would only fail again.
    #[tokio::test]
    async fn a_unique_violation_is_reported_at_once_rather_than_retried() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");
        run(&db, "insert into t values (1)").await.expect("seed");

        let attempts = AtomicU32::new(0);
        let error = db
            .transaction(async |tx| {
                attempts.fetch_add(1, Ordering::Relaxed);
                run(tx, "insert into t values (1)").await
            })
            .await
            .expect_err("the primary key is taken");

        assert!(
            matches!(error, Error::UniqueViolation(_)),
            "a duplicate key is a unique violation, not an opaque database error: {error:?}"
        );
        assert_eq!(
            attempts.load(Ordering::Relaxed),
            1,
            "the body ran once: retrying a unique violation only repeats it"
        );
        assert!(!error.is_retryable());
    }

    #[tokio::test]
    async fn the_isolation_level_reaches_the_server() {
        let Some(db) = postgres().await else { return };
        for (level, expected) in [
            (Isolation::ReadCommitted, "read committed"),
            (Isolation::RepeatableRead, "repeatable read"),
            (Isolation::Serializable, "serializable"),
        ] {
            let tx = db
                .begin_with(TxOptions::new().isolation(level))
                .await
                .expect("begin");
            // `current_setting` reports what the server actually applied, so
            // this fails if the `BEGIN` clause were dropped on the floor.
            let matched = run(
                &tx,
                &format!("select 1 where current_setting('transaction_isolation') = '{expected}'"),
            )
            .await
            .expect("select");
            assert_eq!(matched, 1, "{level:?} did not reach the server");
            tx.rollback().await.expect("rollback");
        }
        db.close().await;
    }

    #[tokio::test]
    async fn a_read_only_transaction_is_refused_by_the_server_too() {
        let Some(db) = postgres().await else { return };
        let table = unique_table("moso_ro");
        run(&db, &format!("create table {table} (id int)"))
            .await
            .expect("create");

        let tx = db
            .begin_with(TxOptions::new().read_only())
            .await
            .expect("begin");
        // Raw SQL is not classified by Moso, so this is PostgreSQL's own
        // `READ ONLY` enforcement rather than the guard in `Handle`.
        let error = run(&tx, &format!("insert into {table} values (1)"))
            .await
            .expect_err("the server refuses the write");
        assert!(
            error.sqlstate() == Some("25006"),
            "expected `read_only_sql_transaction`, got {error:?}"
        );
        drop(tx);

        run(&db, &format!("drop table {table}"))
            .await
            .expect("drop");
        db.close().await;
    }

    #[tokio::test]
    async fn a_per_transaction_statement_timeout_is_applied() {
        let Some(db) = postgres().await else { return };
        let error = db
            .transaction_with(
                TxOptions::new().statement_timeout(Duration::from_millis(100)),
                async |tx| run(tx, "select pg_sleep(3)").await.map(|_| ()),
            )
            .await
            .expect_err("three seconds is more than a tenth of one");

        assert!(
            matches!(error, Error::StatementTimeout { .. }),
            "a cancelled statement must say so: {error:?}"
        );
        assert!(error.to_string().contains("help:"), "{error}");
        db.close().await;
    }

    #[tokio::test]
    async fn a_statement_on_a_finished_transaction_says_so() {
        let db = sqlite().await;
        let tx = db.begin().await.expect("begin");
        tx.commit_shared().await.expect("commit");
        assert!(tx.is_closed());

        let error = run(&tx, "select 1")
            .await
            .expect_err("the transaction is over");
        assert!(
            error.to_string().contains("already been committed"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn opening_a_transaction_on_a_configured_pool_uses_one_connection() {
        let config = DatabaseConfig::from_url("sqlite://:memory:");
        let db = Db::connect(&config).await.expect("open");
        let tx = db.begin().await.expect("begin");
        assert_eq!(tx.depth(), 0);
        assert!(!tx.is_closed());
        assert_eq!(tx.options().isolation, Isolation::ReadCommitted);
        assert!(core::ptr::eq(tx.db().statements(), db.statements()));
        tx.rollback().await.expect("rollback");
    }
}
