//! `UPDATE`, with `FROM`, `RETURNING`, and the guard against an unfiltered
//! mass update.

use crate::dialect::Dialect;
use crate::error::Error;
use crate::expr::Expr;
use crate::ident::{Ident, TableRef};
use crate::select::{Cte, FromItem};
use crate::sql::Sql;
use crate::statement::{Assignment, Returning, Statement, StatementRef};

/// An `UPDATE` statement.
///
/// # The unfiltered-update guard
///
/// [`Update::has_filter`] exists so the layer above can refuse an `UPDATE` with
/// no `WHERE` unless the caller asked for one explicitly. This crate builds
/// what it is told — a statement with no filter is valid SQL and there are
/// legitimate uses — but `moso-orm` requires either a `.filter()` or an
/// `.all_rows()`, because an accidental mass update has cost real companies
/// real data.
///
/// ```
/// use moso_sql::{Expr, Ident, TableRef, Update};
///
/// let update = Update::table(TableRef::from_static("users"))
///     .set(Ident::from_static("name"), Expr::value("Ada"))
///     .filter(Expr::col(Ident::from_static("id")).eq(Expr::value(1)));
/// assert!(update.has_filter());
/// assert_eq!(update.assignments().len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    with: Vec<Cte>,
    table: TableRef,
    alias: Option<Ident>,
    assignments: Vec<Assignment>,
    from: Vec<FromItem>,
    filters: Vec<Expr>,
    returning: Returning,
}

impl Update {
    /// An `UPDATE table` with nothing else set yet.
    ///
    /// ```
    /// use moso_sql::{TableRef, Update};
    ///
    /// assert!(!Update::table(TableRef::from_static("t")).has_filter());
    /// ```
    #[must_use]
    pub const fn table(table: TableRef) -> Self {
        Self {
            with: Vec::new(),
            table,
            alias: None,
            assignments: Vec::new(),
            from: Vec::new(),
            filters: Vec::new(),
            returning: Returning::None,
        }
    }

    /// Aliases the target table, which an `UPDATE … FROM` needs when it joins
    /// the table to itself.
    ///
    /// ```
    /// # use moso_sql::{Ident, TableRef, Update};
    /// let u = Update::table(TableRef::from_static("t")).alias(Ident::from_static("x"));
    /// assert_eq!(u.table_alias().map(Ident::as_str), Some("x"));
    /// ```
    #[must_use]
    pub fn alias(mut self, alias: Ident) -> Self {
        self.alias = Some(alias);
        self
    }

    /// Assigns a value to a column.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, TableRef, Update};
    /// let u = Update::table(TableRef::from_static("t"))
    ///     .set(Ident::from_static("a"), Expr::value(1));
    /// assert_eq!(u.assignments().len(), 1);
    /// ```
    #[must_use]
    pub fn set(mut self, column: Ident, value: Expr) -> Self {
        self.assignments.push(Assignment::new(column, value));
        self
    }

    /// Adds an already-built assignment.
    ///
    /// ```
    /// # use moso_sql::{Assignment, Expr, Ident, TableRef, Update};
    /// let u = Update::table(TableRef::from_static("t"))
    ///     .set_assignment(Assignment::new(Ident::from_static("a"), Expr::value(1)));
    /// assert_eq!(u.assignments().len(), 1);
    /// ```
    #[must_use]
    pub fn set_assignment(mut self, assignment: Assignment) -> Self {
        self.assignments.push(assignment);
        self
    }

    /// Assigns an expression computed from the column's current value, which
    /// is how a counter is incremented without a read-modify-write race.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, TableRef, Update};
    /// // `login_count = login_count + 1`
    /// let u = Update::table(TableRef::from_static("users"))
    ///     .set_with(Ident::from_static("login_count"), |current| current.plus(Expr::value(1)));
    /// assert_eq!(u.assignments().len(), 1);
    /// ```
    #[must_use]
    pub fn set_with(self, column: Ident, value: impl FnOnce(Expr) -> Expr) -> Self {
        let current = Expr::col(column.clone());
        self.set(column, value(current))
    }

    /// Adds a `FROM` item — PostgreSQL's join-in-an-update.
    ///
    /// ```
    /// # use moso_sql::{FromItem, TableRef, Update};
    /// let u = Update::table(TableRef::from_static("t"))
    ///     .from(FromItem::table(TableRef::from_static("s")));
    /// assert_eq!(u.from_items().len(), 1);
    /// ```
    #[must_use]
    pub fn from(mut self, source: FromItem) -> Self {
        self.from.push(source);
        self
    }

    /// Adds a predicate, `AND`-ed with the ones already there.
    ///
    /// ```
    /// # use moso_sql::{Expr, TableRef, Update};
    /// assert!(Update::table(TableRef::from_static("t")).filter(Expr::value(true)).has_filter());
    /// ```
    #[must_use]
    pub fn filter(mut self, predicate: Expr) -> Self {
        self.filters.push(predicate);
        self
    }

    /// Adds a predicate only if there is one.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert!(!Update::table(TableRef::from_static("t")).filter_opt(None).has_filter());
    /// ```
    #[must_use]
    pub fn filter_opt(self, predicate: Option<Expr>) -> Self {
        match predicate {
            Some(predicate) => self.filter(predicate),
            None => self,
        }
    }

    /// Prepends a common table expression.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select, TableRef, Update};
    /// let u = Update::table(TableRef::from_static("t"))
    ///     .with(Cte::new(Ident::from_static("c"), Select::new()));
    /// assert_eq!(u.ctes().len(), 1);
    /// ```
    #[must_use]
    pub fn with(mut self, cte: Cte) -> Self {
        self.with.push(cte);
        self
    }

    /// Sets the `RETURNING` clause.
    ///
    /// ```
    /// # use moso_sql::{Returning, TableRef, Update};
    /// let u = Update::table(TableRef::from_static("t")).returning(Returning::All);
    /// assert_eq!(u.returning_clause(), &Returning::All);
    /// ```
    #[must_use]
    pub fn returning(mut self, returning: Returning) -> Self {
        self.returning = returning;
        self
    }

    /// The target table.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert_eq!(Update::table(TableRef::from_static("t")).target().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn target(&self) -> &TableRef {
        &self.table
    }

    /// The target table's alias, if it has one.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert!(Update::table(TableRef::from_static("t")).table_alias().is_none());
    /// ```
    #[must_use]
    pub const fn table_alias(&self) -> Option<&Ident> {
        self.alias.as_ref()
    }

    /// The `SET` assignments.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert!(Update::table(TableRef::from_static("t")).assignments().is_empty());
    /// ```
    #[must_use]
    pub fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }

    /// The `FROM` items.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert!(Update::table(TableRef::from_static("t")).from_items().is_empty());
    /// ```
    #[must_use]
    pub fn from_items(&self) -> &[FromItem] {
        &self.from
    }

    /// The `WHERE` predicates.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert!(Update::table(TableRef::from_static("t")).filters().is_empty());
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Expr] {
        &self.filters
    }

    /// Whether the statement has any predicate at all.
    ///
    /// The layer above uses this to refuse an accidental mass update.
    ///
    /// ```
    /// # use moso_sql::{Expr, TableRef, Update};
    /// assert!(!Update::table(TableRef::from_static("t")).has_filter());
    /// assert!(Update::table(TableRef::from_static("t")).filter(Expr::value(true)).has_filter());
    /// ```
    #[must_use]
    pub fn has_filter(&self) -> bool {
        !self.filters.is_empty()
    }

    /// The `RETURNING` clause.
    ///
    /// ```
    /// # use moso_sql::{Returning, TableRef, Update};
    /// assert_eq!(Update::table(TableRef::from_static("t")).returning_clause(), &Returning::None);
    /// ```
    #[must_use]
    pub const fn returning_clause(&self) -> &Returning {
        &self.returning
    }

    /// The common table expressions.
    ///
    /// ```
    /// # use moso_sql::{TableRef, Update};
    /// assert!(Update::table(TableRef::from_static("t")).ctes().is_empty());
    /// ```
    #[must_use]
    pub fn ctes(&self) -> &[Cte] {
        &self.with
    }

    /// Renders the statement for a dialect.
    ///
    /// # Errors
    ///
    /// [`Error::Incomplete`] if there is nothing to
    /// set, and [`Error::Unsupported`] for a clause
    /// the dialect does not have.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Postgres, TableRef, Update};
    /// let sql = Update::table(TableRef::from_static("t"))
    ///     .set(Ident::from_static("a"), Expr::value(1))
    ///     .filter(Expr::col(Ident::from_static("id")).eq(Expr::value(2)))
    ///     .build(&Postgres)?;
    /// assert_eq!(sql.args.len(), 2);
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    pub fn build(&self, dialect: &dyn Dialect) -> Result<Sql, Error> {
        dialect.build(StatementRef::Update(self))
    }

    /// Wraps the statement as a [`Statement`].
    ///
    /// ```
    /// # use moso_sql::{Statement, TableRef, Update};
    /// assert!(matches!(Update::table(TableRef::from_static("t")).into_statement(), Statement::Update(_)));
    /// ```
    #[must_use]
    pub fn into_statement(self) -> Statement {
        Statement::Update(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_with_reads_the_current_value() {
        let update = Update::table(TableRef::from_static("users"))
            .set_with(Ident::from_static("login_count"), |current| {
                current.plus(Expr::value(1))
            });
        let assignment = &update.assignments()[0];
        assert_eq!(assignment.column().as_str(), "login_count");
        assert!(matches!(assignment.value(), Expr::Binary { .. }));
    }

    #[test]
    fn the_unfiltered_guard_sees_what_it_should() {
        let bare = Update::table(TableRef::from_static("t"));
        assert!(!bare.has_filter());
        assert!(bare.filter_opt(None).filter(Expr::value(true)).has_filter());
    }
}
