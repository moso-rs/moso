//! The request-scoped transaction, and the error mapping that makes a
//! constraint violation an HTTP problem document.
//!
//! # The shape of the thing
//!
//! ```text
//!   RequestTxLayer::apply            ← inserts an empty slot
//!         │
//!         ├─ handler runs
//!         │     Depends<RequestTx>   ← opens the transaction, once, lazily
//!         │
//!         └─ response returns
//!               commits on 2xx, rolls back otherwise
//! ```
//!
//! The split is forced: "did this succeed" is only known **after** the handler
//! has returned, which a dependency cannot observe, and "is a transaction
//! wanted at all" is only known from the handler's signature, which a layer
//! cannot see. So the layer provides a slot and the dependency fills it, and a
//! handler that never asks for one never opens a transaction.
//!
//! # Why retry is off
//!
//! A serialisation failure inside a request transaction cannot be retried: the
//! request body has already been consumed, and re-running the handler is not
//! something a middleware can do. It becomes a `409` with `retryable: true`,
//! and the documentation points at [`Db::transaction`](crate::Db::transaction)
//! — whose closure *can* be re-run — for work that needs the retry.
//!
//! This module is private; [`RequestTx`] and [`RequestTxLayer`] are re-exported
//! from [`crate::tx`], which is where the frozen paths live.

use core::fmt;
use std::convert::Infallible;
use std::sync::Arc;
use std::task::{Context, Poll};

use moso_core::ctx::RequestCtx;
use moso_core::deps::http;
use moso_core::deps::tower::Service;
use moso_core::di::Dependency;
use moso_core::openapi::{OperationBuilder, ResponseSpec};
use moso_core::router::Route;
use moso_core::{BoxFuture, Request, Response};
use moso_schema::ValidationErrors;

use crate::db::Db;
use crate::error::{Error, Result};
use crate::tx::{Tx, TxOptions};

/// The request-scoped transaction: opened when a handler asks for it,
/// committed after a 2xx, rolled back otherwise.
///
/// `Depends<RequestTx>` in a handler signature is what turns that handler into
/// an atomic unit. Retry is **off**: the request body may already have been
/// consumed, so a serialisation failure becomes a `409` with
/// `retryable: true` and the documentation points at
/// [`Db::transaction`](crate::Db::transaction) for work that can be re-run.
///
/// ```no_run
/// # use moso_orm::{RequestTx, Result};
/// async fn use_it(tx: &RequestTx) -> Result<()> {
///     // `&RequestTx` is an `Executor`, so a query runs on it directly.
///     let _ = tx.tx();
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct RequestTx {
    inner: Arc<Tx>,
}

impl RequestTx {
    /// The transaction underneath.
    ///
    /// ```no_run
    /// # use moso_orm::{RequestTx, Tx};
    /// fn inner(request_tx: &RequestTx) -> &Tx {
    ///     request_tx.tx()
    /// }
    /// ```
    #[must_use]
    pub fn tx(&self) -> &Tx {
        &self.inner
    }

    /// The handle the transaction was opened on.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, RequestTx};
    /// fn pool(request_tx: &RequestTx) -> &Db {
    ///     request_tx.db()
    /// }
    /// ```
    #[must_use]
    pub fn db(&self) -> &Db {
        self.inner.db()
    }

    /// The options a request transaction is opened with: everything the
    /// application configured, and **no retries**.
    ///
    /// ```
    /// use moso_orm::RequestTx;
    ///
    /// assert_eq!(RequestTx::options().max_retries, 0);
    /// ```
    #[must_use]
    pub fn options() -> TxOptions {
        TxOptions::new().max_retries(0)
    }
}

impl fmt::Debug for RequestTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RequestTx").finish_non_exhaustive()
    }
}

impl core::ops::Deref for RequestTx {
    type Target = Tx;

    fn deref(&self) -> &Tx {
        &self.inner
    }
}

impl Dependency for RequestTx {
    fn describe(operation: &mut OperationBuilder) {
        operation.response(
            409,
            ResponseSpec::problem(
                "the transaction lost a race with a concurrent one. `retryable` is `true`: the \
                 same request may be sent again.",
            ),
        );
        operation.response(
            503,
            ResponseSpec::problem(
                "no database connection became free within `database.acquire_timeout`. \
                 `Retry-After` says when to come back.",
            ),
        );
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        let Some(slot) = ctx.extension::<RequestTxSlot>() else {
            return Err(moso_core::Error::internal_msg(
                "`Depends<RequestTx>` needs the `request-tx` middleware, and this route does not \
                 have it\n  \
                 help: `Router::layer(RequestTxLayer::new())`, or `App::middleware(|stack| \
                 stack.push(RequestTxLayer::new()))`\n  \
                 note: the transaction has to be committed after the response is known, which \
                 only a layer can see",
            ));
        };

        let mut open = slot.0.lock().await;
        if let Some(existing) = open.as_ref() {
            // Two extractors in one signature share one transaction. This is
            // the whole reason the slot exists rather than each `Depends`
            // opening its own.
            return Ok(existing.clone());
        }

        let db = ctx.provider::<Db>()?;
        let tx = db.begin_with(RequestTx::options()).await?;
        let request_tx = RequestTx {
            inner: Arc::new(tx),
        };
        *open = Some(request_tx.clone());
        Ok(request_tx)
    }
}

/// The lazily-filled transaction one request shares.
///
/// `Clone` because [`RequestCtx`] snapshots the request extensions; the `Arc`
/// is what makes the clone the *same* slot.
#[derive(Clone, Default)]
pub(crate) struct RequestTxSlot(Arc<tokio::sync::Mutex<Option<RequestTx>>>);

impl RequestTxSlot {
    /// Commits or rolls back whatever the handler opened, if it opened
    /// anything.
    async fn finish(&self, commit: bool) -> Result<()> {
        let opened = self.0.lock().await.take();
        let Some(request_tx) = opened else {
            return Ok(());
        };
        // The handler may still hold a clone — a background task it spawned, a
        // struct it stashed. The transaction ends here regardless, which is
        // what "request-scoped" means; a later statement on the stale handle
        // gets `this transaction has already been committed`.
        if commit {
            request_tx.inner.commit_shared().await
        } else {
            request_tx.inner.rollback_shared().await
        }
    }
}

/// The middleware that commits a [`RequestTx`] after a 2xx and rolls it back
/// otherwise.
///
/// Installed by the application into the middleware stack. It has to be a
/// layer, not part of the dependency, because "did the response succeed" is
/// only known after the handler has returned.
///
/// ```
/// use moso_orm::RequestTxLayer;
/// use moso_core::middleware::CustomLayer;
///
/// let layer = RequestTxLayer::new();
/// assert_eq!(layer.name(), "request-tx");
/// ```
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestTxLayer {
    commit_on_client_error: bool,
}

impl RequestTxLayer {
    /// A layer that commits on 2xx and rolls back on everything else.
    ///
    /// ```
    /// use moso_orm::RequestTxLayer;
    ///
    /// assert!(!RequestTxLayer::new().commits_on_client_error());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            commit_on_client_error: false,
        }
    }

    /// Commits on 4xx as well as 2xx.
    ///
    /// For the applications that record a rejected attempt — a failed login, a
    /// rate-limit hit — in the same transaction that rejected it.
    ///
    /// ```
    /// use moso_orm::RequestTxLayer;
    ///
    /// assert!(RequestTxLayer::new().commit_on_client_error().commits_on_client_error());
    /// ```
    #[must_use]
    pub const fn commit_on_client_error(mut self) -> Self {
        self.commit_on_client_error = true;
        self
    }

    /// Whether a 4xx commits.
    ///
    /// ```
    /// use moso_orm::RequestTxLayer;
    ///
    /// assert!(!RequestTxLayer::new().commits_on_client_error());
    /// ```
    #[must_use]
    pub const fn commits_on_client_error(&self) -> bool {
        self.commit_on_client_error
    }

    /// Whether a response with this status commits.
    ///
    /// ```
    /// use moso_orm::RequestTxLayer;
    ///
    /// assert!(RequestTxLayer::new().commits(200));
    /// assert!(!RequestTxLayer::new().commits(500));
    /// assert!(RequestTxLayer::new().commit_on_client_error().commits(422));
    /// ```
    #[must_use]
    pub const fn commits(&self, status: u16) -> bool {
        if status < 300 {
            return true;
        }
        self.commit_on_client_error && status < 500
    }
}

impl moso_core::middleware::CustomLayer for RequestTxLayer {
    fn name(&self) -> &'static str {
        "request-tx"
    }

    fn apply(&self, service: Route) -> Route {
        Route::new(RequestTxService {
            inner: service,
            layer: *self,
        })
    }

    fn summary(&self) -> String {
        if self.commit_on_client_error {
            String::from("commits on 2xx and 4xx, rolls back on 5xx and panic")
        } else {
            String::from("commits on 2xx, rolls back otherwise")
        }
    }
}

/// The service [`RequestTxLayer`] wraps a route in.
#[derive(Clone)]
struct RequestTxService {
    inner: Route,
    layer: RequestTxLayer,
}

impl Service<Request> for RequestTxService {
    type Response = Response;
    type Error = Infallible;
    type Future = BoxFuture<'static, core::result::Result<Response, Infallible>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<core::result::Result<(), Infallible>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: Request) -> Self::Future {
        // The clone-and-swap dance: `self.inner` is the instance that was
        // polled ready, so it is the one that must be called.
        let ready = self.inner.clone();
        let mut inner = core::mem::replace(&mut self.inner, ready);
        let layer = self.layer;

        let slot = RequestTxSlot::default();
        request.extensions_mut().insert(slot.clone());

        Box::pin(async move {
            let response = inner.call(request).await?;
            let status = response.status().as_u16();
            let commit = layer.commits(status);

            if let Err(error) = slot.finish(commit).await {
                // A commit that fails turns a 2xx into the error it really was:
                // answering 200 for work the database refused is the one
                // outcome a transaction middleware must never produce.
                if commit && status < 300 {
                    tracing::error!(
                        status,
                        "db: the request transaction failed to commit: {error}"
                    );
                    let problem: moso_core::Error = error.into();
                    return Ok(moso_core::IntoResponse::into_response(problem));
                }
                tracing::warn!(
                    status,
                    "db: the request transaction failed to unwind: {error}"
                );
            }
            Ok(response)
        })
    }
}

/// Turns a data-layer error into an HTTP problem response.
///
/// This is non-negotiable N7 made mechanical: a unique violation becomes a
/// `409` with a JSON Pointer at the offending field, a `NotFound` becomes a
/// `404`, a pool timeout becomes a `503`, and a programmer error becomes a
/// `500` whose detail is suppressed in production.
impl From<Error> for moso_core::Error {
    fn from(error: Error) -> Self {
        let detail = error.to_string();
        match &error {
            Error::NotFound { entity } => moso_core::Error::not_found(*entity),

            Error::UniqueViolation(violation) => {
                let mut problem = moso_core::Error::conflict(violation.message().to_owned());
                if let Some(pointer) = violation.pointer() {
                    problem = problem.with_field(&pointer, "unique", violation.message());
                }
                problem.with_source(error)
            }

            // 422 rather than 400: the request parsed, and the value it named
            // does not exist. A 400 would say the syntax was wrong, which it
            // was not.
            Error::ForeignKeyViolation(violation)
            | Error::NotNullViolation(violation)
            | Error::CheckViolation(violation) => {
                let code = match &error {
                    Error::ForeignKeyViolation(_) => "foreign_key",
                    Error::NotNullViolation(_) => "required",
                    _ => "invalid",
                };
                let errors = ValidationErrors::one(
                    violation.pointer().unwrap_or_default(),
                    code,
                    violation.message().to_owned(),
                );
                moso_core::Error::validation(errors).with_source(error)
            }

            Error::StaleWrite { .. } => moso_core::Error::conflict(detail)
                .with_extension("retryable", true)
                .with_source(error),

            Error::Serialization { .. } | Error::Deadlock { .. } => {
                moso_core::Error::conflict(detail)
                    .with_extension("retryable", true)
                    .with_source(error)
            }

            Error::TenantMissing { .. } => moso_core::Error::internal(error),

            Error::PoolTimeout { .. } => {
                let mut problem = moso_core::Error::unavailable(
                    "the database connection pool is exhausted; try again shortly",
                );
                if let Ok(value) = http::HeaderValue::from_str("1") {
                    problem = problem.with_header(http::header::RETRY_AFTER, value);
                }
                problem.with_extension("retryable", true).with_source(error)
            }

            Error::StatementTimeout { after } => {
                moso_core::Error::timeout(*after).with_source(error)
            }

            Error::Connection { .. } => {
                moso_core::Error::unavailable("the database is not reachable").with_source(error)
            }

            Error::Decode(_) | Error::Cursor(_) => {
                // A cursor a client sent is a client error; a column that will
                // not decode is ours. `CursorError` carries its own message,
                // and it is safe to show.
                if matches!(error, Error::Cursor(_)) {
                    moso_core::Error::bad_request(detail).with_source(error)
                } else {
                    moso_core::Error::internal(error)
                }
            }

            _ => moso_core::Error::internal(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::ConstraintViolation;
    use moso_core::middleware::CustomLayer as _;

    #[test]
    fn the_request_layer_commits_on_success_only_by_default() {
        let layer = RequestTxLayer::new();
        assert!(layer.commits(200));
        assert!(layer.commits(201));
        assert!(layer.commits(204));
        assert!(!layer.commits(400));
        assert!(!layer.commits(422));
        assert!(!layer.commits(500));

        let lenient = RequestTxLayer::new().commit_on_client_error();
        assert!(lenient.commits(422));
        assert!(!lenient.commits(500));
    }

    #[test]
    fn the_layer_names_itself_for_the_middleware_listing() {
        let layer = RequestTxLayer::new();
        assert_eq!(layer.name(), "request-tx");
        assert!(layer.summary().contains("2xx"));
        assert!(
            RequestTxLayer::new()
                .commit_on_client_error()
                .summary()
                .contains("4xx")
        );
    }

    #[test]
    fn a_request_transaction_never_retries() {
        assert_eq!(RequestTx::options().max_retries, 0);
        assert!(!RequestTx::options().retries());
    }

    #[test]
    fn a_unique_violation_is_a_409_with_a_field_pointer() {
        let violation = ConstraintViolation::unique("User", "users_email_key")
            .with_column("email")
            .with_message("that email address is already registered");
        let problem: moso_core::Error = Error::UniqueViolation(Box::new(violation)).into();

        assert_eq!(problem.status(), http::StatusCode::CONFLICT);
        let fields = problem.fields().expect("a field pointer");
        assert!(
            format!("{fields:?}").contains("/email"),
            "the pointer names the column: {fields:?}"
        );
    }

    #[test]
    fn a_pool_timeout_is_a_503_with_retry_after_and_never_a_hang() {
        let problem: moso_core::Error = Error::PoolTimeout {
            waited: core::time::Duration::from_secs(10),
            size: 8,
        }
        .into();

        assert_eq!(problem.status(), http::StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            problem
                .headers()
                .is_some_and(|headers| headers.contains_key(http::header::RETRY_AFTER)),
            "a 503 without `Retry-After` tells a client nothing"
        );
        assert!(problem.retryable());
    }

    #[test]
    fn a_serialisation_failure_is_a_409_that_says_it_is_retryable() {
        let problem: moso_core::Error = Error::Serialization {
            code: String::from("40001"),
        }
        .into();

        assert_eq!(problem.status(), http::StatusCode::CONFLICT);
        assert_eq!(
            problem
                .extensions()
                .get("retryable")
                .map(ToString::to_string),
            Some(String::from("true")),
            "this is the flag that tells a client the same request may be sent again"
        );
    }

    #[test]
    fn a_missing_row_is_a_404_and_a_programmer_error_is_a_500() {
        let missing: moso_core::Error = Error::not_found("Post").into();
        assert_eq!(missing.status(), http::StatusCode::NOT_FOUND);

        let ours: moso_core::Error = Error::UnfilteredWrite {
            operation: "UPDATE",
            table: "users",
        }
        .into();
        assert_eq!(ours.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        let tenant: moso_core::Error = Error::TenantMissing { entity: "Invoice" }.into();
        assert_eq!(
            tenant.status(),
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "a missing tenant scope is our bug, never the client's"
        );
    }

    #[test]
    fn a_statement_timeout_is_a_504() {
        let problem: moso_core::Error = Error::StatementTimeout {
            after: core::time::Duration::from_secs(30),
        }
        .into();
        assert_eq!(problem.status(), http::StatusCode::GATEWAY_TIMEOUT);
    }
}

#[cfg(test)]
mod real_database {
    use super::*;
    use crate::db::test_support::sqlite;
    use moso_sql::Sql;

    /// Runs SQL with no parameters.
    async fn run<'e>(executor: impl crate::Executor<'e>, sql: &str) -> Result<u64> {
        executor
            .handle()
            .execute_sql(Sql::new(sql.to_owned(), []))
            .await
    }

    /// Stands in for `Depends<RequestTx>`: what the extractor does once the
    /// layer has put a slot in the request.
    async fn open(slot: &RequestTxSlot, db: &Db) -> RequestTx {
        let mut held = slot.0.lock().await;
        if let Some(existing) = held.as_ref() {
            return existing.clone();
        }
        let tx = db.begin_with(RequestTx::options()).await.expect("begin");
        let request_tx = RequestTx {
            inner: Arc::new(tx),
        };
        *held = Some(request_tx.clone());
        request_tx
    }

    #[tokio::test]
    async fn a_slot_nobody_used_commits_nothing() {
        let db = sqlite().await;
        let slot = RequestTxSlot::default();
        // A handler that never asked for a transaction must not pay for one.
        slot.finish(true).await.expect("nothing to commit");
        db.ping().await.expect("and no connection was taken");
    }

    #[tokio::test]
    async fn two_extractors_in_one_request_share_one_transaction() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        let slot = RequestTxSlot::default();
        let first = open(&slot, &db).await;
        let second = open(&slot, &db).await;
        assert!(
            core::ptr::eq(first.tx(), second.tx()),
            "a second `Depends<RequestTx>` must join the transaction, not open a second one"
        );

        run(&first, "insert into t values (1)")
            .await
            .expect("insert");
        run(&second, "insert into t values (2)")
            .await
            .expect("insert");
        slot.finish(true).await.expect("commit");

        assert_eq!(
            run(&db, "delete from t").await.expect("count"),
            2,
            "both extractors wrote into the same transaction, and it committed"
        );
    }

    #[tokio::test]
    async fn a_non_success_status_rolls_the_request_transaction_back() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        let layer = RequestTxLayer::new();
        for (status, expected_rows) in [(200_u16, 1_u64), (500, 0), (422, 0)] {
            run(&db, "delete from t").await.expect("reset");

            let slot = RequestTxSlot::default();
            let request_tx = open(&slot, &db).await;
            run(&request_tx, "insert into t values (1)")
                .await
                .expect("insert");
            drop(request_tx);

            slot.finish(layer.commits(status)).await.expect("unwind");
            assert_eq!(
                run(&db, "delete from t").await.expect("count"),
                expected_rows,
                "status {status}"
            );
        }
    }

    #[tokio::test]
    async fn a_lenient_layer_keeps_the_writes_a_4xx_made() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        // The failed-login-attempt case: the request is rejected and the record
        // of the rejection is kept.
        let layer = RequestTxLayer::new().commit_on_client_error();
        let slot = RequestTxSlot::default();
        let request_tx = open(&slot, &db).await;
        run(&request_tx, "insert into t values (1)")
            .await
            .expect("insert");
        drop(request_tx);
        slot.finish(layer.commits(429)).await.expect("commit");

        assert_eq!(run(&db, "delete from t").await.expect("count"), 1);
    }

    #[tokio::test]
    async fn a_handler_that_kept_a_clone_cannot_write_after_the_response() {
        let db = sqlite().await;
        run(&db, "create table t (id integer primary key)")
            .await
            .expect("create");

        let slot = RequestTxSlot::default();
        let stale = open(&slot, &db).await;
        slot.finish(true).await.expect("commit");

        let error = run(&stale, "insert into t values (1)")
            .await
            .expect_err("the request is over");
        assert!(
            error.to_string().contains("already been committed"),
            "a statement after the response must say so rather than open a second transaction: \
             {error}"
        );
        assert_eq!(run(&db, "delete from t").await.expect("count"), 0);
    }
}
