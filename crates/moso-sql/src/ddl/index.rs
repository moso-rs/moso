//! `CREATE INDEX`, `DROP INDEX`, and the pieces that make an index definition
//! reproducible: method, operator class, sort order, `NULLS` placement,
//! `INCLUDE` columns and a partial predicate.

use crate::expr::Expr;
use crate::ident::{Ident, TableRef};
use crate::order::{Nulls, Order};

/// `CREATE INDEX`.
///
/// # `CONCURRENTLY` is the default a migration should want
///
/// A plain `CREATE INDEX` holds a lock that blocks every write to the table
/// for the whole build. `CONCURRENTLY` does not, at the cost of two table
/// scans and of not being runnable inside a transaction — which is why
/// [`Ddl::requires_no_transaction`](crate::ddl::Ddl::requires_no_transaction)
/// exists and why the migration runner has to ask.
///
/// ```
/// use moso_sql::ddl::{CreateIndex, IndexMethod, IndexTarget};
/// use moso_sql::{Expr, Ident, TableRef};
///
/// let active_emails = CreateIndex::new(
///     Ident::from_static("idx_users_email_active"),
///     TableRef::from_static("users"),
///     [IndexTarget::column(Ident::from_static("email"))],
/// )
/// .unique()
/// .concurrently()
/// .where_(Expr::col(Ident::from_static("deleted_at")).is_null());
/// assert!(active_emails.is_unique());
/// assert!(active_emails.is_concurrent());
/// assert!(active_emails.predicate().is_some());
/// assert_eq!(active_emails.method(), None::<&IndexMethod>);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CreateIndex {
    name: Ident,
    table: TableRef,
    targets: Vec<IndexTarget>,
    unique: bool,
    concurrently: bool,
    if_not_exists: bool,
    method: Option<IndexMethod>,
    include: Vec<Ident>,
    predicate: Option<Expr>,
    nulls_not_distinct: bool,
}

impl CreateIndex {
    /// An index over the given targets.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateIndex, IndexTarget}, Ident, TableRef};
    /// let index = CreateIndex::new(
    ///     Ident::from_static("i"),
    ///     TableRef::from_static("t"),
    ///     [IndexTarget::column(Ident::from_static("c"))],
    /// );
    /// assert_eq!(index.targets().len(), 1);
    /// ```
    #[must_use]
    pub fn new(
        name: Ident,
        table: TableRef,
        targets: impl IntoIterator<Item = IndexTarget>,
    ) -> Self {
        Self {
            name,
            table,
            targets: targets.into_iter().collect(),
            unique: false,
            concurrently: false,
            if_not_exists: false,
            method: None,
            include: Vec::new(),
            predicate: None,
            nulls_not_distinct: false,
        }
    }

    /// `UNIQUE`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateIndex, IndexTarget}, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).unique();
    /// assert!(i.is_unique());
    /// ```
    #[must_use]
    pub const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// `CONCURRENTLY`.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).concurrently();
    /// assert!(i.is_concurrent());
    /// ```
    #[must_use]
    pub const fn concurrently(mut self) -> Self {
        self.concurrently = true;
        self
    }

    /// `IF NOT EXISTS`.
    ///
    /// A concurrent index build that fails leaves an invalid index behind, so
    /// a re-runnable migration wants this together with a `DROP INDEX` of the
    /// invalid one.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), [])
    ///     .if_not_exists();
    /// assert!(i.is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn if_not_exists(mut self) -> Self {
        self.if_not_exists = true;
        self
    }

    /// `USING <method>`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateIndex, IndexMethod}, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), [])
    ///     .using(IndexMethod::Gin);
    /// assert_eq!(i.method(), Some(&IndexMethod::Gin));
    /// ```
    #[must_use]
    pub fn using(mut self, method: IndexMethod) -> Self {
        self.method = Some(method);
        self
    }

    /// `INCLUDE (…)` — carry extra columns in the leaf pages so a lookup can
    /// be index-only without widening the key.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), [])
    ///     .include([Ident::from_static("name")]);
    /// assert_eq!(i.included().len(), 1);
    /// ```
    #[must_use]
    pub fn include(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.include = columns.into_iter().collect();
        self
    }

    /// `WHERE …` — a partial index.
    ///
    /// A partial unique index is how "unique among rows that are not soft
    /// deleted" is expressed. An `ON CONFLICT` that targets it must repeat the
    /// same predicate; see
    /// [`OnConflict::target_where`](crate::OnConflict::target_where).
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Expr, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), [])
    ///     .where_(Expr::col(Ident::from_static("deleted_at")).is_null());
    /// assert!(i.predicate().is_some());
    /// ```
    #[must_use]
    pub fn where_(mut self, predicate: Expr) -> Self {
        self.predicate = Some(predicate);
        self
    }

    /// `NULLS NOT DISTINCT` — PostgreSQL 15 and later.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), [])
    ///     .nulls_not_distinct();
    /// assert!(i.has_nulls_not_distinct());
    /// ```
    #[must_use]
    pub const fn nulls_not_distinct(mut self) -> Self {
        self.nulls_not_distinct = true;
        self
    }

    /// The index name.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []);
    /// assert_eq!(i.name().as_str(), "i");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The indexed table.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []);
    /// assert_eq!(i.table().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// The indexed columns or expressions.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).targets().is_empty());
    /// ```
    #[must_use]
    pub fn targets(&self) -> &[IndexTarget] {
        &self.targets
    }

    /// Whether the index is unique.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(!CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).is_unique());
    /// ```
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// Whether the index is built concurrently.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(!CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).is_concurrent());
    /// ```
    #[must_use]
    pub const fn is_concurrent(&self) -> bool {
        self.concurrently
    }

    /// Whether `IF NOT EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(!CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn is_if_not_exists(&self) -> bool {
        self.if_not_exists
    }

    /// The index method, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateIndex, IndexMethod}, Ident, TableRef};
    /// let i = CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []);
    /// assert_eq!(i.method(), None::<&IndexMethod>);
    /// ```
    #[must_use]
    pub const fn method(&self) -> Option<&IndexMethod> {
        self.method.as_ref()
    }

    /// The `INCLUDE` columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).included().is_empty());
    /// ```
    #[must_use]
    pub fn included(&self) -> &[Ident] {
        &self.include
    }

    /// The partial-index predicate, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).predicate().is_none());
    /// ```
    #[must_use]
    pub const fn predicate(&self) -> Option<&Expr> {
        self.predicate.as_ref()
    }

    /// Whether `NULLS NOT DISTINCT` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateIndex, Ident, TableRef};
    /// assert!(!CreateIndex::new(Ident::from_static("i"), TableRef::from_static("t"), []).has_nulls_not_distinct());
    /// ```
    #[must_use]
    pub const fn has_nulls_not_distinct(&self) -> bool {
        self.nulls_not_distinct
    }
}

/// One indexed column or expression, with its sort order, `NULLS` placement
/// and operator class.
///
/// The operator class matters more than it looks: a `text` column needs
/// `text_pattern_ops` for a `LIKE 'prefix%'` index to be used at all, and
/// `gin_trgm_ops` for a trigram index.
///
/// ```
/// use moso_sql::ddl::IndexTarget;
/// use moso_sql::{Ident, Order};
///
/// let newest_first = IndexTarget::column(Ident::from_static("created_at")).order(Order::Desc);
/// assert_eq!(newest_first.sort_order(), Some(Order::Desc));
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct IndexTarget {
    expr: Expr,
    order: Option<Order>,
    nulls: Option<Nulls>,
    operator_class: Option<Ident>,
    collation: Option<Ident>,
}

impl IndexTarget {
    /// An index over a column.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// assert!(IndexTarget::column(Ident::from_static("c")).sort_order().is_none());
    /// ```
    #[must_use]
    pub const fn column(name: Ident) -> Self {
        Self::expr(Expr::col(name))
    }

    /// An index over an expression, such as `lower(email)`.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Expr, Function, Ident};
    /// let target = IndexTarget::expr(Expr::Function(Function::Lower(Box::new(
    ///     Expr::col(Ident::from_static("email")),
    /// ))));
    /// assert!(target.operator_class_name().is_none());
    /// ```
    #[must_use]
    pub const fn expr(expr: Expr) -> Self {
        Self {
            expr,
            order: None,
            nulls: None,
            operator_class: None,
            collation: None,
        }
    }

    /// Sets the sort order.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident, Order};
    /// let t = IndexTarget::column(Ident::from_static("c")).order(Order::Desc);
    /// assert_eq!(t.sort_order(), Some(Order::Desc));
    /// ```
    #[must_use]
    pub const fn order(mut self, order: Order) -> Self {
        self.order = Some(order);
        self
    }

    /// Sets the `NULLS` placement.
    ///
    /// An index only serves an `ORDER BY` whose direction *and* `NULLS`
    /// placement it matches, so a paginated query over a nullable column needs
    /// this to agree with the query.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident, Nulls};
    /// let t = IndexTarget::column(Ident::from_static("c")).nulls(Nulls::Last);
    /// assert_eq!(t.nulls_placement(), Some(Nulls::Last));
    /// ```
    #[must_use]
    pub const fn nulls(mut self, nulls: Nulls) -> Self {
        self.nulls = Some(nulls);
        self
    }

    /// Sets the operator class.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// let t = IndexTarget::column(Ident::from_static("c"))
    ///     .operator_class(Ident::from_static("gin_trgm_ops"));
    /// assert!(t.operator_class_name().is_some());
    /// ```
    #[must_use]
    pub fn operator_class(mut self, class: Ident) -> Self {
        self.operator_class = Some(class);
        self
    }

    /// Sets the collation.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// let t = IndexTarget::column(Ident::from_static("c")).collate(Ident::from_static("C"));
    /// assert!(t.collation().is_some());
    /// ```
    #[must_use]
    pub fn collate(mut self, collation: Ident) -> Self {
        self.collation = Some(collation);
        self
    }

    /// The indexed expression.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Expr, Ident};
    /// let t = IndexTarget::column(Ident::from_static("c"));
    /// assert_eq!(t.target_expr(), &Expr::col(Ident::from_static("c")));
    /// ```
    #[must_use]
    pub const fn target_expr(&self) -> &Expr {
        &self.expr
    }

    /// The sort order, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// assert!(IndexTarget::column(Ident::from_static("c")).sort_order().is_none());
    /// ```
    #[must_use]
    pub const fn sort_order(&self) -> Option<Order> {
        self.order
    }

    /// The `NULLS` placement, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// assert!(IndexTarget::column(Ident::from_static("c")).nulls_placement().is_none());
    /// ```
    #[must_use]
    pub const fn nulls_placement(&self) -> Option<Nulls> {
        self.nulls
    }

    /// The operator class, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// assert!(IndexTarget::column(Ident::from_static("c")).operator_class_name().is_none());
    /// ```
    #[must_use]
    pub const fn operator_class_name(&self) -> Option<&Ident> {
        self.operator_class.as_ref()
    }

    /// The collation, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::IndexTarget, Ident};
    /// assert!(IndexTarget::column(Ident::from_static("c")).collation().is_none());
    /// ```
    #[must_use]
    pub const fn collation(&self) -> Option<&Ident> {
        self.collation.as_ref()
    }
}

/// An index access method.
///
/// ```
/// use moso_sql::ddl::IndexMethod;
///
/// assert!(IndexMethod::Gin.suits_jsonb());
/// assert!(!IndexMethod::BTree.suits_jsonb());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum IndexMethod {
    /// `btree` — the default, and the only one that serves an `ORDER BY`.
    BTree,
    /// `hash` — equality only.
    Hash,
    /// `gin` — the one for `jsonb`, arrays and full-text search.
    Gin,
    /// `gist` — ranges, geometry, and exclusion constraints.
    Gist,
    /// `spgist`.
    SpGist,
    /// `brin` — tiny, and effective on a column correlated with physical
    /// order, such as an append-only timestamp.
    Brin,
    /// Any other method an extension provides, such as `hnsw`.
    Custom(Ident),
}

impl IndexMethod {
    /// Whether the method indexes `jsonb`, arrays or `tsvector`.
    ///
    /// ```
    /// use moso_sql::ddl::IndexMethod;
    ///
    /// assert!(IndexMethod::Gin.suits_jsonb());
    /// ```
    #[must_use]
    pub const fn suits_jsonb(&self) -> bool {
        matches!(self, Self::Gin | Self::Gist)
    }
}

/// `DROP INDEX`.
///
/// ```
/// use moso_sql::ddl::DropIndex;
/// use moso_sql::Ident;
///
/// let drop = DropIndex::new(Ident::from_static("idx_users_email")).concurrently().if_exists();
/// assert!(drop.is_concurrent());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct DropIndex {
    name: Ident,
    schema: Option<Ident>,
    if_exists: bool,
    concurrently: bool,
    cascade: bool,
}

impl DropIndex {
    /// Drops an index.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert_eq!(DropIndex::new(Ident::from_static("i")).name().as_str(), "i");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self {
            name,
            schema: None,
            if_exists: false,
            concurrently: false,
            cascade: false,
        }
    }

    /// Qualifies the index with a schema.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// let d = DropIndex::new(Ident::from_static("i")).in_schema(Ident::from_static("s"));
    /// assert!(d.schema().is_some());
    /// ```
    #[must_use]
    pub fn in_schema(mut self, schema: Ident) -> Self {
        self.schema = Some(schema);
        self
    }

    /// `IF EXISTS`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(DropIndex::new(Ident::from_static("i")).if_exists().is_if_exists());
    /// ```
    #[must_use]
    pub const fn if_exists(mut self) -> Self {
        self.if_exists = true;
        self
    }

    /// `CONCURRENTLY`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(DropIndex::new(Ident::from_static("i")).concurrently().is_concurrent());
    /// ```
    #[must_use]
    pub const fn concurrently(mut self) -> Self {
        self.concurrently = true;
        self
    }

    /// `CASCADE`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(DropIndex::new(Ident::from_static("i")).cascade().is_cascade());
    /// ```
    #[must_use]
    pub const fn cascade(mut self) -> Self {
        self.cascade = true;
        self
    }

    /// The index name.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert_eq!(DropIndex::new(Ident::from_static("i")).name().as_str(), "i");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The schema, if the index is qualified.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(DropIndex::new(Ident::from_static("i")).schema().is_none());
    /// ```
    #[must_use]
    pub const fn schema(&self) -> Option<&Ident> {
        self.schema.as_ref()
    }

    /// Whether `IF EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(!DropIndex::new(Ident::from_static("i")).is_if_exists());
    /// ```
    #[must_use]
    pub const fn is_if_exists(&self) -> bool {
        self.if_exists
    }

    /// Whether `CONCURRENTLY` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(!DropIndex::new(Ident::from_static("i")).is_concurrent());
    /// ```
    #[must_use]
    pub const fn is_concurrent(&self) -> bool {
        self.concurrently
    }

    /// Whether `CASCADE` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropIndex, Ident};
    /// assert!(!DropIndex::new(Ident::from_static("i")).is_cascade());
    /// ```
    #[must_use]
    pub const fn is_cascade(&self) -> bool {
        self.cascade
    }
}

/// `ALTER INDEX … RENAME TO …`.
///
/// The rename half of the zero-downtime unique-constraint swap: build a new
/// index concurrently, promote it, drop the old one, rename.
///
/// ```
/// use moso_sql::ddl::RenameIndex;
/// use moso_sql::Ident;
///
/// let rename = RenameIndex::new(Ident::from_static("idx_new"), Ident::from_static("idx"));
/// assert_eq!(rename.to().as_str(), "idx");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct RenameIndex {
    from: Ident,
    to: Ident,
}

impl RenameIndex {
    /// Renames an index.
    ///
    /// ```
    /// # use moso_sql::{ddl::RenameIndex, Ident};
    /// let r = RenameIndex::new(Ident::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(r.from().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn new(from: Ident, to: Ident) -> Self {
        Self { from, to }
    }

    /// The current name.
    ///
    /// ```
    /// # use moso_sql::{ddl::RenameIndex, Ident};
    /// let r = RenameIndex::new(Ident::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(r.from().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn from(&self) -> &Ident {
        &self.from
    }

    /// The new name.
    ///
    /// ```
    /// # use moso_sql::{ddl::RenameIndex, Ident};
    /// let r = RenameIndex::new(Ident::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(r.to().as_str(), "b");
    /// ```
    #[must_use]
    pub const fn to(&self) -> &Ident {
        &self.to
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_concurrent_build_is_flagged_for_the_runner() {
        let index = CreateIndex::new(
            Ident::from_static("i"),
            TableRef::from_static("t"),
            [IndexTarget::column(Ident::from_static("c"))],
        );
        assert!(!index.is_concurrent());
        assert!(index.concurrently().is_concurrent());
    }

    #[test]
    fn an_index_target_carries_everything_the_planner_matches_on() {
        let target = IndexTarget::column(Ident::from_static("created_at"))
            .order(Order::Desc)
            .nulls(Nulls::Last)
            .operator_class(Ident::from_static("timestamptz_ops"));
        assert_eq!(target.sort_order(), Some(Order::Desc));
        assert_eq!(target.nulls_placement(), Some(Nulls::Last));
        assert_eq!(
            target.operator_class_name().map(Ident::as_str),
            Some("timestamptz_ops")
        );
    }
}
