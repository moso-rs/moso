//! [`Executor`] — the one bound that makes a service function work inside and
//! outside a transaction.
//!
//! ```no_run
//! use moso_orm::{Executor, Result};
//!
//! /// Works with `&db`, with `&tx`, and with `&mut tx`. One signature.
//! async fn touch(executor: impl Executor<'_>) -> Result<u64> {
//!     let _ = executor.handle();
//!     Ok(0)
//! }
//! ```
//!
//! # Erase early (rule A2)
//!
//! The trait has exactly **one** required method, [`Executor::handle`], which
//! turns any of the three implementors into the concrete [`Handle`]. Everything
//! else — building the statement, binding, counting, tracing, decoding — lives
//! on `Handle` and is not generic, so `Select<User>::fetch_all` monomorphises
//! once per entity rather than once per (entity, executor) pair.
//!
//! That is a deliberate compile-time decision, and it is why the trait is
//! sealed: a fourth implementor would have to be a fourth `Handle` variant
//! anyway.
//!
//! # What every statement goes through
//!
//! 1. The statement is rendered for the handle's dialect.
//! 2. Its parameters are bound through the driver's native encoders, so a value
//!    round-trips through the same representation it will be decoded from.
//! 3. It runs on the transaction's connection, or on one taken from the pool
//!    with [`DatabaseConfig::acquire_timeout`](crate::DatabaseConfig) — a
//!    request waits, bounded, and then gets a `503`.
//! 4. It is counted, timed, and traced in a span carrying the **parameterised**
//!    text and the caller's file and line. Values are never logged.
//! 5. A driver error becomes an error that names the problem: a `23505` is a
//!    [`Error::UniqueViolation`] with a column, not a five-hundred.

use core::fmt;
use core::sync::atomic::{AtomicU64, Ordering};
use core::time::Duration;
use std::time::Instant;

use moso_sql::{Dialect, Sql, Statement, Value};

use crate::db::{Backend, DatabaseConfig, Db, PooledConnection, QuerySample};
use crate::error::{CallSite, ConstraintKind, ConstraintViolation, DatabaseError, Error, Result};
use crate::row::Row;
use crate::tx::{RequestTx, Tx};

/// The entity name a constraint violation carries when the statement did not
/// come from one. See [`Handle::for_entity`].
const ANONYMOUS_ENTITY: &str = "row";

/// Something a statement can run on: `&Db`, `&Tx` or `&mut Tx`.
///
/// Sealed. The three implementors are the three places a connection can come
/// from, and a fourth would have to become a [`Handle`] variant to be useful.
///
/// ```no_run
/// use moso_orm::{Executor, Result};
///
/// async fn count_rows(executor: impl Executor<'_>) -> Result<u64> {
///     let handle = executor.handle();
///     let _ = handle.backend();
///     Ok(0)
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot run a statement",
    label = "not an executor",
    note = "a query runs on `&Db`, on `&Tx`, or on `&mut Tx` — nothing else",
    note = "help: pass `&db` outside a transaction, or the `tx` the closure gives you inside one",
    note = "help: if this is a `Db` by value, borrow it: `&db`"
)]
pub trait Executor<'e>: sealed::Sealed + Sized {
    /// The concrete handle every statement actually runs on.
    ///
    /// ```no_run
    /// # use moso_orm::{Executor, Handle};
    /// fn erase<'e>(executor: impl Executor<'e>) -> Handle<'e> {
    ///     executor.handle()
    /// }
    /// ```
    fn handle(self) -> Handle<'e>;
}

/// Keeps [`Executor`] closed. See the module documentation for why.
mod sealed {
    /// The bound nothing outside this crate can satisfy.
    ///
    /// It is reachable in rustdoc because it is [`Executor`](super::Executor)'s
    /// supertrait, so it carries its own diagnostic: a reader who ends up here
    /// is trying to add a fourth executor, and the message should say why that
    /// does not work rather than showing them a private trait.
    #[diagnostic::on_unimplemented(
        message = "`{Self}` cannot be an executor, because `Executor` is sealed",
        label = "not one of the three executors",
        note = "a statement runs on `&Db`, `&Tx` or `&mut Tx`; a fourth implementor would have \
                to become a `Handle` variant to be able to run anything",
        note = "help: pass one of those three, or open an issue describing the connection source \
                you need"
    )]
    pub trait Sealed {}

    impl Sealed for &super::Db {}
    impl Sealed for &super::Tx {}
    impl Sealed for &mut super::Tx {}
    impl Sealed for &super::RequestTx {}
}

impl<'e> Executor<'e> for &'e Db {
    fn handle(self) -> Handle<'e> {
        Handle {
            db: self,
            tx: None,
            site: None,
            entity: None,
        }
    }
}

impl<'e> Executor<'e> for &'e Tx {
    fn handle(self) -> Handle<'e> {
        Handle {
            db: self.db(),
            tx: Some(self),
            site: None,
            entity: None,
        }
    }
}

impl<'e> Executor<'e> for &'e mut Tx {
    fn handle(self) -> Handle<'e> {
        // A `Tx` runs statements through interior mutability, so an exclusive
        // borrow reborrows as a shared one. Accepting `&mut Tx` at all is
        // ergonomics: it means a caller holding `let mut tx = db.begin()?` does
        // not have to write `&*tx`.
        let shared: &'e Tx = self;
        Handle {
            db: shared.db(),
            tx: Some(shared),
            site: None,
            entity: None,
        }
    }
}

impl<'e> Executor<'e> for &'e RequestTx {
    fn handle(self) -> Handle<'e> {
        self.tx().handle()
    }
}

/// The erased executor: a pool, and possibly a transaction on it.
///
/// Every statement in the crate goes through here, which is what keeps the
/// generic surface one method deep.
///
/// ```no_run
/// # use moso_orm::{Executor, Handle, Result};
/// # use moso_sql::Statement;
/// async fn run(executor: impl Executor<'_>, statement: Statement) -> Result<u64> {
///     executor.handle().execute(&statement).await
/// }
/// ```
#[derive(Clone, Copy)]
pub struct Handle<'e> {
    db: &'e Db,
    tx: Option<&'e Tx>,
    site: Option<CallSite>,
    entity: Option<&'static str>,
}

impl<'e> Handle<'e> {
    /// The pool this handle draws from.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Handle};
    /// fn pool_of<'e>(handle: Handle<'e>) -> &'e Db {
    ///     handle.db()
    /// }
    /// ```
    #[must_use]
    pub const fn db(&self) -> &'e Db {
        self.db
    }

    /// The transaction, when the handle is inside one.
    ///
    /// ```no_run
    /// # use moso_orm::Handle;
    /// fn inside(handle: Handle<'_>) -> bool {
    ///     handle.transaction().is_some()
    /// }
    /// ```
    #[must_use]
    pub const fn transaction(&self) -> Option<&'e Tx> {
        self.tx
    }

    /// Whether the handle is inside a transaction.
    ///
    /// Read by the tracing span, and by the guard that refuses a `LISTEN` in
    /// one.
    ///
    /// ```no_run
    /// # use moso_orm::Handle;
    /// fn atomic(handle: Handle<'_>) -> bool {
    ///     handle.in_transaction()
    /// }
    /// ```
    #[must_use]
    pub const fn in_transaction(&self) -> bool {
        self.tx.is_some()
    }

    /// Which backend the statement will be rendered for.
    ///
    /// ```no_run
    /// # use moso_orm::{Backend, Handle};
    /// fn backend(handle: Handle<'_>) -> Backend {
    ///     handle.backend()
    /// }
    /// ```
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.db.backend()
    }

    /// The dialect the statement will be rendered with.
    ///
    /// ```no_run
    /// # use moso_orm::Handle;
    /// fn name(handle: Handle<'_>) -> &'static str {
    ///     handle.dialect().name()
    /// }
    /// ```
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        self.db.dialect()
    }

    /// The statement counter this handle increments.
    ///
    /// ```no_run
    /// # use moso_orm::Handle;
    /// fn so_far(handle: Handle<'_>) -> u64 {
    ///     handle.statements().total()
    /// }
    /// ```
    #[must_use]
    pub fn statements(&self) -> &'e StatementCounter {
        self.db.statements()
    }

    /// Renders `statement` for this handle's dialect.
    ///
    /// # Errors
    ///
    /// [`crate::Error::Build`] when the dialect cannot express the statement.
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result};
    /// # use moso_sql::{Sql, Statement};
    /// fn render(handle: Handle<'_>, statement: &Statement) -> Result<Sql> {
    ///     handle.build(statement)
    /// }
    /// ```
    pub fn build(&self, statement: &Statement) -> Result<Sql> {
        Ok(statement.build(self.dialect())?)
    }

    /// Runs `statement` and collects every row.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result, Row};
    /// # use moso_sql::Statement;
    /// async fn all(handle: Handle<'_>, statement: &Statement) -> Result<Vec<Row>> {
    ///     handle.fetch_all(statement).await
    /// }
    /// ```
    pub async fn fetch_all(&self, statement: &Statement) -> Result<Vec<Row>> {
        self.guard_read_only(statement)?;
        let sql = self.build(statement)?;
        self.fetch_all_sql(sql).await
    }

    /// Runs `statement` and returns the first row, if there is one.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result, Row};
    /// # use moso_sql::Statement;
    /// async fn maybe(handle: Handle<'_>, statement: &Statement) -> Result<Option<Row>> {
    ///     handle.fetch_optional(statement).await
    /// }
    /// ```
    pub async fn fetch_optional(&self, statement: &Statement) -> Result<Option<Row>> {
        self.guard_read_only(statement)?;
        let sql = self.build(statement)?;
        self.fetch_optional_sql(sql).await
    }

    /// Runs `statement` and returns the number of rows it affected.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result};
    /// # use moso_sql::Statement;
    /// async fn run(handle: Handle<'_>, statement: &Statement) -> Result<u64> {
    ///     handle.execute(statement).await
    /// }
    /// ```
    pub async fn execute(&self, statement: &Statement) -> Result<u64> {
        self.guard_read_only(statement)?;
        let sql = self.build(statement)?;
        self.execute_sql(sql).await
    }

    /// Streams `statement`'s rows without buffering them.
    ///
    /// **Outside a transaction** this is a genuine stream: the driver's cursor
    /// is polled as the caller consumes it, and a result set larger than memory
    /// is fine. The connection is held for as long as the stream lives, which
    /// is the trade a stream makes.
    ///
    /// **Inside a transaction** the rows are read into memory first. The
    /// transaction owns its connection behind a lock, and a stream that
    /// borrowed both the lock and the connection would be a self-referential
    /// type — which cannot be built without `unsafe`, and this crate forbids
    /// it. For a large result set inside a transaction, page with
    /// [`Select::paginate`](crate::Select) instead.
    ///
    /// A rendering failure surfaces as the stream's first item rather than at
    /// construction, which is what lets this return `RowStream` and keep the
    /// call readable.
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, RowStream};
    /// # use moso_sql::Statement;
    /// fn stream<'e>(handle: Handle<'e>, statement: Statement) -> RowStream<'e> {
    ///     handle.fetch_stream(statement)
    /// }
    /// ```
    #[must_use]
    pub fn fetch_stream(&self, statement: Statement) -> RowStream<'e> {
        use futures_util::StreamExt as _;

        if self.in_transaction() {
            let handle = *self;
            return RowStream::new(Box::pin(
                futures_util::stream::once(async move { handle.fetch_all(&statement).await })
                    .flat_map(|batch| {
                        futures_util::stream::iter(match batch {
                            Ok(rows) => rows.into_iter().map(Ok).collect::<Vec<_>>(),
                            Err(error) => vec![Err(error)],
                        })
                    }),
            ));
        }

        let sql = match self
            .guard_read_only(&statement)
            .and_then(|()| self.build(&statement))
        {
            Ok(sql) => sql,
            Err(error) => {
                return RowStream::new(Box::pin(futures_util::stream::once(
                    async move { Err(error) },
                )));
            }
        };

        // One statement, counted when it is issued rather than when it is
        // drained: a stream the caller abandons still cost a round trip.
        self.db.statements().record(Duration::ZERO);
        let db = self.db;
        match self.db.pool_handle() {
            #[cfg(feature = "postgres")]
            crate::db::PoolHandle::Postgres(pool) => RowStream::new(stream_postgres(db, pool, sql)),
            #[cfg(feature = "sqlite")]
            crate::db::PoolHandle::Sqlite(pool) => RowStream::new(stream_sqlite(db, pool, sql)),
        }
    }

    /// Runs already-rendered SQL and collects every row.
    ///
    /// This is where the raw-SQL escape hatch and the built statements meet.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result, Row};
    /// # use moso_sql::Sql;
    /// async fn all(handle: Handle<'_>, sql: Sql) -> Result<Vec<Row>> {
    ///     handle.fetch_all_sql(sql).await
    /// }
    /// ```
    pub async fn fetch_all_sql(&self, sql: Sql) -> Result<Vec<Row>> {
        match self.run(sql, Mode::All).await? {
            Outcome::Rows(rows) => Ok(rows),
            Outcome::Affected(_) => Ok(Vec::new()),
        }
    }

    /// Runs already-rendered SQL and returns the first row, if there is one.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result, Row};
    /// # use moso_sql::Sql;
    /// async fn one(handle: Handle<'_>, sql: Sql) -> Result<Option<Row>> {
    ///     handle.fetch_optional_sql(sql).await
    /// }
    /// ```
    pub async fn fetch_optional_sql(&self, sql: Sql) -> Result<Option<Row>> {
        match self.run(sql, Mode::Optional).await? {
            Outcome::Rows(rows) => Ok(rows.into_iter().next()),
            Outcome::Affected(_) => Ok(None),
        }
    }

    /// Runs already-rendered SQL for its effect.
    ///
    /// # Errors
    ///
    /// Anything in [`crate::Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Handle, Result};
    /// # use moso_sql::Sql;
    /// async fn run(handle: Handle<'_>, sql: Sql) -> Result<u64> {
    ///     handle.execute_sql(sql).await
    /// }
    /// ```
    pub async fn execute_sql(&self, sql: Sql) -> Result<u64> {
        match self.run(sql, Mode::Execute).await? {
            Outcome::Affected(rows) => Ok(rows),
            Outcome::Rows(rows) => Ok(rows.len() as u64),
        }
    }

    /// Records where the caller built the statement, so an error can name the
    /// user's file and line rather than a framework one.
    ///
    /// ```no_run
    /// # use moso_orm::{CallSite, Handle};
    /// fn tag(handle: Handle<'_>) -> Handle<'_> {
    ///     handle.at(CallSite::caller())
    /// }
    /// ```
    #[must_use]
    pub const fn at(self, site: CallSite) -> Self {
        Self {
            db: self.db,
            tx: self.tx,
            site: Some(site),
            entity: self.entity,
        }
    }

    /// Names the entity the statement was built from, so that a constraint
    /// violation says `User` rather than `row`.
    ///
    /// Every builder that knows its entity should call this; a raw statement
    /// does not know one, which is why it is optional rather than a parameter.
    ///
    /// ```no_run
    /// # use moso_orm::Handle;
    /// fn tag(handle: Handle<'_>) -> Handle<'_> {
    ///     handle.for_entity("User")
    /// }
    /// ```
    #[must_use]
    pub const fn for_entity(self, entity: &'static str) -> Self {
        Self {
            db: self.db,
            tx: self.tx,
            site: self.site,
            entity: Some(entity),
        }
    }

    /// Where the caller built this statement, when it said.
    ///
    /// ```no_run
    /// # use moso_orm::{CallSite, Handle};
    /// fn site(handle: Handle<'_>) -> Option<CallSite> {
    ///     handle.call_site()
    /// }
    /// ```
    #[must_use]
    pub const fn call_site(&self) -> Option<CallSite> {
        self.site
    }

    // ── the one place a statement actually runs ───────────────────────────

    /// Renders nothing, binds, runs, counts, traces and translates.
    async fn run(&self, sql: Sql, mode: Mode) -> Result<Outcome> {
        use tracing::Instrument as _;

        let operation = operation_of(&sql.text);

        // The span is opened *before* the await and the future is driven inside
        // it, so the current subscriber parents it under whatever request span
        // is in scope: that is what makes `db.query` nest in a request trace. At
        // `debug` level with no interested subscriber the macro elides the span
        // and nothing is allocated. `db.rows` and `db.duration_ms` are recorded
        // once the statement has run.
        let span = self.query_span(&sql, operation);
        let started = Instant::now();
        let outcome = self.dispatch(&sql, mode).instrument(span.clone()).await;
        let elapsed = started.elapsed();

        self.db.statements().record(elapsed);
        let rows = match &outcome {
            Ok(Outcome::Rows(rows)) => rows.len() as u64,
            Ok(Outcome::Affected(count)) => *count,
            Err(_) => 0,
        };
        span.record("db.rows", rows);
        span.record("db.duration_ms", elapsed.as_millis());
        span.record(
            "otel.status_code",
            if outcome.is_err() { "ERROR" } else { "OK" },
        );

        if let Some(metrics) = self.db.metrics() {
            metrics.query(&QuerySample {
                operation,
                entity: self.entity,
                elapsed,
                rows,
                in_transaction: self.in_transaction(),
                failed: outcome.is_err(),
            });
        }
        self.trace(&sql, operation, elapsed, rows, outcome.is_err());
        self.warn_if_chatty();

        // A write moves the read-your-writes window, so that `db.read()` in the
        // same request goes to the primary. A statement that failed did not
        // write anything, and a `SELECT` never does.
        if outcome.is_ok() && !matches!(operation, "select" | "with" | "control") {
            self.db.mark_write();
        }
        outcome
    }

    /// Runs on the transaction's connection, or on one from the pool.
    async fn dispatch(&self, sql: &Sql, mode: Mode) -> Result<Outcome> {
        let persistent = self.db.persistent();
        match self.tx {
            Some(tx) => {
                let mut guard = tx.guard().await;
                match self.backend() {
                    #[cfg(feature = "postgres")]
                    Backend::Postgres => {
                        let connection = guard.postgres()?;
                        self.finish(run_postgres(connection, sql, mode, persistent).await, sql)
                    }
                    #[cfg(feature = "sqlite")]
                    Backend::Sqlite => {
                        let connection = guard.sqlite()?;
                        self.finish(run_sqlite(connection, sql, mode, persistent).await, sql)
                    }
                    #[allow(
                        unreachable_patterns,
                        reason = "reachable only when a backend feature is off"
                    )]
                    backend => Err(Error::Unsupported {
                        feature: "this backend",
                        backend,
                    }),
                }
            }
            None => {
                let mut connection = self.db.acquire().await?;
                match &mut connection {
                    #[cfg(feature = "postgres")]
                    PooledConnection::Postgres(conn) => {
                        self.finish(run_postgres(conn, sql, mode, persistent).await, sql)
                    }
                    #[cfg(feature = "sqlite")]
                    PooledConnection::Sqlite(conn) => {
                        self.finish(run_sqlite(conn, sql, mode, persistent).await, sql)
                    }
                }
            }
        }
    }

    /// Turns a driver result into a Moso one, attaching the call site.
    fn finish(
        &self,
        outcome: core::result::Result<Outcome, sqlx::Error>,
        sql: &Sql,
    ) -> Result<Outcome> {
        outcome.map_err(|error| {
            let translated = translate_driver_error(
                error,
                &sql.text,
                self.backend(),
                self.db.config(),
                self.entity.unwrap_or(ANONYMOUS_ENTITY),
            );
            match (translated, self.site) {
                (Error::UniqueViolation(violation), Some(site)) => {
                    Error::UniqueViolation(Box::new((*violation).at(site)))
                }
                (Error::ForeignKeyViolation(violation), Some(site)) => {
                    Error::ForeignKeyViolation(Box::new((*violation).at(site)))
                }
                (Error::NotNullViolation(violation), Some(site)) => {
                    Error::NotNullViolation(Box::new((*violation).at(site)))
                }
                (Error::CheckViolation(violation), Some(site)) => {
                    Error::CheckViolation(Box::new((*violation).at(site)))
                }
                (Error::Database(inner), Some(site)) => {
                    Error::Database(Box::new((*inner).at(site)))
                }
                (other, _) => other,
            }
        })
    }

    /// A read-only transaction refuses a write before it is sent.
    ///
    /// PostgreSQL enforces this itself with `BEGIN … READ ONLY`; SQLite has no
    /// equivalent, so the check is Moso's on both backends and the message is
    /// the same either way. Raw SQL is not classified — the crate does not
    /// parse it — so on SQLite a raw write inside a read-only transaction is
    /// the one case that is not caught.
    fn guard_read_only(&self, statement: &Statement) -> Result<()> {
        let Some(tx) = self.tx else { return Ok(()) };
        if !tx.options().read_only || statement.is_read_only() {
            return Ok(());
        }
        Err(Error::Configuration {
            detail: String::from(
                "this transaction was opened read-only, and this statement writes\n  \
                 help: drop `.read_only()` from the `TxOptions`, or move the write out of this \
                 transaction",
            ),
        })
    }

    /// Opens the span the statement's execution is driven inside.
    ///
    /// Created in the execution path so the current subscriber makes it a child
    /// of whatever request span is in scope — that is how `db.query` comes to
    /// nest under a request in a trace. It is a `debug`-level span, so when no
    /// subscriber is interested the macro elides it and nothing is allocated
    /// beyond what `tracing` already elides.
    ///
    /// The recorded statement is the **parameterised** SQL, never the bound
    /// values — the values may be secrets, and the crate's posture is that they
    /// are never logged. `db.rows`, `db.duration_ms` and `otel.status_code` are
    /// filled in by [`Handle::run`] once the statement has completed.
    fn query_span(&self, sql: &Sql, operation: &str) -> tracing::Span {
        tracing::debug_span!(
            "db.query",
            db.system = otel_system(self.backend()),
            db.operation = operation,
            db.statement = sql.text.as_str(),
            db.in_transaction = self.in_transaction(),
            db.rows = tracing::field::Empty,
            db.duration_ms = tracing::field::Empty,
            otel.status_code = tracing::field::Empty,
            code.filepath = self.site.as_ref().map_or("", CallSite::file),
            code.lineno = self.site.as_ref().map_or(0, CallSite::line),
        )
    }

    /// Emits the statement's event, at a level chosen by its outcome.
    ///
    /// The text is the **parameterised** SQL: it is safe to log in production
    /// because the values are not in it, and it is the thing you want when
    /// grouping by statement in a log aggregator.
    fn trace(&self, sql: &Sql, operation: &str, elapsed: Duration, rows: u64, failed: bool) {
        let millis = elapsed.as_millis();
        let file = self.site.as_ref().map_or("", CallSite::file);
        let line = self.site.as_ref().map_or(0, CallSite::line);

        if failed {
            tracing::debug!(
                db.operation = operation,
                db.statement = sql.text.as_str(),
                db.parameters = sql.args.len(),
                db.duration_ms = millis,
                db.in_transaction = self.in_transaction(),
                code.filepath = file,
                code.lineno = line,
                "db: statement failed"
            );
            return;
        }

        let threshold = u128::from(self.db.config().slow_query_ms);
        if threshold > 0 && millis >= threshold {
            tracing::warn!(
                db.operation = operation,
                db.statement = sql.text.as_str(),
                db.parameters = sql.args.len(),
                db.duration_ms = millis,
                db.rows = rows,
                db.in_transaction = self.in_transaction(),
                code.filepath = file,
                code.lineno = line,
                "db: slow statement (over `database.slow_query_ms`)"
            );
            return;
        }

        tracing::trace!(
            db.operation = operation,
            db.statement = sql.text.as_str(),
            db.parameters = sql.args.len(),
            db.duration_ms = millis,
            db.rows = rows,
            db.in_transaction = self.in_transaction(),
            code.filepath = file,
            code.lineno = line,
            "db: statement"
        );
    }

    /// Says something, once per handle, when a handle has issued more
    /// statements than [`DatabaseConfig::n_plus_one_threshold`].
    ///
    /// Once per **handle** rather than once per process: a handle made with
    /// [`Db::request_scoped`](crate::Db::request_scoped) has its own counter,
    /// so on an application that uses one the warning is per request, which is
    /// where an N+1 actually shows up.
    fn warn_if_chatty(&self) {
        let threshold = u64::from(self.db.config().n_plus_one_threshold);
        if threshold == 0 {
            return;
        }
        let total = self.db.statements().total();
        if total != threshold + 1 {
            // Exactly on the crossing, so the warning happens once and the
            // check is a comparison rather than a lock.
            return;
        }
        tracing::warn!(
            statements = total,
            threshold,
            "db: this handle has issued more statements than `database.n_plus_one_threshold`. \
             If this is one request, it is probably an N+1: load the relation with `.with(..)`. \
             Use `db.request_scoped()` per request to scope this warning to one."
        );
    }
}

impl fmt::Debug for Handle<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Handle")
            .field("in_transaction", &self.in_transaction())
            .field("entity", &self.entity)
            .finish_non_exhaustive()
    }
}

/// What a statement is being run for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Every row.
    All,
    /// At most one row.
    Optional,
    /// The affected count.
    Execute,
}

/// What running a statement produced.
enum Outcome {
    /// The rows it returned.
    Rows(Vec<Row>),
    /// The rows it affected.
    Affected(u64),
}

/// The OpenTelemetry `db.system` value for a backend.
///
/// This is the semantic-convention spelling (`postgresql`, `sqlite`), which is
/// deliberately not [`Backend::as_str`]'s display spelling (`PostgreSQL`,
/// `SQLite`): a trace backend keys on the exact convention string, so the span
/// carries it verbatim.
const fn otel_system(backend: Backend) -> &'static str {
    match backend {
        Backend::Postgres => "postgresql",
        Backend::Sqlite => "sqlite",
    }
}

/// The first word of a statement, lowercased, for a metric label.
fn operation_of(text: &str) -> &'static str {
    let first = text
        .trim_start()
        .split(|c: char| c.is_whitespace() || c == '(')
        .next()
        .unwrap_or_default();
    match first.to_ascii_lowercase().as_str() {
        "select" => "select",
        "insert" => "insert",
        "update" => "update",
        "delete" => "delete",
        "with" => "with",
        "create" | "alter" | "drop" | "truncate" | "comment" => "ddl",
        "begin" | "commit" | "rollback" | "savepoint" | "release" | "set" => "control",
        _ => "other",
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL
// ---------------------------------------------------------------------------

/// Runs one statement on a PostgreSQL connection.
#[cfg(feature = "postgres")]
async fn run_postgres(
    connection: &mut sqlx::PgConnection,
    sql: &Sql,
    mode: Mode,
    persistent: bool,
) -> core::result::Result<Outcome, sqlx::Error> {
    use futures_util::TryStreamExt as _;
    use sqlx::Executor as _;

    let mut arguments = sqlx::postgres::PgArguments::default();
    sqlx::Arguments::reserve(&mut arguments, sql.args.len(), sql.args.len() * 8);
    for value in &sql.args {
        bind_postgres(&mut arguments, value).map_err(sqlx::Error::Encode)?;
    }

    let query =
        sqlx::query_with(sqlx::AssertSqlSafe(sql.text.clone()), arguments).persistent(persistent);

    match mode {
        Mode::All => {
            let rows = connection.fetch(query).try_collect::<Vec<_>>().await?;
            Ok(Outcome::Rows(rows.into_iter().map(Row::postgres).collect()))
        }
        Mode::Optional => {
            let row = connection.fetch_optional(query).await?;
            Ok(Outcome::Rows(row.into_iter().map(Row::postgres).collect()))
        }
        Mode::Execute => {
            let result = connection.execute(query).await?;
            Ok(Outcome::Affected(result.rows_affected()))
        }
    }
}

/// The genuine row stream, for a result set that must not be buffered.
///
/// `&PgPool` is the executor rather than a checked-out connection because
/// sqlx's pool stream *owns* the connection it took, which is what lets the
/// stream outlive this function without a self-referential type.
#[cfg(feature = "postgres")]
fn stream_postgres<'e>(
    db: &'e Db,
    pool: &'e sqlx::PgPool,
    sql: Sql,
) -> futures_util::stream::BoxStream<'e, Result<Row>> {
    use futures_util::TryStreamExt as _;
    use sqlx::Executor as _;

    let text = sql.text.clone();
    let mut arguments = sqlx::postgres::PgArguments::default();
    for value in &sql.args {
        if let Err(error) = bind_postgres(&mut arguments, value) {
            let failure = db.translate(sqlx::Error::Encode(error), &text);
            return Box::pin(futures_util::stream::once(async move { Err(failure) }));
        }
    }
    let query =
        sqlx::query_with(sqlx::AssertSqlSafe(sql.text), arguments).persistent(db.persistent());

    Box::pin(
        pool.fetch(query)
            .map_ok(Row::postgres)
            .map_err(move |error| db.translate(error, &text)),
    )
}

/// The genuine row stream, on SQLite. See [`stream_postgres`].
#[cfg(feature = "sqlite")]
fn stream_sqlite<'e>(
    db: &'e Db,
    pool: &'e sqlx::SqlitePool,
    sql: Sql,
) -> futures_util::stream::BoxStream<'e, Result<Row>> {
    use futures_util::TryStreamExt as _;
    use sqlx::Executor as _;

    let text = sql.text.clone();
    let mut arguments = sqlx::sqlite::SqliteArguments::default();
    for value in &sql.args {
        if let Err(error) = bind_sqlite(&mut arguments, value) {
            let failure = db.translate(sqlx::Error::Encode(error), &text);
            return Box::pin(futures_util::stream::once(async move { Err(failure) }));
        }
    }
    let query =
        sqlx::query_with(sqlx::AssertSqlSafe(sql.text), arguments).persistent(db.persistent());

    Box::pin(
        pool.fetch(query)
            .map_ok(Row::sqlite)
            .map_err(move |error| db.translate(error, &text)),
    )
}

/// Binds one Moso value as a PostgreSQL parameter.
///
/// Every value goes through the driver's own encoder for the natural Rust type,
/// which is the same one the decoder reads it back with — so a round trip is
/// correct by construction rather than by two lists agreeing.
#[cfg(feature = "postgres")]
fn bind_postgres(
    arguments: &mut sqlx::postgres::PgArguments,
    value: &Value,
) -> core::result::Result<(), sqlx::error::BoxDynError> {
    use moso_sql::ValueKind;
    use sqlx::Arguments as _;

    match value {
        Value::Null(kind) => match kind {
            ValueKind::Bool => arguments.add(None::<bool>),
            ValueKind::I8 | ValueKind::I16 | ValueKind::U8 => arguments.add(None::<i16>),
            ValueKind::I32 | ValueKind::U16 => arguments.add(None::<i32>),
            ValueKind::I64 | ValueKind::U32 | ValueKind::U64 => arguments.add(None::<i64>),
            ValueKind::F32 => arguments.add(None::<f32>),
            ValueKind::F64 => arguments.add(None::<f64>),
            ValueKind::Decimal => arguments.add(None::<sqlx::types::Decimal>),
            ValueKind::Bytes => arguments.add(None::<Vec<u8>>),
            ValueKind::Uuid => arguments.add(None::<uuid::Uuid>),
            ValueKind::Json => arguments.add(None::<serde_json::Value>),
            ValueKind::Timestamp => arguments.add(None::<chrono::DateTime<chrono::Utc>>),
            ValueKind::DateTime => arguments.add(None::<chrono::NaiveDateTime>),
            ValueKind::Date => arguments.add(None::<chrono::NaiveDate>),
            ValueKind::Time => arguments.add(None::<chrono::NaiveTime>),
            ValueKind::Interval => arguments.add(None::<sqlx::postgres::types::PgInterval>),
            // An array of unknown element type, and an untyped NULL, are both
            // sent as text: PostgreSQL will cast a text NULL to whatever the
            // column is, and the alternative is refusing a statement that would
            // have worked.
            ValueKind::Text | ValueKind::Array | ValueKind::Unknown => {
                arguments.add(None::<String>)
            }
            _ => arguments.add(None::<String>),
        },
        Value::Bool(inner) => arguments.add(*inner),
        // PostgreSQL has no one-byte integer, and no unsigned types at all, so
        // each one widens to the smallest signed type that holds it.
        Value::I8(inner) => arguments.add(i16::from(*inner)),
        Value::I16(inner) => arguments.add(*inner),
        Value::I32(inner) => arguments.add(*inner),
        Value::I64(inner) => arguments.add(*inner),
        Value::U8(inner) => arguments.add(i16::from(*inner)),
        Value::U16(inner) => arguments.add(i32::from(*inner)),
        Value::U32(inner) => arguments.add(i64::from(*inner)),
        Value::U64(inner) => arguments.add(unsigned_64(*inner)?),
        Value::F32(inner) => arguments.add(*inner),
        Value::F64(inner) => arguments.add(*inner),
        Value::Decimal(inner) => arguments.add(decimal(*inner)?),
        Value::Text(inner) => arguments.add(inner.clone()),
        Value::Bytes(inner) => arguments.add(inner.clone()),
        Value::Uuid(inner) => arguments.add(uuid::Uuid::from_bytes(inner.into_bytes())),
        Value::Json(inner) => arguments.add(json(inner)?),
        Value::Timestamp(inner) => arguments.add(timestamp(*inner)?),
        Value::DateTime(inner) => arguments.add(naive_datetime(*inner)?),
        Value::Date(inner) => arguments.add(date(*inner)?),
        Value::Time(inner) => arguments.add(time(*inner)?),
        Value::Interval(inner) => arguments.add(sqlx::postgres::types::PgInterval {
            months: inner.months(),
            days: inner.day_component(),
            microseconds: inner.microseconds(),
        }),
        Value::Array(inner) => bind_postgres_array(arguments, inner),
        other => Err(format!(
            "`{other:?}` is a value this build of moso-orm cannot bind for PostgreSQL"
        )
        .into()),
    }
}

/// Binds a one-dimensional PostgreSQL array.
#[cfg(feature = "postgres")]
fn bind_postgres_array(
    arguments: &mut sqlx::postgres::PgArguments,
    array: &moso_sql::Array,
) -> core::result::Result<(), sqlx::error::BoxDynError> {
    use moso_sql::ValueKind;
    use sqlx::Arguments as _;

    /// Collects an array's items through one extractor, refusing a mixed array
    /// rather than silently dropping the odd one out.
    fn collect<T>(
        items: &[Value],
        kind: ValueKind,
        extract: impl Fn(&Value) -> Option<T>,
    ) -> core::result::Result<Vec<T>, sqlx::error::BoxDynError> {
        items
            .iter()
            .map(|item| {
                extract(item).ok_or_else(|| -> sqlx::error::BoxDynError {
                    format!(
                        "an array declared as `{kind:?}` contains a `{:?}`; every element must \
                         have the array's element type",
                        item.kind()
                    )
                    .into()
                })
            })
            .collect()
    }

    let items = array.items();
    match array.element_kind() {
        ValueKind::Bool => {
            arguments.add(collect(items, ValueKind::Bool, |value| match value {
                Value::Bool(inner) => Some(*inner),
                _ => None,
            })?)?
        }
        ValueKind::I16 | ValueKind::I8 | ValueKind::U8 => {
            arguments.add(collect(items, ValueKind::I16, |value| match value {
                Value::I16(inner) => Some(*inner),
                Value::I8(inner) => Some(i16::from(*inner)),
                Value::U8(inner) => Some(i16::from(*inner)),
                _ => None,
            })?)?;
        }
        ValueKind::I32 | ValueKind::U16 => {
            arguments.add(collect(items, ValueKind::I32, |value| match value {
                Value::I32(inner) => Some(*inner),
                Value::U16(inner) => Some(i32::from(*inner)),
                _ => None,
            })?)?;
        }
        ValueKind::I64 | ValueKind::U32 => {
            arguments.add(collect(items, ValueKind::I64, |value| match value {
                Value::I64(inner) => Some(*inner),
                Value::U32(inner) => Some(i64::from(*inner)),
                _ => None,
            })?)?;
        }
        ValueKind::F32 => arguments.add(collect(items, ValueKind::F32, |value| match value {
            Value::F32(inner) => Some(*inner),
            _ => None,
        })?)?,
        ValueKind::F64 => arguments.add(collect(items, ValueKind::F64, |value| match value {
            Value::F64(inner) => Some(*inner),
            _ => None,
        })?)?,
        ValueKind::Text => {
            arguments.add(collect(items, ValueKind::Text, |value| match value {
                Value::Text(inner) => Some(inner.clone()),
                _ => None,
            })?)?
        }
        ValueKind::Uuid => {
            arguments.add(collect(items, ValueKind::Uuid, |value| match value {
                Value::Uuid(inner) => Some(uuid::Uuid::from_bytes(inner.into_bytes())),
                _ => None,
            })?)?
        }
        other => {
            return Err(format!(
                "PostgreSQL arrays of `{other:?}` are not bindable yet\n  \
                 help: arrays of bool, the integer types, float, text and uuid are; for anything \
                 else, store the list as `jsonb`"
            )
            .into());
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SQLite
// ---------------------------------------------------------------------------

/// Runs one statement on a SQLite connection.
#[cfg(feature = "sqlite")]
async fn run_sqlite(
    connection: &mut sqlx::SqliteConnection,
    sql: &Sql,
    mode: Mode,
    persistent: bool,
) -> core::result::Result<Outcome, sqlx::Error> {
    use futures_util::TryStreamExt as _;
    use sqlx::Executor as _;

    let mut arguments = sqlx::sqlite::SqliteArguments::default();
    sqlx::Arguments::reserve(&mut arguments, sql.args.len(), sql.args.len() * 8);
    for value in &sql.args {
        bind_sqlite(&mut arguments, value).map_err(sqlx::Error::Encode)?;
    }

    let query =
        sqlx::query_with(sqlx::AssertSqlSafe(sql.text.clone()), arguments).persistent(persistent);

    match mode {
        Mode::All => {
            let rows = connection.fetch(query).try_collect::<Vec<_>>().await?;
            Ok(Outcome::Rows(rows.into_iter().map(Row::sqlite).collect()))
        }
        Mode::Optional => {
            let row = connection.fetch_optional(query).await?;
            Ok(Outcome::Rows(row.into_iter().map(Row::sqlite).collect()))
        }
        Mode::Execute => {
            let result = connection.execute(query).await?;
            Ok(Outcome::Affected(result.rows_affected()))
        }
    }
}

/// Binds one Moso value as a SQLite parameter.
///
/// SQLite has four storage classes, so several types are stored in a
/// representation that is documented rather than native:
///
/// | Moso value | SQLite storage |
/// | --- | --- |
/// | `Uuid` | `BLOB`, sixteen bytes — the driver's own encoding |
/// | `Decimal` | `TEXT`, the plain decimal spelling, so it does not lose digits |
/// | `Json` | `TEXT`, the compact form |
/// | `Interval`, `Array` | not supported; the error says so |
#[cfg(feature = "sqlite")]
fn bind_sqlite(
    arguments: &mut sqlx::sqlite::SqliteArguments,
    value: &Value,
) -> core::result::Result<(), sqlx::error::BoxDynError> {
    use moso_sql::ValueKind;
    use sqlx::Arguments as _;

    match value {
        Value::Null(kind) => match kind {
            ValueKind::Bool => arguments.add(None::<bool>),
            ValueKind::F32 => arguments.add(None::<f32>),
            ValueKind::F64 => arguments.add(None::<f64>),
            ValueKind::Bytes => arguments.add(None::<Vec<u8>>),
            ValueKind::Uuid => arguments.add(None::<uuid::Uuid>),
            ValueKind::Timestamp => arguments.add(None::<chrono::DateTime<chrono::Utc>>),
            ValueKind::DateTime => arguments.add(None::<chrono::NaiveDateTime>),
            ValueKind::Date => arguments.add(None::<chrono::NaiveDate>),
            ValueKind::Time => arguments.add(None::<chrono::NaiveTime>),
            ValueKind::I8 | ValueKind::I16 | ValueKind::I32 | ValueKind::I64 => {
                arguments.add(None::<i64>)
            }
            ValueKind::U8 | ValueKind::U16 | ValueKind::U32 | ValueKind::U64 => {
                arguments.add(None::<i64>)
            }
            _ => arguments.add(None::<String>),
        },
        Value::Bool(inner) => arguments.add(*inner),
        Value::I8(inner) => arguments.add(i64::from(*inner)),
        Value::I16(inner) => arguments.add(i64::from(*inner)),
        Value::I32(inner) => arguments.add(i64::from(*inner)),
        Value::I64(inner) => arguments.add(*inner),
        Value::U8(inner) => arguments.add(i64::from(*inner)),
        Value::U16(inner) => arguments.add(i64::from(*inner)),
        Value::U32(inner) => arguments.add(i64::from(*inner)),
        Value::U64(inner) => arguments.add(unsigned_64(*inner)?),
        Value::F32(inner) => arguments.add(f64::from(*inner)),
        Value::F64(inner) => arguments.add(*inner),
        // SQLite has no exact numeric type. Text keeps every digit, which
        // `REAL` would not, and sorts correctly for equal scales.
        Value::Decimal(inner) => arguments.add(inner.to_string()),
        Value::Text(inner) => arguments.add(inner.clone()),
        Value::Bytes(inner) => arguments.add(inner.clone()),
        Value::Uuid(inner) => arguments.add(uuid::Uuid::from_bytes(inner.into_bytes())),
        Value::Json(inner) => arguments.add(inner.as_json_str().to_owned()),
        Value::Timestamp(inner) => arguments.add(timestamp(*inner)?),
        Value::DateTime(inner) => arguments.add(naive_datetime(*inner)?),
        Value::Date(inner) => arguments.add(date(*inner)?),
        Value::Time(inner) => arguments.add(time(*inner)?),
        Value::Interval(_) => Err(sqlite_cannot(
            "an interval",
            "store it as seconds, or as text",
        )),
        Value::Array(_) => Err(sqlite_cannot(
            "an array",
            "store the list as JSON, or as a join table",
        )),
        other => Err(format!(
            "`{other:?}` is a value this build of moso-orm cannot bind for SQLite"
        )
        .into()),
    }
}

/// The message for a value SQLite has no storage class for.
#[cfg(feature = "sqlite")]
fn sqlite_cannot(what: &str, help: &str) -> sqlx::error::BoxDynError {
    format!(
        "SQLite has no column type for {what}\n  \
         help: {help}\n  \
         note: the divergence table is in `docs/02-data/20-orm-overview.md`"
    )
    .into()
}

// ---------------------------------------------------------------------------
// Value conversions shared by both drivers
// ---------------------------------------------------------------------------

/// A `u64` as the signed integer both servers actually store.
fn unsigned_64(value: u64) -> core::result::Result<i64, sqlx::error::BoxDynError> {
    i64::try_from(value).map_err(|_| -> sqlx::error::BoxDynError {
        format!(
            "{value} is above `i64::MAX`, and neither PostgreSQL nor SQLite has an unsigned \
             64-bit integer\n  \
             help: store it as `numeric`/`TEXT`, or use `i64`"
        )
        .into()
    })
}

/// Moso's exact decimal as the driver's.
///
/// PostgreSQL only: SQLite has no exact numeric type, so a decimal is stored as
/// text there — see [`bind_sqlite`].
#[cfg(feature = "postgres")]
fn decimal(
    value: moso_sql::Decimal,
) -> core::result::Result<sqlx::types::Decimal, sqlx::error::BoxDynError> {
    sqlx::types::Decimal::try_from_i128_with_scale(value.mantissa(), value.scale()).map_err(
        |error| -> sqlx::error::BoxDynError {
            format!(
                "`{value}` does not fit the driver's 96-bit decimal: {error}\n  \
                 help: reduce the scale, or store the number as text"
            )
            .into()
        },
    )
}

/// Validated JSON text as the driver's JSON value.
///
/// PostgreSQL only: SQLite stores the compact text as it is.
#[cfg(feature = "postgres")]
fn json(
    value: &moso_sql::Json,
) -> core::result::Result<serde_json::Value, sqlx::error::BoxDynError> {
    serde_json::from_str(value.as_json_str()).map_err(|error| -> sqlx::error::BoxDynError {
        format!("the JSON parameter is not parseable: {error}").into()
    })
}

/// An instant as `chrono`'s.
fn timestamp(
    value: moso_sql::Timestamp,
) -> core::result::Result<chrono::DateTime<chrono::Utc>, sqlx::error::BoxDynError> {
    chrono::DateTime::from_timestamp(value.unix_seconds(), value.nanoseconds()).ok_or_else(
        || -> sqlx::error::BoxDynError {
            format!(
                "{}s + {}ns is not a representable instant",
                value.unix_seconds(),
                value.nanoseconds()
            )
            .into()
        },
    )
}

/// A calendar date as `chrono`'s.
fn date(
    value: moso_sql::Date,
) -> core::result::Result<chrono::NaiveDate, sqlx::error::BoxDynError> {
    chrono::NaiveDate::from_ymd_opt(
        value.year(),
        u32::from(value.month()),
        u32::from(value.day()),
    )
    .ok_or_else(|| -> sqlx::error::BoxDynError { format!("`{value}` is not a date").into() })
}

/// A wall-clock time as `chrono`'s.
fn time(
    value: moso_sql::Time,
) -> core::result::Result<chrono::NaiveTime, sqlx::error::BoxDynError> {
    chrono::NaiveTime::from_hms_nano_opt(
        u32::from(value.hour()),
        u32::from(value.minute()),
        u32::from(value.second()),
        value.nanosecond(),
    )
    .ok_or_else(|| -> sqlx::error::BoxDynError { format!("`{value}` is not a time").into() })
}

/// A zoneless date and time as `chrono`'s.
fn naive_datetime(
    value: moso_sql::DateTime,
) -> core::result::Result<chrono::NaiveDateTime, sqlx::error::BoxDynError> {
    Ok(chrono::NaiveDateTime::new(
        date(value.date())?,
        time(value.time())?,
    ))
}

// ---------------------------------------------------------------------------
// Error translation
// ---------------------------------------------------------------------------

/// Turns a driver error into one that names the problem (non-negotiable N7).
///
/// This is where a `23505` stops being "error returned from database" and
/// becomes a [`Error::UniqueViolation`] carrying the column a client can fix.
pub(crate) fn translate_driver_error(
    error: sqlx::Error,
    sql: &str,
    backend: Backend,
    config: &DatabaseConfig,
    entity: &'static str,
) -> Error {
    match error {
        sqlx::Error::PoolTimedOut => Error::PoolTimeout {
            waited: config.acquire_timeout,
            size: config.max_connections,
        },
        sqlx::Error::PoolClosed => Error::Connection {
            detail: String::from(
                "the connection pool is closed\n  \
                 help: this is a statement issued after `Db::close`, usually during shutdown",
            ),
        },
        sqlx::Error::Io(inner) => Error::Connection {
            detail: inner.to_string(),
        },
        sqlx::Error::Tls(inner) => Error::Connection {
            detail: format!(
                "the TLS handshake failed: {inner}\n  \
                 help: check `database.tls`; `verify-full` needs the server's CA to be trusted"
            ),
        },
        sqlx::Error::Configuration(inner) => Error::Configuration {
            detail: inner.to_string(),
        },
        sqlx::Error::Protocol(detail) => Error::Connection { detail },
        sqlx::Error::RowNotFound => Error::NotFound { entity },
        sqlx::Error::Database(inner) => {
            translate_database_error(inner.as_ref(), sql, backend, config, entity)
        }
        other => Error::Database(Box::new(
            DatabaseError::new("", other.to_string()).with_sql(sql.to_owned()),
        )),
    }
}

/// The half of the translation that reads a server's own error report.
fn translate_database_error(
    error: &dyn sqlx::error::DatabaseError,
    sql: &str,
    backend: Backend,
    config: &DatabaseConfig,
    entity: &'static str,
) -> Error {
    let sqlstate = error
        .code()
        .map(|code| code.to_string())
        .unwrap_or_default();
    // SQLite's driver reports no constraint name at all, so the message is the
    // only place one can be found; falling back to the SQLSTATE keeps the field
    // populated with something that at least identifies the failure.
    let constraint = error
        .constraint()
        .or_else(|| crate::insert::upsert::constraint_name(error.message()))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| sqlstate.clone());
    let table = error.table().unwrap_or_default().to_owned();

    let kind = match error.kind() {
        sqlx::error::ErrorKind::UniqueViolation => Some(ConstraintKind::Unique),
        sqlx::error::ErrorKind::ForeignKeyViolation => Some(ConstraintKind::ForeignKey),
        sqlx::error::ErrorKind::NotNullViolation => Some(ConstraintKind::NotNull),
        sqlx::error::ErrorKind::CheckViolation => Some(ConstraintKind::Check),
        sqlx::error::ErrorKind::ExclusionViolation => Some(ConstraintKind::Exclusion),
        _ => None,
    };

    if let Some(kind) = kind {
        let mut violation = ConstraintViolation::new(entity, constraint.clone(), kind)
            .with_message(kind.default_message())
            .with_sqlstate(sqlstate.clone())
            .with_sql(sql.to_owned());
        for column in columns_of(error, &constraint, &table, backend) {
            violation = violation.with_column(column);
        }
        return match kind {
            ConstraintKind::Unique => Error::UniqueViolation(Box::new(violation)),
            ConstraintKind::ForeignKey => Error::ForeignKeyViolation(Box::new(violation)),
            ConstraintKind::NotNull => Error::NotNullViolation(Box::new(violation)),
            _ => Error::CheckViolation(Box::new(violation)),
        };
    }

    match sqlstate.as_str() {
        // PostgreSQL's two retryable failures, and the reason `db.transaction`
        // takes a closure.
        "40001" => Error::Serialization { code: sqlstate },
        "40P01" => Error::Deadlock { code: sqlstate },
        // `statement_timeout` fired. `55P03` — `lock_not_available`, which is
        // what `lock_timeout` produces — is deliberately *not* folded in here:
        // waiting for a lock and running too long are different problems with
        // different fixes, and it keeps its own message below.
        "57014" => Error::StatementTimeout {
            after: config.statement_timeout,
        },
        _ => {
            // SQLite reports busy and locked as its own numeric codes rather
            // than a SQLSTATE. Both mean "another writer has it": retryable,
            // which is exactly what `Error::Serialization` makes them.
            if backend == Backend::Sqlite && matches!(sqlstate.as_str(), "5" | "6" | "261" | "517")
            {
                return Error::Serialization { code: sqlstate };
            }
            let mut database = DatabaseError::new(sqlstate.clone(), error.message().to_owned())
                .with_sql(sql.to_owned());
            if sqlstate == "55P03" {
                database = database.with_hint(format!(
                    "another transaction holds the lock and `lock_timeout` \
                     ({}ms) expired; raise `database.lock_timeout`, or take the rows in a \
                     consistent order to stop the two transactions from queueing on each other",
                    config.lock_timeout.as_millis()
                ));
            }
            Error::Database(Box::new(database))
        }
    }
}

/// The columns a constraint covers, as far as the server said.
///
/// PostgreSQL reports the column directly for a `NOT NULL` violation and only
/// the constraint's *name* for the others, so the name is unpicked: the
/// conventional `<table>_<column>_key` / `_fkey` / `_pkey` spelling is what
/// `CREATE TABLE` generates, and recovering `email` from `users_email_key` is
/// what turns a `409` into a `409` with `"pointer": "/email"`.
fn columns_of(
    error: &dyn sqlx::error::DatabaseError,
    constraint: &str,
    table: &str,
    backend: Backend,
) -> Vec<String> {
    #[cfg(feature = "postgres")]
    let detail = if backend == Backend::Postgres {
        match error.try_downcast_ref::<sqlx::postgres::PgDatabaseError>() {
            Some(inner) => {
                if let Some(column) = inner.column() {
                    return vec![column.to_owned()];
                }
                inner.detail()
            }
            None => None,
        }
    } else {
        None
    };
    #[cfg(not(feature = "postgres"))]
    let detail: Option<&str> = None;

    // The server's own words, which beat every heuristic: PostgreSQL's
    // `DETAIL: Key (email)=(…)`, and SQLite's `UNIQUE constraint failed:
    // users.email` — the only place SQLite ever names the column, since its
    // driver reports no constraint and no table.
    let reported = crate::insert::upsert::reported_columns(error.message(), detail);
    if !reported.is_empty() {
        return reported;
    }

    let _ = backend;
    columns_from_constraint_name(constraint, table)
}

/// `users_email_key` over table `users` → `["email"]`.
///
/// A name that does not follow the convention yields nothing rather than a
/// guess: a wrong JSON Pointer is worse than none, because a client will act on
/// it.
fn columns_from_constraint_name(constraint: &str, table: &str) -> Vec<String> {
    const SUFFIXES: [&str; 5] = ["_key", "_fkey", "_pkey", "_check", "_excl"];

    let Some(stripped) = SUFFIXES
        .iter()
        .find_map(|suffix| constraint.strip_suffix(suffix))
    else {
        return Vec::new();
    };
    let middle = if table.is_empty() {
        stripped
    } else {
        match stripped.strip_prefix(&format!("{table}_")) {
            Some(rest) => rest,
            // The prefix is not this table's name, so the convention does not
            // hold and the rest of the name is not a column list.
            None => return Vec::new(),
        }
    };
    if middle.is_empty() {
        return Vec::new();
    }
    // A composite unique index is `users_tenant_id_email_key`, which is
    // ambiguous without the catalogue: `tenant_id_email` could be one column or
    // two. The whole middle is reported as one name, which is right for the
    // common single-column case and honest for the other.
    vec![middle.to_owned()]
}

// ---------------------------------------------------------------------------
// RowStream
// ---------------------------------------------------------------------------

/// A stream of rows, for a result set too large to buffer.
///
/// Implements `futures_core::Stream`, so `StreamExt` works on it; it is a
/// concrete type rather than an `impl Stream` because ADR-0005's seal forbids a
/// foreign trait in a return position.
///
/// ```no_run
/// # use moso_orm::RowStream;
/// fn is_send(stream: RowStream<'_>) -> impl Send + '_ {
///     stream
/// }
/// ```
pub struct RowStream<'e> {
    /// The driver stream, boxed so that the two backends have one type.
    inner: futures_util::stream::BoxStream<'e, Result<Row>>,
}

impl<'e> RowStream<'e> {
    /// Wraps a boxed driver stream. Crate-internal: the only producer is
    /// [`Handle::fetch_stream`].
    pub(crate) const fn new(inner: futures_util::stream::BoxStream<'e, Result<Row>>) -> Self {
        Self { inner }
    }

    /// Reads the whole stream into a vector.
    ///
    /// Defeats the purpose, and exists because a caller that discovers the
    /// result set is small should not have to write the fold.
    ///
    /// # Errors
    ///
    /// The first error the stream yields.
    ///
    /// ```no_run
    /// # use moso_orm::{Result, Row, RowStream};
    /// async fn drain(stream: RowStream<'_>) -> Result<Vec<Row>> {
    ///     stream.collect().await
    /// }
    /// ```
    pub async fn collect(self) -> Result<Vec<Row>> {
        use futures_util::TryStreamExt as _;
        self.inner.try_collect().await
    }
}

impl futures_util::Stream for RowStream<'_> {
    type Item = Result<Row>;

    fn poll_next(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Self::Item>> {
        core::pin::Pin::new(&mut self.inner).poll_next(cx)
    }
}

impl fmt::Debug for RowStream<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RowStream").finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// StatementCounter
// ---------------------------------------------------------------------------

/// How many statements have run, and how long they took.
///
/// Always on. One relaxed increment per statement pays for `assert_queries!`,
/// the N+1 detector and the `moso_db_statements_per_request` histogram.
///
/// ```
/// use moso_orm::StatementCounter;
///
/// let counter = StatementCounter::new();
/// assert_eq!(counter.total(), 0);
///
/// counter.record(core::time::Duration::from_millis(3));
/// counter.record(core::time::Duration::from_millis(7));
/// assert_eq!(counter.total(), 2);
/// assert_eq!(counter.elapsed(), core::time::Duration::from_millis(10));
/// ```
#[derive(Debug, Default)]
pub struct StatementCounter {
    total: AtomicU64,
    micros: AtomicU64,
}

impl StatementCounter {
    /// A counter at zero.
    ///
    /// ```
    /// assert_eq!(moso_orm::StatementCounter::new().total(), 0);
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            total: AtomicU64::new(0),
            micros: AtomicU64::new(0),
        }
    }

    /// Records one statement and how long it took.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::StatementCounter;
    ///
    /// let counter = StatementCounter::new();
    /// counter.record(Duration::from_micros(250));
    /// assert_eq!(counter.total(), 1);
    /// ```
    pub fn record(&self, elapsed: Duration) {
        self.total.fetch_add(1, Ordering::Relaxed);
        let micros = u64::try_from(elapsed.as_micros()).unwrap_or(u64::MAX);
        self.micros.fetch_add(micros, Ordering::Relaxed);
    }

    /// How many statements have run.
    ///
    /// ```
    /// assert_eq!(moso_orm::StatementCounter::new().total(), 0);
    /// ```
    #[must_use]
    pub fn total(&self) -> u64 {
        self.total.load(Ordering::Relaxed)
    }

    /// How long they took in total.
    ///
    /// ```
    /// assert_eq!(moso_orm::StatementCounter::new().elapsed(), core::time::Duration::ZERO);
    /// ```
    #[must_use]
    pub fn elapsed(&self) -> Duration {
        Duration::from_micros(self.micros.load(Ordering::Relaxed))
    }

    /// A snapshot to compare a later reading against.
    ///
    /// This is what `assert_queries!` uses: take a mark, run the block, and
    /// ask how many statements happened between.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::StatementCounter;
    ///
    /// let counter = StatementCounter::new();
    /// let mark = counter.mark();
    /// counter.record(Duration::from_millis(1));
    /// counter.record(Duration::from_millis(1));
    /// assert_eq!(counter.since(mark), 2);
    /// ```
    #[must_use]
    pub fn mark(&self) -> StatementMark {
        StatementMark(self.total())
    }

    /// How many statements have run since `mark`.
    ///
    /// ```
    /// use moso_orm::StatementCounter;
    ///
    /// let counter = StatementCounter::new();
    /// assert_eq!(counter.since(counter.mark()), 0);
    /// ```
    #[must_use]
    pub fn since(&self, mark: StatementMark) -> u64 {
        self.total().saturating_sub(mark.0)
    }

    /// Resets both readings.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::StatementCounter;
    ///
    /// let counter = StatementCounter::new();
    /// counter.record(Duration::from_millis(1));
    /// counter.reset();
    /// assert_eq!(counter.total(), 0);
    /// ```
    pub fn reset(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.micros.store(0, Ordering::Relaxed);
    }
}

/// A reading of a [`StatementCounter`], for comparing against a later one.
///
/// ```
/// use moso_orm::StatementCounter;
///
/// let counter = StatementCounter::new();
/// let mark = counter.mark();
/// assert_eq!(counter.since(mark), 0);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StatementMark(u64);

impl StatementMark {
    /// The count this mark was taken at.
    ///
    /// ```
    /// use moso_orm::StatementCounter;
    ///
    /// assert_eq!(StatementCounter::new().mark().value(), 0);
    /// ```
    #[must_use]
    pub const fn value(self) -> u64 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_counts_and_sums() {
        let counter = StatementCounter::new();
        counter.record(Duration::from_millis(2));
        counter.record(Duration::from_millis(3));
        assert_eq!(counter.total(), 2);
        assert_eq!(counter.elapsed(), Duration::from_millis(5));
    }

    #[test]
    fn a_mark_measures_a_block_which_is_what_assert_queries_needs() {
        let counter = StatementCounter::new();
        counter.record(Duration::from_millis(1));

        let mark = counter.mark();
        counter.record(Duration::from_millis(1));
        counter.record(Duration::from_millis(1));

        assert_eq!(
            counter.since(mark),
            2,
            "the mark ignores earlier statements"
        );
        assert_eq!(counter.total(), 3);
    }

    #[test]
    fn a_reset_clears_both_readings() {
        let counter = StatementCounter::new();
        counter.record(Duration::from_millis(9));
        counter.reset();
        assert_eq!(counter.total(), 0);
        assert_eq!(counter.elapsed(), Duration::ZERO);
    }

    #[test]
    fn the_counter_is_shareable_across_tasks() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<StatementCounter>();
    }

    #[test]
    fn the_operation_label_is_bounded_so_a_metric_cannot_explode() {
        assert_eq!(operation_of("select 1"), "select");
        assert_eq!(operation_of("  SELECT * from t"), "select");
        assert_eq!(operation_of("insert into t values (1)"), "insert");
        assert_eq!(operation_of("with x as (select 1) select * from x"), "with");
        assert_eq!(operation_of("create table t (id int)"), "ddl");
        assert_eq!(operation_of("savepoint s1"), "control");
        assert_eq!(operation_of("vacuum"), "other");
        assert_eq!(operation_of(""), "other");
    }

    #[test]
    fn a_constraint_name_gives_up_the_column_when_it_follows_the_convention() {
        assert_eq!(
            columns_from_constraint_name("users_email_key", "users"),
            ["email"]
        );
        assert_eq!(
            columns_from_constraint_name("posts_author_id_fkey", "posts"),
            ["author_id"]
        );
        assert!(
            columns_from_constraint_name("users_pkey", "users").is_empty(),
            "`users_pkey` names no column, so there is no pointer to give"
        );
    }

    #[test]
    fn a_constraint_name_that_breaks_the_convention_yields_no_pointer() {
        // A wrong pointer is worse than none: a client will act on it.
        assert!(columns_from_constraint_name("uq_user_email", "users").is_empty());
        assert!(columns_from_constraint_name("users_email_key", "accounts").is_empty());
        assert!(columns_from_constraint_name("no_suffix_here", "users").is_empty());
    }

    #[test]
    fn a_constraint_with_no_table_still_gives_the_middle() {
        assert_eq!(columns_from_constraint_name("email_key", ""), ["email"]);
    }
}

#[cfg(test)]
mod real_database {
    use super::*;
    use crate::db::test_support::{postgres, sqlite, unique_table};
    use crate::db::{DatabaseConfig, DbMetrics};
    use moso_sql::{Array, Date, Decimal, Interval, Json, Time, Timestamp, Uuid, ValueKind};
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;

    /// Runs SQL, returning the affected count.
    async fn run(db: &Db, text: &str, args: Vec<Value>) -> Result<u64> {
        db.handle()
            .execute_sql(Sql::new(text.to_owned(), args))
            .await
    }

    /// Proves one value survives the round trip: it is inserted through the
    /// binder, then matched with `=` against the same binding. A `delete` that
    /// reports one row means the value the server stored is the value the
    /// binder sent.
    async fn round_trips(db: &Db, column_type: &str, value: Value) {
        let table = unique_table("moso_bind");
        let (p1, p2) = match db.backend() {
            Backend::Postgres => ("$1", "$1"),
            Backend::Sqlite => ("?", "?"),
        };

        run(
            db,
            &format!("create table {table} (v {column_type})"),
            vec![],
        )
        .await
        .unwrap_or_else(|error| panic!("create {column_type}: {error}"));
        run(
            db,
            &format!("insert into {table} (v) values ({p1})"),
            vec![value.clone()],
        )
        .await
        .unwrap_or_else(|error| panic!("insert {value:?} into {column_type}: {error}"));

        let predicate = if value.is_null() {
            String::from("v is null")
        } else {
            format!("v = {p2}")
        };
        let args = if value.is_null() {
            vec![]
        } else {
            vec![value.clone()]
        };
        let matched = run(db, &format!("delete from {table} where {predicate}"), args)
            .await
            .unwrap_or_else(|error| panic!("match {value:?}: {error}"));

        assert_eq!(
            matched, 1,
            "`{value:?}` did not come back out of a `{column_type}` column as it went in"
        );
        run(db, &format!("drop table {table}"), vec![])
            .await
            .expect("drop");
    }

    #[tokio::test]
    async fn every_postgres_parameter_type_round_trips() {
        let Some(db) = postgres().await else { return };

        round_trips(&db, "boolean", Value::Bool(true)).await;
        round_trips(&db, "smallint", Value::I8(-7)).await;
        round_trips(&db, "smallint", Value::I16(-30_000)).await;
        round_trips(&db, "integer", Value::I32(i32::MIN)).await;
        round_trips(&db, "bigint", Value::I64(i64::MAX)).await;
        round_trips(&db, "smallint", Value::U8(255)).await;
        round_trips(&db, "integer", Value::U16(65_535)).await;
        round_trips(&db, "bigint", Value::U32(u32::MAX)).await;
        round_trips(&db, "bigint", Value::U64(1 << 62)).await;
        round_trips(&db, "real", Value::F32(1.5)).await;
        round_trips(&db, "double precision", Value::F64(-0.125)).await;
        round_trips(
            &db,
            "numeric",
            Value::Decimal(Decimal::new(-1999, 2).expect("in range")),
        )
        .await;
        round_trips(&db, "text", Value::text("a 'quoted' ; string")).await;
        round_trips(&db, "bytea", Value::bytes([0_u8, 1, 254, 255])).await;
        round_trips(&db, "uuid", Value::Uuid(Uuid::from_u128(0x1234_5678))).await;
        round_trips(
            &db,
            "timestamptz",
            Value::Timestamp(Timestamp::new(1_700_000_000, 123_456_000).expect("valid")),
        )
        .await;
        round_trips(
            &db,
            "timestamp",
            Value::DateTime(moso_sql::DateTime::new(
                Date::new(2024, 2, 29).expect("a leap day"),
                Time::new(23, 59, 58, 0).expect("valid"),
            )),
        )
        .await;
        round_trips(
            &db,
            "date",
            Value::Date(Date::new(1999, 12, 31).expect("valid")),
        )
        .await;
        round_trips(
            &db,
            "time",
            Value::Time(Time::new(1, 2, 3, 0).expect("valid")),
        )
        .await;
        round_trips(
            &db,
            "interval",
            Value::Interval(Interval::new(14, 3, 4_500_000)),
        )
        .await;
        round_trips(
            &db,
            "jsonb",
            Value::Json(Json::parse(r#"{"a":[1,2,3],"b":null}"#).expect("valid JSON")),
        )
        .await;
        round_trips(&db, "bigint[]", Value::Array(Array::of([1_i64, 2, 3]))).await;
        round_trips(&db, "text[]", Value::Array(Array::of([String::from("a")]))).await;

        db.close().await;
    }

    #[tokio::test]
    async fn a_typed_null_reaches_the_right_column_type() {
        let Some(db) = postgres().await else { return };
        // A `NULL` that forgot its type is the classic "could not determine
        // data type of parameter" failure, which is why `Value::Null` carries
        // one and why the binder honours it.
        for (column, kind) in [
            ("boolean", ValueKind::Bool),
            ("bigint", ValueKind::I64),
            ("text", ValueKind::Text),
            ("uuid", ValueKind::Uuid),
            ("numeric", ValueKind::Decimal),
            ("timestamptz", ValueKind::Timestamp),
            ("jsonb", ValueKind::Json),
        ] {
            round_trips(&db, column, Value::null(kind)).await;
        }
        db.close().await;
    }

    #[tokio::test]
    async fn every_sqlite_parameter_type_round_trips() {
        let db = sqlite().await;

        round_trips(&db, "boolean", Value::Bool(false)).await;
        round_trips(&db, "integer", Value::I64(i64::MIN)).await;
        round_trips(&db, "integer", Value::I32(42)).await;
        round_trips(&db, "real", Value::F64(2.5)).await;
        round_trips(&db, "text", Value::text("héllo")).await;
        round_trips(&db, "blob", Value::bytes([9_u8, 8, 7])).await;
        round_trips(&db, "blob", Value::Uuid(Uuid::from_u128(7))).await;
        round_trips(
            &db,
            "text",
            Value::Decimal(Decimal::new(1999, 2).expect("ok")),
        )
        .await;
        round_trips(
            &db,
            "text",
            Value::Json(Json::parse(r#"{"a":1}"#).expect("valid JSON")),
        )
        .await;
        round_trips(&db, "text", Value::null(ValueKind::Text)).await;
    }

    #[tokio::test]
    async fn sqlite_says_what_it_cannot_store_instead_of_guessing() {
        let db = sqlite().await;
        run(&db, "create table t (v text)", vec![])
            .await
            .expect("create");

        let error = run(
            &db,
            "insert into t values (?)",
            vec![Value::Interval(Interval::from_days(1))],
        )
        .await
        .expect_err("SQLite has no interval type");
        let text = error.to_string();
        assert!(text.contains("interval"), "{text}");
        assert!(text.contains("help:"), "{text}");

        let error = run(
            &db,
            "insert into t values (?)",
            vec![Value::Array(Array::of([1_i64]))],
        )
        .await
        .expect_err("SQLite has no arrays");
        assert!(error.to_string().contains("help:"), "{error}");
    }

    #[tokio::test]
    async fn a_u64_above_the_signed_range_is_refused_rather_than_wrapped() {
        let db = sqlite().await;
        run(&db, "create table t (v integer)", vec![])
            .await
            .expect("create");

        let error = run(&db, "insert into t values (?)", vec![Value::U64(u64::MAX)])
            .await
            .expect_err("neither server has an unsigned 64-bit integer");
        let text = error.to_string();
        assert!(text.contains("unsigned"), "{text}");
        assert!(text.contains("help:"), "{text}");
    }

    #[tokio::test]
    async fn a_mixed_array_is_refused_rather_than_silently_truncated() {
        let Some(db) = postgres().await else { return };
        // A unique name, like every other PostgreSQL test here: the server is
        // shared with the rest of the suite (and with the previous run), so a
        // fixed name turns one failure between `create` and `drop` into a
        // permanently red test.
        let table = unique_table("array");
        run(&db, &format!("create table {table} (v bigint[])"), vec![])
            .await
            .expect("create");

        let mixed = Array::new(ValueKind::I64, [Value::I64(1), Value::text("two")]);
        let error = run(
            &db,
            &format!("insert into {table} values ($1)"),
            vec![Value::Array(mixed)],
        )
        .await
        .expect_err("an array must be homogeneous");
        assert!(
            error.to_string().contains("element type"),
            "the message must name the problem: {error}"
        );

        run(&db, &format!("drop table {table}"), vec![])
            .await
            .expect("drop");
        db.close().await;
    }

    /// Non-negotiable N7: the error names the problem and points at the field.
    #[tokio::test]
    async fn a_unique_violation_names_the_column_a_client_can_fix() {
        let Some(db) = postgres().await else { return };
        let table = unique_table("users");
        run(
            &db,
            &format!("create table {table} (id int primary key, email text unique)"),
            vec![],
        )
        .await
        .expect("create");
        run(
            &db,
            &format!("insert into {table} values (1, 'ada@example.com')"),
            vec![],
        )
        .await
        .expect("seed");

        let error = run(
            &db,
            &format!("insert into {table} values (2, 'ada@example.com')"),
            vec![],
        )
        .await
        .expect_err("the email is taken");

        let Error::UniqueViolation(violation) = &error else {
            panic!("expected a unique violation, got {error:?}");
        };
        assert_eq!(violation.sqlstate(), Some("23505"));
        assert_eq!(
            violation.columns(),
            ["email"],
            "the constraint name gives up the column: {violation:?}"
        );
        assert_eq!(error.field_pointer().as_deref(), Some("/email"));
        assert!(error.sql().is_some(), "the statement text is attached");
        assert!(!error.is_retryable());

        run(&db, &format!("drop table {table}"), vec![])
            .await
            .expect("drop");
        db.close().await;
    }

    #[tokio::test]
    async fn a_foreign_key_and_a_check_are_translated_too() {
        let Some(db) = postgres().await else { return };
        let parent = unique_table("parent");
        let child = unique_table("child");
        run(
            &db,
            &format!("create table {parent} (id int primary key)"),
            vec![],
        )
        .await
        .expect("create parent");
        run(
            &db,
            &format!(
                "create table {child} (id int primary key, \
                 parent_id int references {parent}(id), \
                 qty int check (qty > 0))"
            ),
            vec![],
        )
        .await
        .expect("create child");

        let error = run(
            &db,
            &format!("insert into {child} values (1, 99, 1)"),
            vec![],
        )
        .await
        .expect_err("there is no parent 99");
        assert!(matches!(error, Error::ForeignKeyViolation(_)), "{error:?}");
        assert_eq!(error.sqlstate(), Some("23503"));

        let error = run(
            &db,
            &format!("insert into {child} values (2, null, 0)"),
            vec![],
        )
        .await
        .expect_err("zero is not above zero");
        assert!(matches!(error, Error::CheckViolation(_)), "{error:?}");

        let error = run(
            &db,
            &format!("insert into {child} (id, parent_id, qty) values (null, null, 1)"),
            vec![],
        )
        .await
        .expect_err("the primary key is not nullable");
        assert!(matches!(error, Error::NotNullViolation(_)), "{error:?}");

        run(&db, &format!("drop table {child}"), vec![])
            .await
            .expect("drop");
        run(&db, &format!("drop table {parent}"), vec![])
            .await
            .expect("drop");
        db.close().await;
    }

    #[tokio::test]
    async fn sqlite_constraint_violations_are_translated_the_same_way() {
        let db = sqlite().await;
        run(
            &db,
            "create table t (id integer primary key, e text unique)",
            vec![],
        )
        .await
        .expect("create");
        run(&db, "insert into t values (1, 'a')", vec![])
            .await
            .expect("seed");

        let error = run(&db, "insert into t values (2, 'a')", vec![])
            .await
            .expect_err("`a` is taken");
        assert!(
            matches!(error, Error::UniqueViolation(_)),
            "SQLite's constraint errors must be translated too: {error:?}"
        );
    }

    #[tokio::test]
    async fn a_broken_statement_keeps_its_sqlstate_and_its_text() {
        let Some(db) = postgres().await else { return };
        let error = run(&db, "select * from a_table_that_is_not_there", vec![])
            .await
            .expect_err("no such table");

        assert_eq!(error.sqlstate(), Some("42P01"));
        assert_eq!(
            error.sql(),
            Some("select * from a_table_that_is_not_there"),
            "the parameterised text is attached, which is what makes a log searchable"
        );
        db.close().await;
    }

    #[tokio::test]
    async fn the_call_site_travels_with_the_error() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)", vec![])
            .await
            .expect("create");
        run(&db, "insert into t values (1)", vec![])
            .await
            .expect("seed");

        let site = CallSite::caller();
        let error = db
            .handle()
            .at(site)
            .for_entity("Widget")
            .execute_sql(Sql::new("insert into t values (1)".to_owned(), []))
            .await
            .expect_err("duplicate");

        let Error::UniqueViolation(violation) = &error else {
            panic!("{error:?}");
        };
        assert_eq!(violation.entity(), "Widget", "the builder named the entity");
        assert_eq!(
            violation.call_site().map(|at| at.file()),
            Some(site.file()),
            "and the error points at the user's file rather than this crate's"
        );
    }

    #[tokio::test]
    async fn every_statement_is_counted_and_a_write_moves_the_sticky_window() {
        let db = sqlite().await;
        let mark = db.statements().mark();

        run(&db, "create table t (id integer primary key)", vec![])
            .await
            .expect("create");
        run(&db, "select 1", vec![]).await.expect("select");
        assert_eq!(db.statements().since(mark), 2);
        assert!(db.statements().elapsed() > Duration::ZERO);

        let fresh = db.request_scoped();
        assert!(
            !fresh.prefers_primary(),
            "a request that has not written yet may read a replica"
        );
        run(&fresh, "select 1", vec![]).await.expect("select");
        assert!(!fresh.prefers_primary(), "a `select` is not a write");
        run(&fresh, "insert into t values (1)", vec![])
            .await
            .expect("insert");
        assert!(
            fresh.prefers_primary(),
            "and an `insert` is: reads follow writes for `sticky_window`"
        );
    }

    #[tokio::test]
    async fn a_metrics_recorder_sees_every_statement() {
        /// Counts what it is given and nothing else.
        #[derive(Default)]
        struct Counting {
            queries: AtomicU64,
            failures: AtomicU64,
            acquires: AtomicU64,
        }
        impl DbMetrics for Counting {
            fn query(&self, sample: &QuerySample<'_>) {
                self.queries.fetch_add(1, Ordering::Relaxed);
                if sample.failed {
                    self.failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            fn acquire(&self, _: &crate::db::AcquireSample) {
                self.acquires.fetch_add(1, Ordering::Relaxed);
            }
        }

        let recorder = Arc::new(Counting::default());
        let db = sqlite().await.with_metrics(recorder.clone());

        run(&db, "create table t (id integer primary key)", vec![])
            .await
            .expect("create");
        let _ = run(&db, "select * from nothing_here", vec![]).await;

        assert_eq!(recorder.queries.load(Ordering::Relaxed), 2);
        assert_eq!(recorder.failures.load(Ordering::Relaxed), 1);
        assert!(recorder.acquires.load(Ordering::Relaxed) >= 2);
    }

    #[tokio::test]
    async fn a_stream_yields_rows_one_at_a_time_and_reports_its_errors() {
        use futures_util::StreamExt as _;

        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)", vec![])
            .await
            .expect("create");
        run(&db, "insert into t values (1), (2), (3)", vec![])
            .await
            .expect("seed");

        let statement = moso_sql::Statement::Raw(moso_sql::RawStatement::new("select id from t"));
        let mut stream = db.handle().fetch_stream(statement);
        let mut seen = 0_usize;
        while let Some(row) = stream.next().await {
            row.expect("a row");
            seen += 1;
        }
        assert_eq!(seen, 3);

        // A statement that cannot run surfaces as the stream's first item
        // rather than as a panic at construction.
        let broken =
            moso_sql::Statement::Raw(moso_sql::RawStatement::new("select * from not_a_table"));
        let failures = db.handle().fetch_stream(broken).collect().await;
        assert!(failures.is_err(), "the stream reported the failure");
    }

    #[tokio::test]
    async fn the_slow_query_threshold_is_read_from_the_configuration() {
        // Nothing is asserted about the log line — a tracing subscriber is the
        // application's — but the path that formats it must run, because a
        // panic in a warning is a production outage on the slowest day.
        let Some(url) = crate::db::test_support::postgres_url() else {
            return;
        };
        let db = Db::connect(&DatabaseConfig::from_url(url).with_slow_query_ms(1))
            .await
            .expect("open");
        run(&db, "select pg_sleep(0.05)", vec![])
            .await
            .expect("select");
        db.close().await;
    }

    // ── the query span ──────────────────────────────────────────────────────

    /// One captured span: its name and every field recorded on it, at creation
    /// or later through `Span::record`.
    #[derive(Default)]
    struct CapturedSpan {
        name: &'static str,
        fields: std::collections::BTreeMap<&'static str, String>,
    }

    /// A minimal `tracing::Subscriber` that keeps every span it is told about.
    ///
    /// Hand-written rather than pulled from `tracing-subscriber`, so the test
    /// adds no dependency: it records the fields the ORM's span sets and lets
    /// the test read them back. `enabled` is unconditionally true so a
    /// `debug`-level span is created and its fields flow through.
    #[derive(Default)]
    struct Capturing {
        spans: std::sync::Mutex<std::collections::HashMap<u64, CapturedSpan>>,
        next: std::sync::atomic::AtomicU64,
    }

    /// Stringifies whatever value a field carries, by whichever typed hook the
    /// value arrives through.
    struct FieldSink<'a>(&'a mut std::collections::BTreeMap<&'static str, String>);

    impl tracing::field::Visit for FieldSink<'_> {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
            self.0.insert(field.name(), format!("{value:?}"));
        }
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.0.insert(field.name(), value.to_owned());
        }
        fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
            self.0.insert(field.name(), value.to_string());
        }
        fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
            self.0.insert(field.name(), value.to_string());
        }
    }

    impl tracing::Subscriber for Capturing {
        fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
            true
        }

        fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            let raw = self.next.fetch_add(1, Ordering::Relaxed) + 1;
            let mut captured = CapturedSpan {
                name: attrs.metadata().name(),
                fields: std::collections::BTreeMap::new(),
            };
            attrs.record(&mut FieldSink(&mut captured.fields));
            self.spans
                .lock()
                .expect("not poisoned")
                .insert(raw, captured);
            tracing::span::Id::from_u64(raw)
        }

        fn record(&self, span: &tracing::span::Id, values: &tracing::span::Record<'_>) {
            if let Some(captured) = self
                .spans
                .lock()
                .expect("not poisoned")
                .get_mut(&span.into_u64())
            {
                values.record(&mut FieldSink(&mut captured.fields));
            }
        }

        fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
        fn event(&self, _: &tracing::Event<'_>) {}
        fn enter(&self, _: &tracing::span::Id) {}
        fn exit(&self, _: &tracing::span::Id) {}
    }

    /// A query run under a subscriber emits a `db.query` span carrying the
    /// OpenTelemetry-shaped fields, so it nests under a request span in a trace.
    ///
    /// SQLite in-memory needs no server, so this runs everywhere — it is not
    /// gated on `DATABASE_URL`.
    #[tokio::test]
    async fn a_query_emits_a_db_span_with_otel_fields() {
        let db = sqlite().await;
        run(&db, "create table widgets (id integer primary key)", vec![])
            .await
            .expect("create");
        run(&db, "insert into widgets values (1), (2)", vec![])
            .await
            .expect("seed");

        let recorder = Arc::new(Capturing::default());
        let subscriber = tracing::Dispatch::new(Arc::clone(&recorder));

        {
            let _guard = tracing::dispatcher::set_default(&subscriber);
            run(&db, "select id from widgets", vec![])
                .await
                .expect("select");
        }

        let spans = recorder.spans.lock().expect("not poisoned");
        let query = spans
            .values()
            .find(|span| {
                span.name == "db.query"
                    && span.fields.get("db.operation").map(String::as_str) == Some("select")
            })
            .expect("the select produced a `db.query` span");

        assert_eq!(
            query.fields.get("db.system").map(String::as_str),
            Some("sqlite")
        );
        assert_eq!(
            query.fields.get("db.statement").map(String::as_str),
            Some("select id from widgets"),
            "the span carries the parameterised statement text"
        );
        assert_eq!(
            query.fields.get("db.in_transaction").map(String::as_str),
            Some("false")
        );
        assert_eq!(
            query.fields.get("db.rows").map(String::as_str),
            Some("2"),
            "the row count is recorded after the statement runs"
        );
        assert_eq!(
            query.fields.get("otel.status_code").map(String::as_str),
            Some("OK")
        );
        assert!(
            query.fields.contains_key("db.duration_ms"),
            "the elapsed time is recorded on the span"
        );
    }
}
