//! `INSERT`, including `ON CONFLICT` and `RETURNING`.

use crate::dialect::Dialect;
use crate::error::Error;
use crate::expr::Expr;
use crate::ident::{Ident, TableRef};
use crate::select::{Cte, Select};
use crate::sql::Sql;
use crate::statement::{Assignment, Returning, Statement, StatementRef};

/// An `INSERT` statement.
///
/// The column list is set once and every row is checked against it at build
/// time, so a row with the wrong number of values is
/// [`Error::RowArity`] rather than a database error
/// with no context.
///
/// ```
/// use moso_sql::{Expr, Ident, Insert, TableRef};
///
/// let insert = Insert::into_table(TableRef::from_static("users"))
///     .columns([Ident::from_static("email"), Ident::from_static("name")])
///     .values([Expr::value("ada@example.com"), Expr::value("Ada")]);
/// assert_eq!(insert.row_count(), 1);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    with: Vec<Cte>,
    table: TableRef,
    columns: Vec<Ident>,
    rows: Vec<Vec<Expr>>,
    source: Option<Box<Select>>,
    default_values: bool,
    on_conflict: Option<OnConflict>,
    returning: Returning,
}

impl Insert {
    /// An `INSERT INTO table` with nothing else set yet.
    ///
    /// ```
    /// use moso_sql::{Insert, TableRef};
    ///
    /// assert_eq!(Insert::into_table(TableRef::from_static("t")).row_count(), 0);
    /// ```
    #[must_use]
    pub const fn into_table(table: TableRef) -> Self {
        Self {
            with: Vec::new(),
            table,
            columns: Vec::new(),
            rows: Vec::new(),
            source: None,
            default_values: false,
            on_conflict: None,
            returning: Returning::None,
        }
    }

    /// Sets the column list. Every row must have this many values.
    ///
    /// ```
    /// # use moso_sql::{Ident, Insert, TableRef};
    /// let insert = Insert::into_table(TableRef::from_static("t"))
    ///     .columns([Ident::from_static("a")]);
    /// assert_eq!(insert.column_names().len(), 1);
    /// ```
    #[must_use]
    pub fn columns(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.columns = columns.into_iter().collect();
        self
    }

    /// Adds one row.
    ///
    /// Call it repeatedly for a multi-row insert: one statement, one round
    /// trip, which is the difference between a bulk load that takes a second
    /// and one that takes a minute.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Insert, TableRef};
    /// let insert = Insert::into_table(TableRef::from_static("t"))
    ///     .columns([Ident::from_static("a")])
    ///     .values([Expr::value(1)])
    ///     .values([Expr::value(2)]);
    /// assert_eq!(insert.row_count(), 2);
    /// ```
    #[must_use]
    pub fn values(mut self, row: impl IntoIterator<Item = Expr>) -> Self {
        self.rows.push(row.into_iter().collect());
        self
    }

    /// Adds several rows at once.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Insert, TableRef};
    /// let insert = Insert::into_table(TableRef::from_static("t"))
    ///     .columns([Ident::from_static("a")])
    ///     .rows([vec![Expr::value(1)], vec![Expr::value(2)]]);
    /// assert_eq!(insert.row_count(), 2);
    /// ```
    #[must_use]
    pub fn rows(mut self, rows: impl IntoIterator<Item = Vec<Expr>>) -> Self {
        self.rows.extend(rows);
        self
    }

    /// `INSERT INTO … SELECT …`.
    ///
    /// ```
    /// # use moso_sql::{Ident, Insert, Select, TableRef};
    /// let insert = Insert::into_table(TableRef::from_static("archive"))
    ///     .columns([Ident::from_static("id")])
    ///     .from_select(Select::new());
    /// assert!(insert.source_query().is_some());
    /// ```
    #[must_use]
    pub fn from_select(mut self, query: Select) -> Self {
        self.source = Some(Box::new(query));
        self
    }

    /// `INSERT INTO … DEFAULT VALUES`.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(Insert::into_table(TableRef::from_static("t")).default_values().uses_default_values());
    /// ```
    #[must_use]
    pub const fn default_values(mut self) -> Self {
        self.default_values = true;
        self
    }

    /// Prepends a common table expression.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Insert, Select, TableRef};
    /// let insert = Insert::into_table(TableRef::from_static("t"))
    ///     .with(Cte::new(Ident::from_static("c"), Select::new()));
    /// assert_eq!(insert.ctes().len(), 1);
    /// ```
    #[must_use]
    pub fn with(mut self, cte: Cte) -> Self {
        self.with.push(cte);
        self
    }

    /// Sets the `ON CONFLICT` clause.
    ///
    /// ```
    /// use moso_sql::{Ident, Insert, OnConflict, TableRef};
    ///
    /// let upsert = Insert::into_table(TableRef::from_static("users"))
    ///     .on_conflict(OnConflict::columns([Ident::from_static("email")]).do_nothing());
    /// assert!(upsert.conflict().is_some());
    /// ```
    #[must_use]
    pub fn on_conflict(mut self, conflict: OnConflict) -> Self {
        self.on_conflict = Some(conflict);
        self
    }

    /// Sets the `RETURNING` clause.
    ///
    /// ```
    /// use moso_sql::{Insert, Returning, TableRef};
    ///
    /// let insert = Insert::into_table(TableRef::from_static("t")).returning(Returning::All);
    /// assert_eq!(insert.returning_clause(), &Returning::All);
    /// ```
    #[must_use]
    pub fn returning(mut self, returning: Returning) -> Self {
        self.returning = returning;
        self
    }

    /// The target table.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert_eq!(Insert::into_table(TableRef::from_static("t")).table().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// The column list.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(Insert::into_table(TableRef::from_static("t")).column_names().is_empty());
    /// ```
    #[must_use]
    pub fn column_names(&self) -> &[Ident] {
        &self.columns
    }

    /// The rows.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(Insert::into_table(TableRef::from_static("t")).value_rows().is_empty());
    /// ```
    #[must_use]
    pub fn value_rows(&self) -> &[Vec<Expr>] {
        &self.rows
    }

    /// How many rows this statement inserts, not counting a `SELECT` source.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert_eq!(Insert::into_table(TableRef::from_static("t")).row_count(), 0);
    /// ```
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// How many parameters the value rows will bind, which is what a batched
    /// insert has to compare against
    /// [`Dialect::max_bind_params`] before
    /// deciding on a chunk size.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Insert, TableRef};
    /// let insert = Insert::into_table(TableRef::from_static("t"))
    ///     .columns([Ident::from_static("a")])
    ///     .values([Expr::value(1)])
    ///     .values([Expr::value(2)]);
    /// assert_eq!(insert.bind_count(), 2);
    /// ```
    #[must_use]
    pub fn bind_count(&self) -> usize {
        self.rows.iter().map(Vec::len).sum()
    }

    /// The `SELECT` source, if this is an `INSERT … SELECT`.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(Insert::into_table(TableRef::from_static("t")).source_query().is_none());
    /// ```
    #[must_use]
    pub fn source_query(&self) -> Option<&Select> {
        self.source.as_deref()
    }

    /// Whether `DEFAULT VALUES` was asked for.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(!Insert::into_table(TableRef::from_static("t")).uses_default_values());
    /// ```
    #[must_use]
    pub const fn uses_default_values(&self) -> bool {
        self.default_values
    }

    /// The `ON CONFLICT` clause, if any.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(Insert::into_table(TableRef::from_static("t")).conflict().is_none());
    /// ```
    #[must_use]
    pub const fn conflict(&self) -> Option<&OnConflict> {
        self.on_conflict.as_ref()
    }

    /// The `RETURNING` clause.
    ///
    /// ```
    /// # use moso_sql::{Insert, Returning, TableRef};
    /// assert_eq!(Insert::into_table(TableRef::from_static("t")).returning_clause(), &Returning::None);
    /// ```
    #[must_use]
    pub const fn returning_clause(&self) -> &Returning {
        &self.returning
    }

    /// The common table expressions.
    ///
    /// ```
    /// # use moso_sql::{Insert, TableRef};
    /// assert!(Insert::into_table(TableRef::from_static("t")).ctes().is_empty());
    /// ```
    #[must_use]
    pub fn ctes(&self) -> &[Cte] {
        &self.with
    }

    /// Renders the statement for a dialect.
    ///
    /// # Errors
    ///
    /// [`Error::RowArity`] if a row does not match the
    /// column list, [`Error::Incomplete`] if there is
    /// nothing to insert, and [`Error::Unsupported`]
    /// for a clause the dialect does not have.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Insert, Postgres, TableRef};
    /// let sql = Insert::into_table(TableRef::from_static("t"))
    ///     .columns([Ident::from_static("a")])
    ///     .values([Expr::value(1)])
    ///     .build(&Postgres)?;
    /// assert_eq!(sql.args.len(), 1);
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    pub fn build(&self, dialect: &dyn Dialect) -> Result<Sql, Error> {
        dialect.build(StatementRef::Insert(self))
    }

    /// Wraps the statement as a [`Statement`].
    ///
    /// ```
    /// # use moso_sql::{Insert, Statement, TableRef};
    /// let s = Insert::into_table(TableRef::from_static("t")).into_statement();
    /// assert!(matches!(s, Statement::Insert(_)));
    /// ```
    #[must_use]
    pub fn into_statement(self) -> Statement {
        Statement::Insert(self)
    }
}

/// An `ON CONFLICT` clause.
///
/// ```
/// use moso_sql::{Ident, OnConflict};
///
/// // The idempotent-seed idiom.
/// let ignore = OnConflict::columns([Ident::from_static("email")]).do_nothing();
/// assert!(matches!(ignore.action(), moso_sql::ConflictAction::DoNothing));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct OnConflict {
    target: ConflictTarget,
    target_where: Option<Expr>,
    action: ConflictAction,
    update_where: Option<Expr>,
}

impl OnConflict {
    /// Conflicts on a unique index over these columns.
    ///
    /// ```
    /// # use moso_sql::{Ident, OnConflict};
    /// let c = OnConflict::columns([Ident::from_static("email")]);
    /// assert!(matches!(c.target(), moso_sql::ConflictTarget::Columns(_)));
    /// ```
    #[must_use]
    pub fn columns(columns: impl IntoIterator<Item = Ident>) -> Self {
        Self {
            target: ConflictTarget::Columns(columns.into_iter().collect()),
            target_where: None,
            action: ConflictAction::DoNothing,
            update_where: None,
        }
    }

    /// Conflicts on a named constraint.
    ///
    /// ```
    /// # use moso_sql::{Ident, OnConflict};
    /// let c = OnConflict::constraint(Ident::from_static("users_email_key"));
    /// assert!(matches!(c.target(), moso_sql::ConflictTarget::Constraint(_)));
    /// ```
    #[must_use]
    pub const fn constraint(name: Ident) -> Self {
        Self {
            target: ConflictTarget::Constraint(name),
            target_where: None,
            action: ConflictAction::DoNothing,
            update_where: None,
        }
    }

    /// Conflicts on any unique constraint. Only valid with `DO NOTHING`.
    ///
    /// ```
    /// # use moso_sql::OnConflict;
    /// assert!(matches!(OnConflict::any().target(), moso_sql::ConflictTarget::Any));
    /// ```
    #[must_use]
    pub const fn any() -> Self {
        Self {
            target: ConflictTarget::Any,
            target_where: None,
            action: ConflictAction::DoNothing,
            update_where: None,
        }
    }

    /// Restricts the inferred index to a partial one with this predicate.
    ///
    /// A partial unique index is only matched when the `ON CONFLICT` target
    /// repeats its `WHERE`, which is a rule the server enforces and nothing
    /// explains.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, OnConflict};
    /// let c = OnConflict::columns([Ident::from_static("email")])
    ///     .target_where(Expr::col(Ident::from_static("deleted_at")).is_null());
    /// assert!(c.target_predicate().is_some());
    /// ```
    #[must_use]
    pub fn target_where(mut self, predicate: Expr) -> Self {
        self.target_where = Some(predicate);
        self
    }

    /// `DO NOTHING`.
    ///
    /// ```
    /// # use moso_sql::{ConflictAction, OnConflict};
    /// assert!(matches!(OnConflict::any().do_nothing().action(), ConflictAction::DoNothing));
    /// ```
    #[must_use]
    pub fn do_nothing(mut self) -> Self {
        self.action = ConflictAction::DoNothing;
        self
    }

    /// `DO UPDATE SET …` with explicit assignments.
    ///
    /// ```
    /// use moso_sql::{Assignment, Expr, Ident, OnConflict};
    ///
    /// let c = OnConflict::columns([Ident::from_static("email")]).do_update([
    ///     Assignment::new(Ident::from_static("name"), Expr::excluded(Ident::from_static("name"))),
    /// ]);
    /// assert!(matches!(c.action(), moso_sql::ConflictAction::DoUpdate(_)));
    /// ```
    #[must_use]
    pub fn do_update(mut self, assignments: impl IntoIterator<Item = Assignment>) -> Self {
        self.action = ConflictAction::DoUpdate(assignments.into_iter().collect());
        self
    }

    /// `DO UPDATE SET c = excluded.c` for each named column — the ordinary
    /// upsert.
    ///
    /// ```
    /// # use moso_sql::{Ident, OnConflict};
    /// let c = OnConflict::columns([Ident::from_static("email")])
    ///     .do_update_columns([Ident::from_static("name")]);
    /// assert!(matches!(c.action(), moso_sql::ConflictAction::DoUpdate(_)));
    /// ```
    #[must_use]
    pub fn do_update_columns(self, columns: impl IntoIterator<Item = Ident>) -> Self {
        let assignments = columns.into_iter().map(|column| {
            let value = Expr::excluded(column.clone());
            Assignment::new(column, value)
        });
        self.do_update(assignments)
    }

    /// Adds a `WHERE` to the `DO UPDATE`, so the update only happens when the
    /// existing row actually needs it.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, OnConflict};
    /// let c = OnConflict::columns([Ident::from_static("email")])
    ///     .do_update_columns([Ident::from_static("name")])
    ///     .update_where(Expr::value(true));
    /// assert!(c.update_predicate().is_some());
    /// ```
    #[must_use]
    pub fn update_where(mut self, predicate: Expr) -> Self {
        self.update_where = Some(predicate);
        self
    }

    /// What the clause conflicts on.
    ///
    /// ```
    /// # use moso_sql::{ConflictTarget, OnConflict};
    /// assert!(matches!(OnConflict::any().target(), ConflictTarget::Any));
    /// ```
    #[must_use]
    pub const fn target(&self) -> &ConflictTarget {
        &self.target
    }

    /// The target's partial-index predicate, if any.
    ///
    /// ```
    /// # use moso_sql::OnConflict;
    /// assert!(OnConflict::any().target_predicate().is_none());
    /// ```
    #[must_use]
    pub const fn target_predicate(&self) -> Option<&Expr> {
        self.target_where.as_ref()
    }

    /// What happens on a conflict.
    ///
    /// ```
    /// # use moso_sql::{ConflictAction, OnConflict};
    /// assert!(matches!(OnConflict::any().action(), ConflictAction::DoNothing));
    /// ```
    #[must_use]
    pub const fn action(&self) -> &ConflictAction {
        &self.action
    }

    /// The `DO UPDATE`'s own `WHERE`, if any.
    ///
    /// ```
    /// # use moso_sql::OnConflict;
    /// assert!(OnConflict::any().update_predicate().is_none());
    /// ```
    #[must_use]
    pub const fn update_predicate(&self) -> Option<&Expr> {
        self.update_where.as_ref()
    }
}

/// What an `ON CONFLICT` clause conflicts on.
///
/// ```
/// use moso_sql::{ConflictTarget, Ident};
///
/// assert!(matches!(ConflictTarget::Constraint(Ident::from_static("k")), ConflictTarget::Constraint(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ConflictTarget {
    /// Any unique constraint. Valid only with `DO NOTHING`.
    Any,
    /// The unique index inferred from these columns.
    Columns(Vec<Ident>),
    /// A named constraint.
    Constraint(Ident),
}

/// What an `ON CONFLICT` clause does.
///
/// ```
/// use moso_sql::ConflictAction;
///
/// assert!(matches!(ConflictAction::DoNothing, ConflictAction::DoNothing));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ConflictAction {
    /// `DO NOTHING`.
    DoNothing,
    /// `DO UPDATE SET …`.
    DoUpdate(Vec<Assignment>),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn do_update_columns_expands_to_excluded() {
        let conflict = OnConflict::columns([Ident::from_static("email")])
            .do_update_columns([Ident::from_static("name"), Ident::from_static("updated_at")]);
        match conflict.action() {
            ConflictAction::DoUpdate(assignments) => {
                assert_eq!(assignments.len(), 2);
                assert_eq!(assignments[0].column().as_str(), "name");
                assert_eq!(
                    assignments[0].value(),
                    &Expr::excluded(Ident::from_static("name"))
                );
            }
            other => panic!("expected a DO UPDATE, got {other:?}"),
        }
    }

    #[test]
    fn bind_count_is_what_a_batcher_needs() {
        let insert = Insert::into_table(TableRef::from_static("t"))
            .columns([Ident::from_static("a"), Ident::from_static("b")])
            .values([Expr::value(1), Expr::value(2)])
            .values([Expr::value(3), Expr::value(4)]);
        assert_eq!(insert.row_count(), 2);
        assert_eq!(insert.bind_count(), 4);
    }
}
