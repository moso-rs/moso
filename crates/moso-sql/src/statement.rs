//! The statement enum, the borrowed form dialects render, and the pieces
//! several statements share.

use crate::ddl::Ddl;
use crate::delete::Delete;
use crate::dialect::Dialect;
use crate::error::Error;
use crate::expr::Expr;
use crate::ident::{ColumnRef, Ident};
use crate::insert::Insert;
use crate::select::{Select, SelectItem};
use crate::sql::Sql;
use crate::update::Update;
use crate::value::{Bindable, Value};

/// Any statement `moso-sql` can build.
///
/// ```
/// use moso_sql::{Select, Statement, StatementKind};
///
/// let statement = Select::new().into_statement();
/// assert_eq!(statement.kind(), StatementKind::Select);
/// assert!(statement.is_read_only());
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Statement {
    /// A `SELECT`.
    Select(Select),
    /// An `INSERT`.
    Insert(Insert),
    /// An `UPDATE`.
    Update(Update),
    /// A `DELETE`.
    Delete(Delete),
    /// A schema-changing statement.
    Ddl(Ddl),
    /// A raw statement with bound parameters.
    Raw(RawStatement),
}

impl Statement {
    /// The borrowed form, which is what [`Dialect::build`] takes.
    ///
    /// ```
    /// use moso_sql::{Select, Statement, StatementRef};
    ///
    /// let statement = Select::new().into_statement();
    /// assert!(matches!(statement.borrowed(), StatementRef::Select(_)));
    /// ```
    #[must_use]
    pub const fn borrowed(&self) -> StatementRef<'_> {
        match self {
            Self::Select(select) => StatementRef::Select(select),
            Self::Insert(insert) => StatementRef::Insert(insert),
            Self::Update(update) => StatementRef::Update(update),
            Self::Delete(delete) => StatementRef::Delete(delete),
            Self::Ddl(ddl) => StatementRef::Ddl(ddl),
            Self::Raw(raw) => StatementRef::Raw(raw),
        }
    }

    /// What kind of statement this is.
    ///
    /// ```
    /// use moso_sql::{Select, StatementKind};
    ///
    /// assert_eq!(Select::new().into_statement().kind(), StatementKind::Select);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> StatementKind {
        self.borrowed().kind()
    }

    /// Whether the statement only reads.
    ///
    /// A read-only statement may be routed to a replica; anything else must
    /// not be. A [`Statement::Raw`] is treated as a write, because the crate
    /// does not parse it and guessing wrong sends a write to a replica.
    ///
    /// ```
    /// use moso_sql::{RawStatement, Select, Statement};
    ///
    /// assert!(Select::new().into_statement().is_read_only());
    /// assert!(!Statement::Raw(RawStatement::new("vacuum")).is_read_only());
    /// ```
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.borrowed().is_read_only()
    }

    /// Renders the statement for a dialect.
    ///
    /// # Errors
    ///
    /// [`Error`] if the statement is incomplete or uses a construct the
    /// dialect does not have.
    ///
    /// ```
    /// use moso_sql::{Postgres, Select, TableRef};
    ///
    /// let sql = Select::from_table(TableRef::from_static("t")).select_all()
    ///     .into_statement()
    ///     .build(&Postgres)?;
    /// assert!(sql.text.starts_with("SELECT"));
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    pub fn build(&self, dialect: &dyn Dialect) -> Result<Sql, Error> {
        dialect.build(self.borrowed())
    }
}

/// A borrowed statement.
///
/// [`Dialect::build`] takes this rather than `&Statement` so that
/// `Select::build` does not have to clone itself into a `Statement` first.
///
/// ```
/// use moso_sql::{Select, StatementRef};
///
/// let select = Select::new();
/// assert!(matches!(StatementRef::Select(&select), StatementRef::Select(_)));
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub enum StatementRef<'a> {
    /// A `SELECT`.
    Select(&'a Select),
    /// An `INSERT`.
    Insert(&'a Insert),
    /// An `UPDATE`.
    Update(&'a Update),
    /// A `DELETE`.
    Delete(&'a Delete),
    /// A schema-changing statement.
    Ddl(&'a Ddl),
    /// A raw statement.
    Raw(&'a RawStatement),
}

impl StatementRef<'_> {
    /// What kind of statement this is.
    ///
    /// ```
    /// use moso_sql::{Select, StatementKind, StatementRef};
    ///
    /// let select = Select::new();
    /// assert_eq!(StatementRef::Select(&select).kind(), StatementKind::Select);
    /// ```
    #[must_use]
    pub const fn kind(self) -> StatementKind {
        match self {
            Self::Select(_) => StatementKind::Select,
            Self::Insert(_) => StatementKind::Insert,
            Self::Update(_) => StatementKind::Update,
            Self::Delete(_) => StatementKind::Delete,
            Self::Ddl(_) => StatementKind::Ddl,
            Self::Raw(_) => StatementKind::Raw,
        }
    }

    /// Whether the statement only reads.
    ///
    /// ```
    /// use moso_sql::{Select, StatementRef};
    ///
    /// let select = Select::new();
    /// assert!(StatementRef::Select(&select).is_read_only());
    /// ```
    #[must_use]
    pub const fn is_read_only(self) -> bool {
        matches!(self, Self::Select(_))
    }
}

/// The kind of a statement, without its payload.
///
/// ```
/// use moso_sql::StatementKind;
///
/// assert_eq!(StatementKind::Select.as_str(), "SELECT");
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum StatementKind {
    /// A `SELECT`.
    Select,
    /// An `INSERT`.
    Insert,
    /// An `UPDATE`.
    Update,
    /// A `DELETE`.
    Delete,
    /// A schema-changing statement.
    Ddl,
    /// A raw statement.
    Raw,
}

impl StatementKind {
    /// The keyword, for log lines, span fields and error messages.
    ///
    /// ```
    /// assert_eq!(moso_sql::StatementKind::Ddl.as_str(), "DDL");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Select => "SELECT",
            Self::Insert => "INSERT",
            Self::Update => "UPDATE",
            Self::Delete => "DELETE",
            Self::Ddl => "DDL",
            Self::Raw => "RAW",
        }
    }
}

/// A `RETURNING` clause.
///
/// PostgreSQL and SQLite (3.35 and later) both support it; asking for one on a
/// dialect that does not is [`Error::Unsupported`]
/// rather than a silently dropped clause, because a caller that expected a row
/// back and got none would misreport the write as a failure.
///
/// ```
/// use moso_sql::{ColumnRef, Returning};
///
/// let ids = Returning::columns([ColumnRef::from_static("id")]);
/// assert!(ids.is_some());
/// assert!(!Returning::None.is_some());
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub enum Returning {
    /// No `RETURNING` clause.
    #[default]
    None,
    /// `RETURNING *`.
    All,
    /// `RETURNING a, b, …`.
    Items(Vec<SelectItem>),
}

impl Returning {
    /// `RETURNING` the given columns.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Returning};
    ///
    /// let r = Returning::columns([ColumnRef::from_static("id")]);
    /// assert!(matches!(r, Returning::Items(_)));
    /// ```
    #[must_use]
    pub fn columns(columns: impl IntoIterator<Item = ColumnRef>) -> Self {
        Self::Items(columns.into_iter().map(SelectItem::column).collect())
    }

    /// `RETURNING` the given expressions.
    ///
    /// ```
    /// use moso_sql::{Expr, Returning, SelectItem};
    ///
    /// let r = Returning::items([SelectItem::expr(Expr::value(1))]);
    /// assert!(matches!(r, Returning::Items(_)));
    /// ```
    #[must_use]
    pub fn items(items: impl IntoIterator<Item = SelectItem>) -> Self {
        Self::Items(items.into_iter().collect())
    }

    /// Whether the statement returns rows.
    ///
    /// ```
    /// assert!(!moso_sql::Returning::None.is_some());
    /// assert!(moso_sql::Returning::All.is_some());
    /// ```
    #[must_use]
    pub const fn is_some(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// One `column = expression` of a `SET` list.
///
/// ```
/// use moso_sql::{Assignment, Expr, Ident};
///
/// let a = Assignment::new(Ident::from_static("name"), Expr::value("Ada"));
/// assert_eq!(a.column().as_str(), "name");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Assignment {
    column: Ident,
    value: Expr,
}

impl Assignment {
    /// Assigns an expression to a column.
    ///
    /// ```
    /// # use moso_sql::{Assignment, Expr, Ident};
    /// assert_eq!(Assignment::new(Ident::from_static("a"), Expr::null()).column().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn new(column: Ident, value: Expr) -> Self {
        Self { column, value }
    }

    /// Assigns a bound value to a column.
    ///
    /// ```
    /// # use moso_sql::{Assignment, Ident};
    /// assert_eq!(Assignment::set(Ident::from_static("a"), 1_i32).column().as_str(), "a");
    /// ```
    #[must_use]
    pub fn set(column: Ident, value: impl Bindable) -> Self {
        Self {
            column,
            value: Expr::Value(value.into_value()),
        }
    }

    /// The column being assigned to.
    ///
    /// ```
    /// # use moso_sql::{Assignment, Expr, Ident};
    /// assert_eq!(Assignment::new(Ident::from_static("a"), Expr::null()).column().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn column(&self) -> &Ident {
        &self.column
    }

    /// The value being assigned.
    ///
    /// ```
    /// # use moso_sql::{Assignment, Expr, Ident};
    /// let a = Assignment::new(Ident::from_static("a"), Expr::null());
    /// assert_eq!(a.value(), &Expr::null());
    /// ```
    #[must_use]
    pub const fn value(&self) -> &Expr {
        &self.value
    }
}

/// A whole statement written as raw SQL, with bound parameters.
///
/// This is the statement half of non-negotiable N8, and what `moso::sql!`
/// produces. The placeholder convention is the same as
/// [`RawExpr`](crate::RawExpr)'s: `?` is a placeholder, `??` is a literal
/// question mark, and the dialect renumbers them.
///
/// ```
/// use moso_sql::RawStatement;
///
/// let statement = RawStatement::new("select * from users where email = ?").bind("ada@example.com");
/// assert_eq!(statement.placeholder_count(), 1);
/// assert_eq!(statement.args().len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct RawStatement {
    text: String,
    args: Vec<Value>,
    read_only: bool,
}

impl RawStatement {
    /// A raw statement with no bound values yet.
    ///
    /// It is assumed to write, so it is never routed to a replica. Say
    /// [`RawStatement::read_only`] when it does not.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawStatement::new("select 1").text(), "select 1");
    /// ```
    #[must_use]
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            args: Vec::new(),
            read_only: false,
        }
    }

    /// A raw statement with its values.
    ///
    /// ```
    /// use moso_sql::{RawStatement, Value};
    ///
    /// let s = RawStatement::with_args("select ?", [Value::I32(1)]);
    /// assert_eq!(s.args().len(), 1);
    /// ```
    #[must_use]
    pub fn with_args(text: impl Into<String>, args: impl IntoIterator<Item = Value>) -> Self {
        Self {
            text: text.into(),
            args: args.into_iter().collect(),
            read_only: false,
        }
    }

    /// Binds one more value, in placeholder order.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawStatement::new("select ?").bind(1).args().len(), 1);
    /// ```
    #[must_use]
    pub fn bind(mut self, value: impl Bindable) -> Self {
        self.args.push(value.into_value());
        self
    }

    /// Binds one more already-built [`Value`].
    ///
    /// ```
    /// use moso_sql::{RawStatement, Value};
    ///
    /// assert_eq!(RawStatement::new("select ?").bind_value(Value::I32(1)).args().len(), 1);
    /// ```
    #[must_use]
    pub fn bind_value(mut self, value: Value) -> Self {
        self.args.push(value);
        self
    }

    /// Declares that the statement only reads, so it may go to a replica.
    ///
    /// ```
    /// assert!(moso_sql::RawStatement::new("select 1").read_only().is_read_only());
    /// ```
    #[must_use]
    pub const fn read_only(mut self) -> Self {
        self.read_only = true;
        self
    }

    /// The statement text, with its placeholders unexpanded.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawStatement::new("select 1").text(), "select 1");
    /// ```
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The bound values, in placeholder order.
    ///
    /// ```
    /// assert!(moso_sql::RawStatement::new("select 1").args().is_empty());
    /// ```
    #[must_use]
    pub fn args(&self) -> &[Value] {
        &self.args
    }

    /// Whether the caller declared the statement read-only.
    ///
    /// ```
    /// assert!(!moso_sql::RawStatement::new("select 1").is_read_only());
    /// ```
    #[must_use]
    pub const fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// How many placeholders the text has, counting `??` as a literal question
    /// mark rather than as one.
    ///
    /// ```
    /// assert_eq!(moso_sql::RawStatement::new("select ? , ??").placeholder_count(), 1);
    /// ```
    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        crate::expr::RawExpr::new(self.text.clone()).placeholder_count()
    }

    /// Wraps the statement as a [`Statement`].
    ///
    /// ```
    /// use moso_sql::{RawStatement, Statement};
    ///
    /// assert!(matches!(RawStatement::new("select 1").into_statement(), Statement::Raw(_)));
    /// ```
    #[must_use]
    pub fn into_statement(self) -> Statement {
        Statement::Raw(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_raw_statement_is_a_write_until_it_says_otherwise() {
        let raw = RawStatement::new("select 1");
        assert!(!raw.is_read_only());
        assert!(raw.read_only().is_read_only());
        assert!(!Statement::Raw(RawStatement::new("select 1")).is_read_only());
    }

    #[test]
    fn borrowing_keeps_the_kind() {
        let statement = Select::new().into_statement();
        assert_eq!(statement.borrowed().kind(), StatementKind::Select);
        assert_eq!(statement.kind().as_str(), "SELECT");
    }
}
