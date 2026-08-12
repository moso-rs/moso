//! `SELECT`: the statement, and everything that only appears inside one.

use crate::dialect::Dialect;
use crate::error::Error;
use crate::expr::{Expr, WindowSpec};
use crate::ident::{ColumnRef, Ident, TableRef};
use crate::order::OrderTerm;
use crate::sql::Sql;
use crate::statement::{Statement, StatementRef};

/// A `SELECT` statement.
///
/// # Shape stability (ADR-0007, non-negotiable N1)
///
/// Every combinator takes and returns `Select`. Clauses accumulate in runtime
/// vectors, never in the type, so a query built in a loop and a query built in
/// one chain have the same type and no error message ever prints a generic
/// tower. `moso-orm`'s `Select<E>` wraps this and adds the entity's type back
/// on top, and that is the *only* type parameter a user sees.
///
/// ```
/// use moso_sql::{Expr, Ident, Select, TableRef};
///
/// let mut query = Select::from_table(TableRef::from_static("users")).select_all();
/// for name in ["is_admin", "is_active"] {
///     query = query.filter(Expr::col(Ident::new(name)?).eq(Expr::value(true)));
/// }
/// assert_eq!(query.filters().len(), 2);
/// # Ok::<(), moso_sql::IdentError>(())
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Select {
    with: Vec<Cte>,
    recursive: bool,
    distinct: Distinct,
    items: Vec<SelectItem>,
    from: Vec<FromItem>,
    joins: Vec<Join>,
    filters: Vec<Expr>,
    group_by: Vec<Expr>,
    having: Vec<Expr>,
    windows: Vec<(Ident, WindowSpec)>,
    order_by: Vec<OrderTerm>,
    limit: Option<u64>,
    offset: Option<u64>,
    lock: Option<Lock>,
    set_ops: Vec<(SetOp, Box<Select>)>,
}

impl Select {
    /// A `SELECT` with no `FROM`, for `select 1` and for probes.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().from_items().is_empty());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            with: Vec::new(),
            recursive: false,
            distinct: Distinct::All,
            items: Vec::new(),
            from: Vec::new(),
            joins: Vec::new(),
            filters: Vec::new(),
            group_by: Vec::new(),
            having: Vec::new(),
            windows: Vec::new(),
            order_by: Vec::new(),
            limit: None,
            offset: None,
            lock: None,
            set_ops: Vec::new(),
        }
    }

    /// A `SELECT` over one table.
    ///
    /// ```
    /// use moso_sql::{Select, TableRef};
    ///
    /// assert_eq!(Select::from_table(TableRef::from_static("users")).from_items().len(), 1);
    /// ```
    #[must_use]
    pub fn from_table(table: TableRef) -> Self {
        Self::new().from(FromItem::table(table))
    }

    /// Adds a `FROM` item. More than one is a cross join.
    ///
    /// ```
    /// use moso_sql::{FromItem, Select, TableRef};
    ///
    /// let q = Select::new().from(FromItem::table(TableRef::from_static("a")));
    /// assert_eq!(q.from_items().len(), 1);
    /// ```
    #[must_use]
    pub fn from(mut self, item: FromItem) -> Self {
        self.from.push(item);
        self
    }

    /// Prepends a common table expression.
    ///
    /// ```
    /// use moso_sql::{Cte, Ident, Select, TableRef};
    ///
    /// let recent = Cte::new(
    ///     Ident::from_static("recent"),
    ///     Select::from_table(TableRef::from_static("posts")).select_all(),
    /// );
    /// let q = Select::from_table(TableRef::from_static("recent")).select_all().with(recent);
    /// assert_eq!(q.ctes().len(), 1);
    /// ```
    #[must_use]
    pub fn with(mut self, cte: Cte) -> Self {
        self.with.push(cte);
        self
    }

    /// Marks the `WITH` clause `RECURSIVE`.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().recursive(true).is_recursive());
    /// ```
    #[must_use]
    pub const fn recursive(mut self, recursive: bool) -> Self {
        self.recursive = recursive;
        self
    }

    /// `SELECT DISTINCT`.
    ///
    /// ```
    /// use moso_sql::{Distinct, Select};
    ///
    /// assert_eq!(Select::new().distinct().distinct_mode(), &Distinct::Distinct);
    /// ```
    #[must_use]
    pub fn distinct(mut self) -> Self {
        self.distinct = Distinct::Distinct;
        self
    }

    /// `SELECT DISTINCT ON (…)` — PostgreSQL only.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Select};
    ///
    /// let q = Select::new().distinct_on([Expr::col(Ident::from_static("author_id"))]);
    /// assert!(matches!(q.distinct_mode(), moso_sql::Distinct::On(_)));
    /// ```
    #[must_use]
    pub fn distinct_on(mut self, exprs: impl IntoIterator<Item = Expr>) -> Self {
        self.distinct = Distinct::On(exprs.into_iter().collect());
        self
    }

    /// Adds `*` to the projection.
    ///
    /// ```
    /// assert_eq!(moso_sql::Select::new().select_all().items().len(), 1);
    /// ```
    #[must_use]
    pub fn select_all(mut self) -> Self {
        self.items.push(SelectItem::All);
        self
    }

    /// Adds `qualifier.*` to the projection.
    ///
    /// ```
    /// use moso_sql::{Ident, Select};
    ///
    /// assert_eq!(Select::new().select_table_all(Ident::from_static("u")).items().len(), 1);
    /// ```
    #[must_use]
    pub fn select_table_all(mut self, qualifier: Ident) -> Self {
        self.items.push(SelectItem::AllFrom(qualifier));
        self
    }

    /// Adds a column to the projection.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Select};
    ///
    /// let q = Select::new().select_column(ColumnRef::from_static("id"));
    /// assert_eq!(q.items().len(), 1);
    /// ```
    #[must_use]
    pub fn select_column(mut self, column: ColumnRef) -> Self {
        self.items.push(SelectItem::expr(Expr::Column(column)));
        self
    }

    /// Adds an expression to the projection.
    ///
    /// ```
    /// use moso_sql::{Expr, Select};
    ///
    /// assert_eq!(Select::new().select_expr(Expr::value(1)).items().len(), 1);
    /// ```
    #[must_use]
    pub fn select_expr(mut self, expr: Expr) -> Self {
        self.items.push(SelectItem::expr(expr));
        self
    }

    /// Adds an aliased expression to the projection.
    ///
    /// ```
    /// use moso_sql::{Expr, Ident, Select};
    ///
    /// let q = Select::new().select_expr_as(Expr::value(1), Ident::from_static("one"));
    /// assert_eq!(q.items().len(), 1);
    /// ```
    #[must_use]
    pub fn select_expr_as(mut self, expr: Expr, alias: Ident) -> Self {
        self.items.push(SelectItem::aliased(expr, alias));
        self
    }

    /// Adds several projection items at once.
    ///
    /// ```
    /// use moso_sql::{ColumnRef, Select, SelectItem};
    ///
    /// let q = Select::new().select_items([
    ///     SelectItem::column(ColumnRef::from_static("id")),
    ///     SelectItem::column(ColumnRef::from_static("email")),
    /// ]);
    /// assert_eq!(q.items().len(), 2);
    /// ```
    #[must_use]
    pub fn select_items(mut self, items: impl IntoIterator<Item = SelectItem>) -> Self {
        self.items.extend(items);
        self
    }

    /// Replaces the whole projection.
    ///
    /// A count query is a full query with its projection swapped, which is why
    /// this exists rather than only additive combinators.
    ///
    /// ```
    /// use moso_sql::{Aggregate, Select, SelectItem};
    ///
    /// let q = Select::new().select_all()
    ///     .set_projection([SelectItem::expr(Aggregate::count_star().into_expr())]);
    /// assert_eq!(q.items().len(), 1);
    /// ```
    #[must_use]
    pub fn set_projection(mut self, items: impl IntoIterator<Item = SelectItem>) -> Self {
        self.items = items.into_iter().collect();
        self
    }

    /// Adds a join.
    ///
    /// ```
    /// use moso_sql::{Expr, FromItem, Ident, Join, JoinKind, Select, TableRef};
    ///
    /// let on = Expr::column(TableRef::from_static("posts").column(Ident::from_static("author_id")))
    ///     .eq(Expr::column(TableRef::from_static("users").column(Ident::from_static("id"))));
    /// let q = Select::from_table(TableRef::from_static("users"))
    ///     .join(Join::new(JoinKind::Left, FromItem::table(TableRef::from_static("posts")), on));
    /// assert_eq!(q.joins().len(), 1);
    /// ```
    #[must_use]
    pub fn join(mut self, join: Join) -> Self {
        self.joins.push(join);
        self
    }

    /// `INNER JOIN source ON condition`.
    ///
    /// ```
    /// # use moso_sql::{Expr, FromItem, Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a"))
    ///     .inner_join(FromItem::table(TableRef::from_static("b")), Expr::value(true));
    /// assert_eq!(q.joins().len(), 1);
    /// ```
    #[must_use]
    pub fn inner_join(self, source: FromItem, condition: Expr) -> Self {
        self.join(Join::new(JoinKind::Inner, source, condition))
    }

    /// `LEFT JOIN source ON condition`.
    ///
    /// ```
    /// # use moso_sql::{Expr, FromItem, Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a"))
    ///     .left_join(FromItem::table(TableRef::from_static("b")), Expr::value(true));
    /// assert_eq!(q.joins().len(), 1);
    /// ```
    #[must_use]
    pub fn left_join(self, source: FromItem, condition: Expr) -> Self {
        self.join(Join::new(JoinKind::Left, source, condition))
    }

    /// `RIGHT JOIN source ON condition`.
    ///
    /// ```
    /// # use moso_sql::{Expr, FromItem, Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a"))
    ///     .right_join(FromItem::table(TableRef::from_static("b")), Expr::value(true));
    /// assert_eq!(q.joins().len(), 1);
    /// ```
    #[must_use]
    pub fn right_join(self, source: FromItem, condition: Expr) -> Self {
        self.join(Join::new(JoinKind::Right, source, condition))
    }

    /// `FULL OUTER JOIN source ON condition`.
    ///
    /// ```
    /// # use moso_sql::{Expr, FromItem, Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a"))
    ///     .full_join(FromItem::table(TableRef::from_static("b")), Expr::value(true));
    /// assert_eq!(q.joins().len(), 1);
    /// ```
    #[must_use]
    pub fn full_join(self, source: FromItem, condition: Expr) -> Self {
        self.join(Join::new(JoinKind::Full, source, condition))
    }

    /// `CROSS JOIN source`.
    ///
    /// ```
    /// # use moso_sql::{FromItem, Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a"))
    ///     .cross_join(FromItem::table(TableRef::from_static("b")));
    /// assert_eq!(q.joins().len(), 1);
    /// ```
    #[must_use]
    pub fn cross_join(self, source: FromItem) -> Self {
        self.join(Join::cross(source))
    }

    /// Adds a predicate, `AND`-ed with the ones already there.
    ///
    /// ```
    /// # use moso_sql::{Expr, Select};
    /// assert_eq!(Select::new().filter(Expr::value(true)).filters().len(), 1);
    /// ```
    #[must_use]
    pub fn filter(mut self, expr: Expr) -> Self {
        self.filters.push(expr);
        self
    }

    /// Adds a predicate only if there is one — the ergonomic core of dynamic
    /// queries (non-negotiable N4).
    ///
    /// ```
    /// # use moso_sql::{Expr, Select};
    /// let q = Select::new().filter_opt(None).filter_opt(Some(Expr::value(true)));
    /// assert_eq!(q.filters().len(), 1);
    /// ```
    #[must_use]
    pub fn filter_opt(self, expr: Option<Expr>) -> Self {
        match expr {
            Some(expr) => self.filter(expr),
            None => self,
        }
    }

    /// Adds a predicate only if `condition` holds, without evaluating the
    /// closure otherwise.
    ///
    /// ```
    /// # use moso_sql::{Expr, Select};
    /// let q = Select::new().filter_if(false, || Expr::value(true));
    /// assert!(q.filters().is_empty());
    /// ```
    #[must_use]
    pub fn filter_if(self, condition: bool, expr: impl FnOnce() -> Expr) -> Self {
        if condition { self.filter(expr()) } else { self }
    }

    /// Applies a transformation only if `condition` holds.
    ///
    /// ```
    /// # use moso_sql::Select;
    /// let q = Select::new().when(true, Select::distinct);
    /// assert_eq!(q.distinct_mode(), &moso_sql::Distinct::Distinct);
    /// ```
    #[must_use]
    pub fn when(self, condition: bool, transform: impl FnOnce(Self) -> Self) -> Self {
        if condition { transform(self) } else { self }
    }

    /// Applies a transformation. The plain form of [`Select::when`], for named
    /// reusable scopes.
    ///
    /// ```
    /// # use moso_sql::Select;
    /// assert_eq!(Select::new().apply(Select::distinct).distinct_mode(), &moso_sql::Distinct::Distinct);
    /// ```
    #[must_use]
    pub fn apply(self, transform: impl FnOnce(Self) -> Self) -> Self {
        transform(self)
    }

    /// Adds a `GROUP BY` expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, Select};
    /// let q = Select::new().group_by(Expr::col(Ident::from_static("author_id")));
    /// assert_eq!(q.group_by_exprs().len(), 1);
    /// ```
    #[must_use]
    pub fn group_by(mut self, expr: Expr) -> Self {
        self.group_by.push(expr);
        self
    }

    /// Adds a `HAVING` predicate, `AND`-ed with the ones already there.
    ///
    /// ```
    /// # use moso_sql::{Expr, Select};
    /// assert_eq!(Select::new().having(Expr::value(true)).having_exprs().len(), 1);
    /// ```
    #[must_use]
    pub fn having(mut self, expr: Expr) -> Self {
        self.having.push(expr);
        self
    }

    /// Declares a named window, so several calls can share one specification.
    ///
    /// ```
    /// # use moso_sql::{Ident, Select, WindowSpec};
    /// let q = Select::new().window(Ident::from_static("w"), WindowSpec::new());
    /// assert_eq!(q.windows().len(), 1);
    /// ```
    #[must_use]
    pub fn window(mut self, name: Ident, spec: WindowSpec) -> Self {
        self.windows.push((name, spec));
        self
    }

    /// Adds an `ORDER BY` term.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, OrderTerm, Select};
    /// let q = Select::new().order_by(OrderTerm::asc(Expr::col(Ident::from_static("id"))));
    /// assert_eq!(q.order_terms().len(), 1);
    /// ```
    #[must_use]
    pub fn order_by(mut self, term: OrderTerm) -> Self {
        self.order_by.push(term);
        self
    }

    /// Sets `LIMIT`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Select::new().limit(10).limit_value(), Some(10));
    /// ```
    #[must_use]
    pub const fn limit(mut self, rows: u64) -> Self {
        self.limit = Some(rows);
        self
    }

    /// Sets `OFFSET`.
    ///
    /// ```
    /// assert_eq!(moso_sql::Select::new().offset(20).offset_value(), Some(20));
    /// ```
    #[must_use]
    pub const fn offset(mut self, rows: u64) -> Self {
        self.offset = Some(rows);
        self
    }

    /// Sets the row-level lock.
    ///
    /// ```
    /// use moso_sql::{Lock, LockStrength, Select};
    ///
    /// let q = Select::new().lock(Lock::new(LockStrength::Update).skip_locked());
    /// assert!(q.lock_mode().is_some());
    /// ```
    #[must_use]
    pub fn lock(mut self, lock: Lock) -> Self {
        self.lock = Some(lock);
        self
    }

    /// Combines this query with another through a set operation.
    ///
    /// ```
    /// use moso_sql::{Select, SetOp, TableRef};
    ///
    /// let q = Select::from_table(TableRef::from_static("a")).select_all()
    ///     .set_op(SetOp::UnionAll, Select::from_table(TableRef::from_static("b")).select_all());
    /// assert_eq!(q.set_operations().len(), 1);
    /// ```
    #[must_use]
    pub fn set_op(mut self, op: SetOp, other: Select) -> Self {
        self.set_ops.push((op, Box::new(other)));
        self
    }

    /// `UNION` — deduplicating.
    ///
    /// ```
    /// # use moso_sql::{Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a")).union(Select::new());
    /// assert_eq!(q.set_operations().len(), 1);
    /// ```
    #[must_use]
    pub fn union(self, other: Select) -> Self {
        self.set_op(SetOp::Union, other)
    }

    /// `UNION ALL` — keeping duplicates, and the one a recursive CTE needs.
    ///
    /// ```
    /// # use moso_sql::{Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a")).union_all(Select::new());
    /// assert_eq!(q.set_operations().len(), 1);
    /// ```
    #[must_use]
    pub fn union_all(self, other: Select) -> Self {
        self.set_op(SetOp::UnionAll, other)
    }

    /// `INTERSECT`.
    ///
    /// ```
    /// # use moso_sql::{Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a")).intersect(Select::new());
    /// assert_eq!(q.set_operations().len(), 1);
    /// ```
    #[must_use]
    pub fn intersect(self, other: Select) -> Self {
        self.set_op(SetOp::Intersect, other)
    }

    /// `EXCEPT`.
    ///
    /// ```
    /// # use moso_sql::{Select, TableRef};
    /// let q = Select::from_table(TableRef::from_static("a")).except(Select::new());
    /// assert_eq!(q.set_operations().len(), 1);
    /// ```
    #[must_use]
    pub fn except(self, other: Select) -> Self {
        self.set_op(SetOp::Except, other)
    }

    /// Drops the `ORDER BY`. A `count(*)` wrapper must, because ordering a
    /// counted subquery is wasted work and PostgreSQL rejects it in some
    /// positions.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, OrderTerm, Select};
    /// let q = Select::new().order_by(OrderTerm::asc(Expr::col(Ident::from_static("id"))));
    /// assert!(q.clear_order_by().order_terms().is_empty());
    /// ```
    #[must_use]
    pub fn clear_order_by(mut self) -> Self {
        self.order_by.clear();
        self
    }

    /// Drops `LIMIT` and `OFFSET`.
    ///
    /// ```
    /// # use moso_sql::Select;
    /// assert_eq!(Select::new().limit(1).clear_limit().limit_value(), None);
    /// ```
    #[must_use]
    pub const fn clear_limit(mut self) -> Self {
        self.limit = None;
        self.offset = None;
        self
    }

    /// The common table expressions.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().ctes().is_empty());
    /// ```
    #[must_use]
    pub fn ctes(&self) -> &[Cte] {
        &self.with
    }

    /// Whether the `WITH` clause is `RECURSIVE`.
    ///
    /// ```
    /// assert!(!moso_sql::Select::new().is_recursive());
    /// ```
    #[must_use]
    pub const fn is_recursive(&self) -> bool {
        self.recursive
    }

    /// The `DISTINCT` mode.
    ///
    /// ```
    /// use moso_sql::{Distinct, Select};
    ///
    /// assert_eq!(Select::new().distinct_mode(), &Distinct::All);
    /// ```
    #[must_use]
    pub const fn distinct_mode(&self) -> &Distinct {
        &self.distinct
    }

    /// The projection.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().items().is_empty());
    /// ```
    #[must_use]
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// The `FROM` items.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().from_items().is_empty());
    /// ```
    #[must_use]
    pub fn from_items(&self) -> &[FromItem] {
        &self.from
    }

    /// The joins.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().joins().is_empty());
    /// ```
    #[must_use]
    pub fn joins(&self) -> &[Join] {
        &self.joins
    }

    /// The `WHERE` predicates, which are `AND`-ed together.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().filters().is_empty());
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Expr] {
        &self.filters
    }

    /// The `GROUP BY` expressions.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().group_by_exprs().is_empty());
    /// ```
    #[must_use]
    pub fn group_by_exprs(&self) -> &[Expr] {
        &self.group_by
    }

    /// The `HAVING` predicates.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().having_exprs().is_empty());
    /// ```
    #[must_use]
    pub fn having_exprs(&self) -> &[Expr] {
        &self.having
    }

    /// The named windows.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().windows().is_empty());
    /// ```
    #[must_use]
    pub fn windows(&self) -> &[(Ident, WindowSpec)] {
        &self.windows
    }

    /// The `ORDER BY` terms.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().order_terms().is_empty());
    /// ```
    #[must_use]
    pub fn order_terms(&self) -> &[OrderTerm] {
        &self.order_by
    }

    /// The `LIMIT`, if set.
    ///
    /// ```
    /// assert_eq!(moso_sql::Select::new().limit_value(), None);
    /// ```
    #[must_use]
    pub const fn limit_value(&self) -> Option<u64> {
        self.limit
    }

    /// The `OFFSET`, if set.
    ///
    /// ```
    /// assert_eq!(moso_sql::Select::new().offset_value(), None);
    /// ```
    #[must_use]
    pub const fn offset_value(&self) -> Option<u64> {
        self.offset
    }

    /// The row-level lock, if set.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().lock_mode().is_none());
    /// ```
    #[must_use]
    pub const fn lock_mode(&self) -> Option<&Lock> {
        self.lock.as_ref()
    }

    /// The set operations applied to this query.
    ///
    /// ```
    /// assert!(moso_sql::Select::new().set_operations().is_empty());
    /// ```
    #[must_use]
    pub fn set_operations(&self) -> &[(SetOp, Box<Select>)] {
        &self.set_ops
    }

    /// Renders the statement for a dialect.
    ///
    /// # Errors
    ///
    /// [`Error`] if the query is incomplete (no projection, no `FROM` where
    /// one is required) or uses a construct the dialect does not have.
    ///
    /// ```
    /// use moso_sql::{Postgres, Select, TableRef};
    ///
    /// let sql = Select::from_table(TableRef::from_static("users")).select_all().build(&Postgres)?;
    /// assert_eq!(sql.text, r#"SELECT * FROM "users""#);
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    pub fn build(&self, dialect: &dyn Dialect) -> Result<Sql, Error> {
        dialect.build(StatementRef::Select(self))
    }

    /// Wraps the query as a [`Statement`].
    ///
    /// ```
    /// use moso_sql::{Select, Statement};
    ///
    /// assert!(matches!(Select::new().into_statement(), Statement::Select(_)));
    /// ```
    #[must_use]
    pub fn into_statement(self) -> Statement {
        Statement::Select(self)
    }
}

/// One item of a `FROM` clause.
///
/// A common table expression is referenced as an ordinary table, by the name
/// it was declared with.
///
/// ```
/// use moso_sql::{FromItem, Ident, TableRef};
///
/// let aliased = FromItem::table_as(TableRef::from_static("users"), Ident::from_static("u"));
/// assert_eq!(aliased.alias().map(Ident::as_str), Some("u"));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum FromItem {
    /// A table, optionally aliased.
    Table {
        /// The table.
        table: TableRef,
        /// Its alias in this query.
        alias: Option<Ident>,
        /// `FROM ONLY t` — do not descend into partitions or child tables.
        only: bool,
    },
    /// A subquery. An alias is mandatory, as it is in the SQL standard.
    Subquery {
        /// The subquery.
        query: Box<Select>,
        /// Its alias.
        alias: Ident,
        /// `LATERAL` — the subquery may refer to columns of the items to its
        /// left, which is what makes a per-parent `LIMIT` expressible.
        lateral: bool,
    },
    /// A `VALUES` list used as a table, which is how a batched update sends
    /// one row per target row in a single statement.
    Values {
        /// The rows.
        rows: Vec<Vec<Expr>>,
        /// The table alias.
        alias: Ident,
        /// The column names.
        columns: Vec<Ident>,
    },
    /// A set-returning function, such as `unnest(…)` or `jsonb_to_recordset(…)`.
    Function {
        /// The function call.
        function: crate::expr::Function,
        /// Its alias.
        alias: Option<Ident>,
        /// `LATERAL`.
        lateral: bool,
        /// `WITH ORDINALITY` — add a row-number column.
        with_ordinality: bool,
    },
}

impl FromItem {
    /// A plain table.
    ///
    /// ```
    /// use moso_sql::{FromItem, TableRef};
    ///
    /// assert!(FromItem::table(TableRef::from_static("t")).alias().is_none());
    /// ```
    #[must_use]
    pub const fn table(table: TableRef) -> Self {
        Self::Table {
            table,
            alias: None,
            only: false,
        }
    }

    /// An aliased table.
    ///
    /// ```
    /// use moso_sql::{FromItem, Ident, TableRef};
    ///
    /// let item = FromItem::table_as(TableRef::from_static("t"), Ident::from_static("x"));
    /// assert_eq!(item.alias().map(Ident::as_str), Some("x"));
    /// ```
    #[must_use]
    pub const fn table_as(table: TableRef, alias: Ident) -> Self {
        Self::Table {
            table,
            alias: Some(alias),
            only: false,
        }
    }

    /// `FROM ONLY t` — the parent of a partitioned table, without its
    /// partitions.
    ///
    /// ```
    /// use moso_sql::{FromItem, TableRef};
    ///
    /// assert!(matches!(FromItem::only(TableRef::from_static("t")), FromItem::Table { only: true, .. }));
    /// ```
    #[must_use]
    pub const fn only(table: TableRef) -> Self {
        Self::Table {
            table,
            alias: None,
            only: true,
        }
    }

    /// An aliased subquery.
    ///
    /// ```
    /// use moso_sql::{FromItem, Ident, Select};
    ///
    /// let item = FromItem::subquery(Select::new(), Ident::from_static("s"));
    /// assert_eq!(item.alias().map(Ident::as_str), Some("s"));
    /// ```
    #[must_use]
    pub fn subquery(query: Select, alias: Ident) -> Self {
        Self::Subquery {
            query: Box::new(query),
            alias,
            lateral: false,
        }
    }

    /// A `LATERAL` subquery, which may refer to the items to its left.
    ///
    /// ```
    /// use moso_sql::{FromItem, Ident, Select};
    ///
    /// let item = FromItem::lateral(Select::new(), Ident::from_static("s"));
    /// assert!(matches!(item, FromItem::Subquery { lateral: true, .. }));
    /// ```
    #[must_use]
    pub fn lateral(query: Select, alias: Ident) -> Self {
        Self::Subquery {
            query: Box::new(query),
            alias,
            lateral: true,
        }
    }

    /// A `VALUES` list used as a table.
    ///
    /// ```
    /// use moso_sql::{Expr, FromItem, Ident};
    ///
    /// let item = FromItem::values(
    ///     [vec![Expr::value(1), Expr::value("a")]],
    ///     Ident::from_static("v"),
    ///     [Ident::from_static("id"), Ident::from_static("name")],
    /// );
    /// assert_eq!(item.alias().map(Ident::as_str), Some("v"));
    /// ```
    #[must_use]
    pub fn values(
        rows: impl IntoIterator<Item = Vec<Expr>>,
        alias: Ident,
        columns: impl IntoIterator<Item = Ident>,
    ) -> Self {
        Self::Values {
            rows: rows.into_iter().collect(),
            alias,
            columns: columns.into_iter().collect(),
        }
    }

    /// A set-returning function call.
    ///
    /// ```
    /// use moso_sql::{Function, FromItem, Ident};
    ///
    /// let item = FromItem::function(
    ///     Function::custom(Ident::from_static("unnest"), []),
    ///     Some(Ident::from_static("u")),
    /// );
    /// assert_eq!(item.alias().map(Ident::as_str), Some("u"));
    /// ```
    #[must_use]
    pub const fn function(function: crate::expr::Function, alias: Option<Ident>) -> Self {
        Self::Function {
            function,
            alias,
            lateral: false,
            with_ordinality: false,
        }
    }

    /// The alias, if the item has one.
    ///
    /// ```
    /// use moso_sql::{FromItem, TableRef};
    ///
    /// assert!(FromItem::table(TableRef::from_static("t")).alias().is_none());
    /// ```
    #[must_use]
    pub const fn alias(&self) -> Option<&Ident> {
        match self {
            Self::Table { alias, .. } | Self::Function { alias, .. } => alias.as_ref(),
            Self::Subquery { alias, .. } | Self::Values { alias, .. } => Some(alias),
        }
    }

    /// Whether the item is `LATERAL`.
    ///
    /// ```
    /// use moso_sql::{FromItem, Ident, Select};
    ///
    /// assert!(FromItem::lateral(Select::new(), Ident::from_static("s")).is_lateral());
    /// ```
    #[must_use]
    pub const fn is_lateral(&self) -> bool {
        match self {
            Self::Subquery { lateral, .. } | Self::Function { lateral, .. } => *lateral,
            _ => false,
        }
    }
}

/// A join: a kind, a source, and a condition.
///
/// ```
/// use moso_sql::{Expr, FromItem, Join, JoinCondition, JoinKind, TableRef};
///
/// let join = Join::new(JoinKind::Left, FromItem::table(TableRef::from_static("posts")), Expr::value(true));
/// assert_eq!(join.kind(), JoinKind::Left);
/// assert!(matches!(join.condition(), JoinCondition::On(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Join {
    kind: JoinKind,
    source: FromItem,
    condition: JoinCondition,
}

impl Join {
    /// A join with an `ON` condition.
    ///
    /// ```
    /// # use moso_sql::{Expr, FromItem, Join, JoinKind, TableRef};
    /// let j = Join::new(JoinKind::Inner, FromItem::table(TableRef::from_static("t")), Expr::value(true));
    /// assert_eq!(j.kind(), JoinKind::Inner);
    /// ```
    #[must_use]
    pub const fn new(kind: JoinKind, source: FromItem, condition: Expr) -> Self {
        Self {
            kind,
            source,
            condition: JoinCondition::On(condition),
        }
    }

    /// A join with a `USING (…)` condition.
    ///
    /// ```
    /// # use moso_sql::{FromItem, Ident, Join, JoinKind, TableRef};
    /// let j = Join::using(
    ///     JoinKind::Inner,
    ///     FromItem::table(TableRef::from_static("t")),
    ///     [Ident::from_static("id")],
    /// );
    /// assert!(matches!(j.condition(), moso_sql::JoinCondition::Using(_)));
    /// ```
    #[must_use]
    pub fn using(
        kind: JoinKind,
        source: FromItem,
        columns: impl IntoIterator<Item = Ident>,
    ) -> Self {
        Self {
            kind,
            source,
            condition: JoinCondition::Using(columns.into_iter().collect()),
        }
    }

    /// A `CROSS JOIN`, which has no condition.
    ///
    /// ```
    /// # use moso_sql::{FromItem, Join, JoinKind, TableRef};
    /// let j = Join::cross(FromItem::table(TableRef::from_static("t")));
    /// assert_eq!(j.kind(), JoinKind::Cross);
    /// ```
    #[must_use]
    pub const fn cross(source: FromItem) -> Self {
        Self {
            kind: JoinKind::Cross,
            source,
            condition: JoinCondition::None,
        }
    }

    /// The join kind.
    ///
    /// ```
    /// # use moso_sql::{FromItem, Join, JoinKind, TableRef};
    /// assert_eq!(Join::cross(FromItem::table(TableRef::from_static("t"))).kind(), JoinKind::Cross);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> JoinKind {
        self.kind
    }

    /// The joined source.
    ///
    /// ```
    /// # use moso_sql::{FromItem, Join, TableRef};
    /// let j = Join::cross(FromItem::table(TableRef::from_static("t")));
    /// assert!(j.source().alias().is_none());
    /// ```
    #[must_use]
    pub const fn source(&self) -> &FromItem {
        &self.source
    }

    /// The join condition.
    ///
    /// ```
    /// # use moso_sql::{FromItem, Join, JoinCondition, TableRef};
    /// let j = Join::cross(FromItem::table(TableRef::from_static("t")));
    /// assert!(matches!(j.condition(), JoinCondition::None));
    /// ```
    #[must_use]
    pub const fn condition(&self) -> &JoinCondition {
        &self.condition
    }
}

/// The kind of a join.
///
/// ```
/// use moso_sql::JoinKind;
///
/// assert!(JoinKind::Left.keeps_left_rows());
/// assert!(!JoinKind::Inner.keeps_left_rows());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum JoinKind {
    /// `INNER JOIN`.
    Inner,
    /// `LEFT OUTER JOIN`.
    Left,
    /// `RIGHT OUTER JOIN`. SQLite has supported this since 3.39.
    Right,
    /// `FULL OUTER JOIN`.
    Full,
    /// `CROSS JOIN`.
    Cross,
}

impl JoinKind {
    /// Whether rows on the left survive with no match on the right, which is
    /// what decides whether a preload may filter on the joined table.
    ///
    /// ```
    /// use moso_sql::JoinKind;
    ///
    /// assert!(JoinKind::Full.keeps_left_rows());
    /// ```
    #[must_use]
    pub const fn keeps_left_rows(self) -> bool {
        matches!(self, Self::Left | Self::Full)
    }
}

/// How a join matches rows.
///
/// ```
/// use moso_sql::{Expr, JoinCondition};
///
/// assert!(matches!(JoinCondition::On(Expr::value(true)), JoinCondition::On(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum JoinCondition {
    /// `ON <predicate>`.
    On(Expr),
    /// `USING (a, b)` — equality on identically named columns, and the joined
    /// column appears once in the result.
    Using(Vec<Ident>),
    /// `NATURAL` — join on every identically named column. Accepted for
    /// completeness; `moso-orm` never generates one, because adding a column
    /// silently changes the query.
    Natural,
    /// No condition, as in a `CROSS JOIN`.
    None,
}

/// One item of a `SELECT` list.
///
/// ```
/// use moso_sql::{ColumnRef, Expr, Ident, SelectItem};
///
/// let named = SelectItem::aliased(Expr::value(1), Ident::from_static("one"));
/// assert_eq!(named.alias().map(Ident::as_str), Some("one"));
/// assert!(SelectItem::column(ColumnRef::from_static("id")).alias().is_none());
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum SelectItem {
    /// `*`.
    All,
    /// `qualifier.*`.
    AllFrom(Ident),
    /// An expression, optionally aliased.
    Expr {
        /// The expression.
        expr: Expr,
        /// Its output name.
        alias: Option<Ident>,
    },
}

impl SelectItem {
    /// An unaliased expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, SelectItem};
    /// assert!(SelectItem::expr(Expr::value(1)).alias().is_none());
    /// ```
    #[must_use]
    pub const fn expr(expr: Expr) -> Self {
        Self::Expr { expr, alias: None }
    }

    /// A column.
    ///
    /// ```
    /// # use moso_sql::{ColumnRef, SelectItem};
    /// assert!(SelectItem::column(ColumnRef::from_static("id")).alias().is_none());
    /// ```
    #[must_use]
    pub const fn column(column: ColumnRef) -> Self {
        Self::Expr {
            expr: Expr::Column(column),
            alias: None,
        }
    }

    /// An aliased expression.
    ///
    /// ```
    /// # use moso_sql::{Expr, Ident, SelectItem};
    /// let item = SelectItem::aliased(Expr::value(1), Ident::from_static("one"));
    /// assert_eq!(item.alias().map(Ident::as_str), Some("one"));
    /// ```
    #[must_use]
    pub const fn aliased(expr: Expr, alias: Ident) -> Self {
        Self::Expr {
            expr,
            alias: Some(alias),
        }
    }

    /// The output name, if the item has one.
    ///
    /// ```
    /// # use moso_sql::SelectItem;
    /// assert!(SelectItem::All.alias().is_none());
    /// ```
    #[must_use]
    pub const fn alias(&self) -> Option<&Ident> {
        match self {
            Self::Expr { alias, .. } => alias.as_ref(),
            _ => None,
        }
    }
}

/// The `DISTINCT` modifier.
///
/// ```
/// use moso_sql::Distinct;
///
/// assert_eq!(Distinct::default(), Distinct::All);
/// ```
#[derive(Clone, Debug, Default, PartialEq)]
#[non_exhaustive]
pub enum Distinct {
    /// `ALL` — the default; duplicates are kept.
    #[default]
    All,
    /// `DISTINCT`.
    Distinct,
    /// `DISTINCT ON (…)` — PostgreSQL only. Keeps the first row of each group
    /// in `ORDER BY` order.
    On(Vec<Expr>),
}

/// A row-level lock.
///
/// ```
/// use moso_sql::{Lock, LockBehavior, LockStrength};
///
/// // The job-queue idiom: take the next unlocked row, never wait.
/// let claim = Lock::new(LockStrength::Update).skip_locked();
/// assert_eq!(claim.behavior(), LockBehavior::SkipLocked);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Lock {
    strength: LockStrength,
    behavior: LockBehavior,
    of: Vec<TableRef>,
}

impl Lock {
    /// A lock of the given strength that waits for conflicting locks.
    ///
    /// ```
    /// use moso_sql::{Lock, LockBehavior, LockStrength};
    ///
    /// assert_eq!(Lock::new(LockStrength::Share).behavior(), LockBehavior::Wait);
    /// ```
    #[must_use]
    pub const fn new(strength: LockStrength) -> Self {
        Self {
            strength,
            behavior: LockBehavior::Wait,
            of: Vec::new(),
        }
    }

    /// `SKIP LOCKED` — pass over rows another transaction holds.
    ///
    /// ```
    /// # use moso_sql::{Lock, LockBehavior, LockStrength};
    /// assert_eq!(Lock::new(LockStrength::Update).skip_locked().behavior(), LockBehavior::SkipLocked);
    /// ```
    #[must_use]
    pub fn skip_locked(mut self) -> Self {
        self.behavior = LockBehavior::SkipLocked;
        self
    }

    /// `NOWAIT` — fail immediately rather than wait.
    ///
    /// ```
    /// # use moso_sql::{Lock, LockBehavior, LockStrength};
    /// assert_eq!(Lock::new(LockStrength::Update).nowait().behavior(), LockBehavior::NoWait);
    /// ```
    #[must_use]
    pub fn nowait(mut self) -> Self {
        self.behavior = LockBehavior::NoWait;
        self
    }

    /// `OF table` — restrict the lock to some of the joined tables.
    ///
    /// ```
    /// # use moso_sql::{Lock, LockStrength, TableRef};
    /// let lock = Lock::new(LockStrength::Update).of(TableRef::from_static("accounts"));
    /// assert_eq!(lock.tables().len(), 1);
    /// ```
    #[must_use]
    pub fn of(mut self, table: TableRef) -> Self {
        self.of.push(table);
        self
    }

    /// The lock strength.
    ///
    /// ```
    /// # use moso_sql::{Lock, LockStrength};
    /// assert_eq!(Lock::new(LockStrength::Update).strength(), LockStrength::Update);
    /// ```
    #[must_use]
    pub const fn strength(&self) -> LockStrength {
        self.strength
    }

    /// What to do when a row is already locked.
    ///
    /// ```
    /// # use moso_sql::{Lock, LockBehavior, LockStrength};
    /// assert_eq!(Lock::new(LockStrength::Update).behavior(), LockBehavior::Wait);
    /// ```
    #[must_use]
    pub const fn behavior(&self) -> LockBehavior {
        self.behavior
    }

    /// The tables the lock is restricted to.
    ///
    /// ```
    /// # use moso_sql::{Lock, LockStrength};
    /// assert!(Lock::new(LockStrength::Update).tables().is_empty());
    /// ```
    #[must_use]
    pub fn tables(&self) -> &[TableRef] {
        &self.of
    }
}

/// How strong a row-level lock is.
///
/// ```
/// use moso_sql::LockStrength;
///
/// assert!(LockStrength::Update.blocks_writers());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LockStrength {
    /// `FOR UPDATE` — blocks every other lock on the row.
    Update,
    /// `FOR NO KEY UPDATE` — weaker, and does not block a foreign-key check.
    NoKeyUpdate,
    /// `FOR SHARE` — blocks writers, allows other readers.
    Share,
    /// `FOR KEY SHARE` — weakest; blocks only key changes.
    KeyShare,
}

impl LockStrength {
    /// Whether the lock blocks another transaction from writing the row.
    ///
    /// ```
    /// use moso_sql::LockStrength;
    ///
    /// assert!(!LockStrength::KeyShare.blocks_writers());
    /// ```
    #[must_use]
    pub const fn blocks_writers(self) -> bool {
        matches!(self, Self::Update | Self::NoKeyUpdate | Self::Share)
    }
}

/// What a locking `SELECT` does when a row it wants is already locked.
///
/// ```
/// use moso_sql::LockBehavior;
///
/// assert_ne!(LockBehavior::Wait, LockBehavior::SkipLocked);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum LockBehavior {
    /// Wait for the other transaction. The default.
    #[default]
    Wait,
    /// Skip the row. The correct choice for a queue worker.
    SkipLocked,
    /// Fail immediately.
    NoWait,
}

/// A set operation between two queries.
///
/// ```
/// use moso_sql::SetOp;
///
/// assert!(SetOp::UnionAll.keeps_duplicates());
/// assert!(!SetOp::Union.keeps_duplicates());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SetOp {
    /// `UNION`.
    Union,
    /// `UNION ALL`.
    UnionAll,
    /// `INTERSECT`.
    Intersect,
    /// `INTERSECT ALL`.
    IntersectAll,
    /// `EXCEPT`.
    Except,
    /// `EXCEPT ALL`.
    ExceptAll,
}

impl SetOp {
    /// Whether the operation keeps duplicate rows.
    ///
    /// ```
    /// assert!(moso_sql::SetOp::ExceptAll.keeps_duplicates());
    /// ```
    #[must_use]
    pub const fn keeps_duplicates(self) -> bool {
        matches!(self, Self::UnionAll | Self::IntersectAll | Self::ExceptAll)
    }
}

/// A common table expression.
///
/// ```
/// use moso_sql::{Cte, Ident, Select, TableRef};
///
/// let cte = Cte::new(
///     Ident::from_static("recent"),
///     Select::from_table(TableRef::from_static("posts")).select_all(),
/// );
/// assert_eq!(cte.name().as_str(), "recent");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Cte {
    name: Ident,
    columns: Vec<Ident>,
    query: Box<Statement>,
    materialized: Option<bool>,
}

impl Cte {
    /// A CTE over a `SELECT`.
    ///
    /// ```
    /// use moso_sql::{Cte, Ident, Select};
    ///
    /// assert_eq!(Cte::new(Ident::from_static("c"), Select::new()).name().as_str(), "c");
    /// ```
    #[must_use]
    pub fn new(name: Ident, query: Select) -> Self {
        Self {
            name,
            columns: Vec::new(),
            query: Box::new(Statement::Select(query)),
            materialized: None,
        }
    }

    /// A CTE over any statement, which is how a data-modifying CTE moves rows
    /// between tables in one round trip.
    ///
    /// ```
    /// use moso_sql::{Cte, Delete, Ident, TableRef};
    ///
    /// let moved = Cte::from_statement(
    ///     Ident::from_static("deleted"),
    ///     Delete::from_table(TableRef::from_static("stale")).into_statement(),
    /// );
    /// assert_eq!(moved.name().as_str(), "deleted");
    /// ```
    #[must_use]
    pub fn from_statement(name: Ident, query: Statement) -> Self {
        Self {
            name,
            columns: Vec::new(),
            query: Box::new(query),
            materialized: None,
        }
    }

    /// Names the CTE's output columns.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select};
    /// let cte = Cte::new(Ident::from_static("c"), Select::new())
    ///     .columns([Ident::from_static("id")]);
    /// assert_eq!(cte.column_names().len(), 1);
    /// ```
    #[must_use]
    pub fn columns(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.columns = columns.into_iter().collect();
        self
    }

    /// Forces `MATERIALIZED` or `NOT MATERIALIZED` — PostgreSQL 12 and later.
    ///
    /// Worth setting explicitly on a CTE that is referenced once and filters
    /// heavily: the planner's default changed in 12 and the two behaviours
    /// differ by orders of magnitude.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select};
    /// let cte = Cte::new(Ident::from_static("c"), Select::new()).materialized(false);
    /// assert_eq!(cte.materialization(), Some(false));
    /// ```
    #[must_use]
    pub const fn materialized(mut self, materialized: bool) -> Self {
        self.materialized = Some(materialized);
        self
    }

    /// The CTE's name, which is how a `FROM` refers to it.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select};
    /// assert_eq!(Cte::new(Ident::from_static("c"), Select::new()).name().as_str(), "c");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The declared output column names.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select};
    /// assert!(Cte::new(Ident::from_static("c"), Select::new()).column_names().is_empty());
    /// ```
    #[must_use]
    pub fn column_names(&self) -> &[Ident] {
        &self.columns
    }

    /// The CTE's body.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select, Statement};
    /// let cte = Cte::new(Ident::from_static("c"), Select::new());
    /// assert!(matches!(cte.query(), Statement::Select(_)));
    /// ```
    #[must_use]
    pub fn query(&self) -> &Statement {
        &self.query
    }

    /// The materialisation override, if one was given.
    ///
    /// ```
    /// # use moso_sql::{Cte, Ident, Select};
    /// assert!(Cte::new(Ident::from_static("c"), Select::new()).materialization().is_none());
    /// ```
    #[must_use]
    pub const fn materialization(&self) -> Option<bool> {
        self.materialized
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_builder_is_shape_stable() {
        // N1: ten chained combinators, and the type never moves. If this
        // stops compiling, the builder grew a type parameter.
        fn assert_select(_: &Select) {}

        let query = Select::from_table(TableRef::from_static("users"))
            .select_all()
            .filter(Expr::value(true))
            .filter_opt(None)
            .filter_if(false, || Expr::value(true))
            .when(false, Select::distinct)
            .group_by(Expr::col(Ident::from_static("id")))
            .having(Expr::value(true))
            .order_by(OrderTerm::asc(Expr::col(Ident::from_static("id"))))
            .limit(10)
            .offset(20)
            .lock(Lock::new(LockStrength::Update));
        assert_select(&query);
        assert_eq!(query.filters().len(), 1);
        assert_eq!(query.limit_value(), Some(10));
    }

    #[test]
    fn dynamic_filters_accumulate_without_type_gymnastics() {
        let mut query = Select::from_table(TableRef::from_static("products")).select_all();
        for (apply, column) in [(true, "category_id"), (false, "brand_id"), (true, "stock")] {
            query = query.filter_if(apply, || {
                Expr::col(Ident::new(column).expect("literal")).is_not_null()
            });
        }
        assert_eq!(query.filters().len(), 2);
    }

    #[test]
    fn clearing_order_and_limit_leaves_the_rest_alone() {
        let query = Select::from_table(TableRef::from_static("t"))
            .select_all()
            .filter(Expr::value(true))
            .order_by(OrderTerm::asc(Expr::value(1)))
            .limit(5)
            .offset(5)
            .clear_order_by()
            .clear_limit();
        assert!(query.order_terms().is_empty());
        assert_eq!(query.limit_value(), None);
        assert_eq!(query.offset_value(), None);
        assert_eq!(query.filters().len(), 1);
    }
}
