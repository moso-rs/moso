//! The raw-SQL escape hatch — non-negotiable N8.
//!
//! [`RawQuery`] is what `moso::sql!` expands to: a fragment the programmer
//! wrote, with the interpolated identifiers bound as **parameters**. There is
//! no path from a runtime string to SQL syntax here, so the macro cannot
//! produce an injection even when it is handed a request body.
//!
//! What this does not cover, [`Db::postgres_pool`](crate::Db::postgres_pool)
//! does: everything sqlx can do is one method away.

use core::marker::PhantomData;

use moso_sql::{Sql, Value};

use crate::entity::Entity;
use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::projection::Projection;
use crate::sqltype::SqlType;

/// A hand-written statement with bound parameters.
///
/// ```
/// use moso_orm::RawQuery;
///
/// let query = RawQuery::new("select id, email from users where created_at > $1")
///     .bind(0_i64);
///
/// assert_eq!(query.args().len(), 1);
/// assert!(query.text().contains("$1"));
/// ```
pub struct RawQuery {
    text: String,
    args: Vec<Value>,
    read_only: bool,
}

impl RawQuery {
    /// A statement with no parameters yet.
    ///
    /// The text is written by the programmer; values arrive through
    /// [`RawQuery::bind`], never through `format!`.
    ///
    /// ```
    /// use moso_orm::RawQuery;
    ///
    /// assert_eq!(RawQuery::new("select 1").args().len(), 0);
    /// ```
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            args: Vec::new(),
            read_only: false,
        }
    }

    /// Binds one parameter, in order.
    ///
    /// ```
    /// use moso_orm::RawQuery;
    ///
    /// let q = RawQuery::new("select * from users where id = $1").bind(7_i64);
    /// assert_eq!(q.args().len(), 1);
    /// ```
    #[must_use]
    pub fn bind(mut self, value: impl SqlType) -> Self {
        self.args.push(value.into_value());
        self
    }

    /// Binds a borrowed string without an intermediate `String`.
    ///
    /// `&str` is deliberately not a [`SqlType`] — a decode would have to hand
    /// back a borrow of the row's buffer — so the one common borrowed case gets
    /// its own door rather than forcing `.to_owned()` at every call.
    ///
    /// ```
    /// use moso_orm::RawQuery;
    ///
    /// let q = RawQuery::new("select * from users where email = $1").bind_text("a@b.c");
    /// assert_eq!(q.args().len(), 1);
    /// ```
    #[must_use]
    pub fn bind_text(mut self, value: &str) -> Self {
        self.args.push(Value::text(value));
        self
    }

    /// Binds an already-bound value.
    ///
    /// ```
    /// use moso_orm::RawQuery;
    /// use moso_sql::Value;
    ///
    /// let q = RawQuery::new("select $1").bind_value(Value::I32(1));
    /// assert_eq!(q.args().len(), 1);
    /// ```
    #[must_use]
    pub fn bind_value(mut self, value: Value) -> Self {
        self.args.push(value);
        self
    }

    /// Declares the statement read-only, so it may run on a replica.
    ///
    /// ```
    /// use moso_orm::RawQuery;
    ///
    /// assert!(RawQuery::new("select 1").read_only().is_read_only());
    /// ```
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// The statement text.
    ///
    /// ```
    /// assert_eq!(moso_orm::RawQuery::new("select 1").text(), "select 1");
    /// ```
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The bound parameters, in order.
    ///
    /// ```
    /// assert!(moso_orm::RawQuery::new("select 1").args().is_empty());
    /// ```
    #[must_use]
    pub fn args(&self) -> &[Value] {
        &self.args
    }

    /// Whether the statement may run on a replica.
    ///
    /// ```
    /// assert!(!moso_orm::RawQuery::new("select 1").is_read_only());
    /// ```
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// The rendered form the executor runs.
    ///
    /// ```
    /// use moso_orm::RawQuery;
    ///
    /// let sql = RawQuery::new("select 1").into_sql();
    /// assert_eq!(sql.as_str(), "select 1");
    /// ```
    #[must_use]
    pub fn into_sql(self) -> Sql {
        Sql::new(self.text, self.args)
    }

    /// Runs the statement and decodes every row as `E`.
    ///
    /// # Errors
    ///
    /// [`Error::Decode`] when the select list does not match the entity, plus
    /// the rest of [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, RawQuery, Result};
    /// async fn run<E: Entity>(q: RawQuery, ex: impl Executor<'_>) -> Result<Vec<E>> {
    ///     q.fetch_all::<E>(ex).await
    /// }
    /// ```
    pub async fn fetch_all<E: Entity>(self, executor: impl Executor<'_>) -> Result<Vec<E>> {
        let rows = executor.handle().fetch_all_sql(self.into_sql()).await?;
        let mut entities = Vec::with_capacity(rows.len());
        for row in &rows {
            entities.push(E::from_row(row)?);
        }
        Ok(entities)
    }

    /// Runs the statement and decodes the single row it must produce.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when nothing came back.
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, RawQuery, Result};
    /// async fn run<E: Entity>(q: RawQuery, ex: impl Executor<'_>) -> Result<E> {
    ///     q.fetch_one::<E>(ex).await
    /// }
    /// ```
    pub async fn fetch_one<E: Entity>(self, executor: impl Executor<'_>) -> Result<E> {
        self.fetch_optional::<E>(executor)
            .await?
            .ok_or(Error::NotFound { entity: E::NAME })
    }

    /// Runs the statement and decodes the first row, if there is one.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, RawQuery, Result};
    /// async fn run<E: Entity>(q: RawQuery, ex: impl Executor<'_>) -> Result<Option<E>> {
    ///     q.fetch_optional::<E>(ex).await
    /// }
    /// ```
    pub async fn fetch_optional<E: Entity>(self, executor: impl Executor<'_>) -> Result<Option<E>> {
        let Some(row) = executor
            .handle()
            .fetch_optional_sql(self.into_sql())
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(E::from_row(&row)?))
    }

    /// Runs the statement and decodes every row into a projection.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Executor, Projection, RawQuery, Result};
    /// async fn run<P: Projection>(q: RawQuery, ex: impl Executor<'_>) -> Result<Vec<P>> {
    ///     q.project_all::<P>(ex).await
    /// }
    /// ```
    pub async fn project_all<P: Projection>(self, executor: impl Executor<'_>) -> Result<Vec<P>> {
        let rows = executor.handle().fetch_all_sql(self.into_sql()).await?;
        let mut projected = Vec::with_capacity(rows.len());
        for row in &rows {
            projected.push(P::from_row(row)?);
        }
        Ok(projected)
    }

    /// Runs the statement for its effect.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Executor, RawQuery, Result};
    /// async fn run(q: RawQuery, ex: impl Executor<'_>) -> Result<u64> {
    ///     q.execute(ex).await
    /// }
    /// ```
    pub async fn execute(self, executor: impl Executor<'_>) -> Result<u64> {
        executor.handle().execute_sql(self.into_sql()).await
    }
}

impl core::fmt::Debug for RawQuery {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The text is safe to print — it is parameterised — and the values are
        // deliberately reduced to a count, because a `Debug` that prints bound
        // parameters ends up in a log with somebody's password in it.
        f.debug_struct("RawQuery")
            .field("text", &self.text)
            .field("args", &self.args.len())
            .finish()
    }
}

/// A statement that decodes into a chosen type, for the `sql!` macro's
/// turbofish-free form.
///
/// ```
/// use moso_orm::{RawQuery, Typed};
///
/// # struct UserSummary;
/// let typed: Typed<UserSummary> = Typed::new(RawQuery::new("select 1"));
/// assert_eq!(typed.query().text(), "select 1");
/// ```
pub struct Typed<T> {
    query: RawQuery,
    output: PhantomData<fn() -> T>,
}

impl<T> Typed<T> {
    /// Attaches a decode target to a raw statement.
    ///
    /// ```
    /// # use moso_orm::{RawQuery, Typed};
    /// # struct Summary;
    /// let typed: Typed<Summary> = Typed::new(RawQuery::new("select 1"));
    /// assert!(typed.query().args().is_empty());
    /// ```
    #[must_use]
    pub const fn new(query: RawQuery) -> Self {
        Self {
            query,
            output: PhantomData,
        }
    }

    /// The statement underneath.
    ///
    /// ```
    /// # use moso_orm::{RawQuery, Typed};
    /// # struct Summary;
    /// let typed: Typed<Summary> = Typed::new(RawQuery::new("select 1"));
    /// assert_eq!(typed.query().text(), "select 1");
    /// ```
    #[must_use]
    pub const fn query(&self) -> &RawQuery {
        &self.query
    }
}

impl<T: Projection> Typed<T> {
    /// Runs the statement and decodes every row.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Executor, Projection, Result, Typed};
    /// async fn run<P: Projection>(t: Typed<P>, ex: impl Executor<'_>) -> Result<Vec<P>> {
    ///     t.fetch_all(ex).await
    /// }
    /// ```
    pub async fn fetch_all(self, executor: impl Executor<'_>) -> Result<Vec<T>> {
        self.query.project_all::<T>(executor).await
    }
}

impl<T> core::fmt::Debug for Typed<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Typed").field("query", &self.query).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_becomes_a_parameter_never_text() {
        let sneaky = "'; drop table users; --";
        let query = RawQuery::new("select * from users where name = $1").bind_text(sneaky);

        assert!(
            !query.text().contains("drop table"),
            "a bound value must never reach the statement text"
        );
        assert_eq!(query.args().len(), 1);
        assert_eq!(query.args()[0], Value::text(sneaky));
    }

    #[test]
    fn debug_counts_the_arguments_rather_than_printing_them() {
        let query = RawQuery::new("select $1").bind_text("hunter2");
        let rendered = format!("{query:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert!(rendered.contains("args: 1"), "{rendered}");
    }

    #[test]
    fn a_read_only_statement_says_so() {
        assert!(!RawQuery::new("select 1").is_read_only());
        assert!(RawQuery::new("select 1").read_only().is_read_only());
    }

    #[test]
    fn rendering_keeps_the_text_and_the_arguments_together() {
        let sql = RawQuery::new("select $1, $2")
            .bind(1_i64)
            .bind_text("x")
            .into_sql();
        assert_eq!(sql.as_str(), "select $1, $2");
        assert_eq!(sql.arg_count(), 2);
    }
}
