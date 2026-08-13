//! `DELETE`, with `USING`, `RETURNING`, and the same unfiltered guard as
//! [`Update`](crate::Update).

use crate::dialect::Dialect;
use crate::error::Error;
use crate::expr::Expr;
use crate::ident::{Ident, TableRef};
use crate::select::{Cte, FromItem};
use crate::sql::Sql;
use crate::statement::{Returning, Statement, StatementRef};

/// A `DELETE` statement.
///
/// ```
/// use moso_sql::{Delete, Expr, Ident, TableRef};
///
/// let delete = Delete::from_table(TableRef::from_static("sessions"))
///     .filter(Expr::col(Ident::from_static("expires_at")).lt(Expr::value(0_i64)));
/// assert!(delete.has_filter());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    with: Vec<Cte>,
    table: TableRef,
    alias: Option<Ident>,
    using: Vec<FromItem>,
    filters: Vec<Expr>,
    returning: Returning,
}

impl Delete {
    /// A `DELETE FROM table` with nothing else set yet.
    ///
    /// ```
    /// use moso_sql::{Delete, TableRef};
    ///
    /// assert!(!Delete::from_table(TableRef::from_static("t")).has_filter());
    /// ```
    #[must_use]
    pub const fn from_table(table: TableRef) -> Self {
        Self {
            with: Vec::new(),
            table,
            alias: None,
            using: Vec::new(),
            filters: Vec::new(),
            returning: Returning::None,
        }
    }

    /// Aliases the target table.
    ///
    /// ```
    /// # use moso_sql::{Delete, Ident, TableRef};
    /// let d = Delete::from_table(TableRef::from_static("t")).alias(Ident::from_static("x"));
    /// assert_eq!(d.table_alias().map(Ident::as_str), Some("x"));
    /// ```
    #[must_use]
    pub fn alias(mut self, alias: Ident) -> Self {
        self.alias = Some(alias);
        self
    }

    /// Adds a `USING` item — PostgreSQL's join-in-a-delete.
    ///
    /// ```
    /// # use moso_sql::{Delete, FromItem, TableRef};
    /// let d = Delete::from_table(TableRef::from_static("t"))
    ///     .using(FromItem::table(TableRef::from_static("s")));
    /// assert_eq!(d.using_items().len(), 1);
    /// ```
    #[must_use]
    pub fn using(mut self, source: FromItem) -> Self {
        self.using.push(source);
        self
    }

    /// Adds a predicate, `AND`-ed with the ones already there.
    ///
    /// ```
    /// # use moso_sql::{Delete, Expr, TableRef};
    /// assert!(Delete::from_table(TableRef::from_static("t")).filter(Expr::value(true)).has_filter());
    /// ```
    #[must_use]
    pub fn filter(mut self, predicate: Expr) -> Self {
        self.filters.push(predicate);
        self
    }

    /// Adds a predicate only if there is one.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert!(!Delete::from_table(TableRef::from_static("t")).filter_opt(None).has_filter());
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
    /// # use moso_sql::{Cte, Delete, Ident, Select, TableRef};
    /// let d = Delete::from_table(TableRef::from_static("t"))
    ///     .with(Cte::new(Ident::from_static("c"), Select::new()));
    /// assert_eq!(d.ctes().len(), 1);
    /// ```
    #[must_use]
    pub fn with(mut self, cte: Cte) -> Self {
        self.with.push(cte);
        self
    }

    /// Sets the `RETURNING` clause.
    ///
    /// ```
    /// # use moso_sql::{Delete, Returning, TableRef};
    /// let d = Delete::from_table(TableRef::from_static("t")).returning(Returning::All);
    /// assert_eq!(d.returning_clause(), &Returning::All);
    /// ```
    #[must_use]
    pub fn returning(mut self, returning: Returning) -> Self {
        self.returning = returning;
        self
    }

    /// The target table.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert_eq!(Delete::from_table(TableRef::from_static("t")).target().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn target(&self) -> &TableRef {
        &self.table
    }

    /// The target table's alias, if it has one.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert!(Delete::from_table(TableRef::from_static("t")).table_alias().is_none());
    /// ```
    #[must_use]
    pub const fn table_alias(&self) -> Option<&Ident> {
        self.alias.as_ref()
    }

    /// The `USING` items.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert!(Delete::from_table(TableRef::from_static("t")).using_items().is_empty());
    /// ```
    #[must_use]
    pub fn using_items(&self) -> &[FromItem] {
        &self.using
    }

    /// The `WHERE` predicates.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert!(Delete::from_table(TableRef::from_static("t")).filters().is_empty());
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Expr] {
        &self.filters
    }

    /// Whether the statement has any predicate at all.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert!(!Delete::from_table(TableRef::from_static("t")).has_filter());
    /// ```
    #[must_use]
    pub fn has_filter(&self) -> bool {
        !self.filters.is_empty()
    }

    /// The `RETURNING` clause.
    ///
    /// ```
    /// # use moso_sql::{Delete, Returning, TableRef};
    /// assert_eq!(Delete::from_table(TableRef::from_static("t")).returning_clause(), &Returning::None);
    /// ```
    #[must_use]
    pub const fn returning_clause(&self) -> &Returning {
        &self.returning
    }

    /// The common table expressions.
    ///
    /// ```
    /// # use moso_sql::{Delete, TableRef};
    /// assert!(Delete::from_table(TableRef::from_static("t")).ctes().is_empty());
    /// ```
    #[must_use]
    pub fn ctes(&self) -> &[Cte] {
        &self.with
    }

    /// Renders the statement for a dialect.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a clause the
    /// dialect does not have — SQLite has no `USING`, for instance.
    ///
    /// ```
    /// # use moso_sql::{Delete, Expr, Ident, Postgres, TableRef};
    /// let sql = Delete::from_table(TableRef::from_static("t"))
    ///     .filter(Expr::col(Ident::from_static("id")).eq(Expr::value(1)))
    ///     .build(&Postgres)?;
    /// assert_eq!(sql.args.len(), 1);
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    pub fn build(&self, dialect: &dyn Dialect) -> Result<Sql, Error> {
        dialect.build(StatementRef::Delete(self))
    }

    /// Wraps the statement as a [`Statement`].
    ///
    /// ```
    /// # use moso_sql::{Delete, Statement, TableRef};
    /// let s = Delete::from_table(TableRef::from_static("t")).into_statement();
    /// assert!(matches!(s, Statement::Delete(_)));
    /// ```
    #[must_use]
    pub fn into_statement(self) -> Statement {
        Statement::Delete(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unfiltered_guard_sees_what_it_should() {
        let bare = Delete::from_table(TableRef::from_static("t"));
        assert!(!bare.has_filter());
        assert!(bare.filter(Expr::value(true)).has_filter());
    }
}
