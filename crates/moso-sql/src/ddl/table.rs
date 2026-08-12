//! `CREATE TABLE`, `ALTER TABLE`, `DROP TABLE`, and the column and constraint
//! descriptions they carry.

use crate::expr::Expr;
use crate::ident::{Ident, TableRef};
use crate::types::DataType;

/// `CREATE TABLE`.
///
/// ```
/// use moso_sql::ddl::{ColumnSpec, CreateTable, TableConstraint};
/// use moso_sql::{DataType, Ident, TableRef};
///
/// let users = CreateTable::new(TableRef::from_static("users"))
///     .if_not_exists()
///     .column(ColumnSpec::new(Ident::from_static("id"), DataType::Uuid).primary_key())
///     .column(ColumnSpec::new(Ident::from_static("email"), DataType::Text).not_null())
///     .constraint(TableConstraint::unique(
///         Some(Ident::from_static("users_email_key")),
///         [Ident::from_static("email")],
///     ));
/// assert_eq!(users.columns().len(), 2);
/// assert_eq!(users.constraints().len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CreateTable {
    table: TableRef,
    if_not_exists: bool,
    temporary: bool,
    unlogged: bool,
    columns: Vec<ColumnSpec>,
    constraints: Vec<TableConstraint>,
    partition_by: Option<Partitioning>,
    comment: Option<String>,
}

impl CreateTable {
    /// An empty table definition.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).columns().is_empty());
    /// ```
    #[must_use]
    pub const fn new(table: TableRef) -> Self {
        Self {
            table,
            if_not_exists: false,
            temporary: false,
            unlogged: false,
            columns: Vec::new(),
            constraints: Vec::new(),
            partition_by: None,
            comment: None,
        }
    }

    /// `IF NOT EXISTS`.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).if_not_exists().is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn if_not_exists(mut self) -> Self {
        self.if_not_exists = true;
        self
    }

    /// `TEMPORARY` — the table lives for the session.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).temporary().is_temporary());
    /// ```
    #[must_use]
    pub const fn temporary(mut self) -> Self {
        self.temporary = true;
        self
    }

    /// `UNLOGGED` — PostgreSQL only. Much faster to write, and emptied by a
    /// crash, so it is right for a scratch table and wrong for everything else.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).unlogged().is_unlogged());
    /// ```
    #[must_use]
    pub const fn unlogged(mut self) -> Self {
        self.unlogged = true;
        self
    }

    /// Adds a column.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ColumnSpec, CreateTable}, DataType, Ident, TableRef};
    /// let t = CreateTable::new(TableRef::from_static("t"))
    ///     .column(ColumnSpec::new(Ident::from_static("a"), DataType::Integer));
    /// assert_eq!(t.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn column(mut self, column: ColumnSpec) -> Self {
        self.columns.push(column);
        self
    }

    /// Adds several columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ColumnSpec, CreateTable}, DataType, Ident, TableRef};
    /// let t = CreateTable::new(TableRef::from_static("t")).columns_from([
    ///     ColumnSpec::new(Ident::from_static("a"), DataType::Integer),
    ///     ColumnSpec::new(Ident::from_static("b"), DataType::Text),
    /// ]);
    /// assert_eq!(t.columns().len(), 2);
    /// ```
    #[must_use]
    pub fn columns_from(mut self, columns: impl IntoIterator<Item = ColumnSpec>) -> Self {
        self.columns.extend(columns);
        self
    }

    /// Adds a table-level constraint.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateTable, TableConstraint}, Ident, TableRef};
    /// let t = CreateTable::new(TableRef::from_static("t"))
    ///     .constraint(TableConstraint::primary_key(None, [Ident::from_static("id")]));
    /// assert_eq!(t.constraints().len(), 1);
    /// ```
    #[must_use]
    pub fn constraint(mut self, constraint: TableConstraint) -> Self {
        self.constraints.push(constraint);
        self
    }

    /// `PARTITION BY …`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateTable, PartitionStrategy, Partitioning}, Ident, TableRef};
    /// let t = CreateTable::new(TableRef::from_static("events")).partition_by(
    ///     Partitioning::new(PartitionStrategy::Range, [Ident::from_static("created_at")]),
    /// );
    /// assert!(t.partitioning().is_some());
    /// ```
    #[must_use]
    pub fn partition_by(mut self, partitioning: Partitioning) -> Self {
        self.partition_by = Some(partitioning);
        self
    }

    /// Attaches a comment, emitted as a following `COMMENT ON TABLE`.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// let t = CreateTable::new(TableRef::from_static("t")).comment("Everyone who can sign in.");
    /// assert!(t.comment_text().is_some());
    /// ```
    #[must_use]
    pub fn comment(mut self, text: impl Into<String>) -> Self {
        self.comment = Some(text.into());
        self
    }

    /// The table.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert_eq!(CreateTable::new(TableRef::from_static("t")).table().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// Whether `IF NOT EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(!CreateTable::new(TableRef::from_static("t")).is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn is_if_not_exists(&self) -> bool {
        self.if_not_exists
    }

    /// Whether the table is temporary.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(!CreateTable::new(TableRef::from_static("t")).is_temporary());
    /// ```
    #[must_use]
    pub const fn is_temporary(&self) -> bool {
        self.temporary
    }

    /// Whether the table is unlogged.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(!CreateTable::new(TableRef::from_static("t")).is_unlogged());
    /// ```
    #[must_use]
    pub const fn is_unlogged(&self) -> bool {
        self.unlogged
    }

    /// The columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).columns().is_empty());
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[ColumnSpec] {
        &self.columns
    }

    /// The table-level constraints.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).constraints().is_empty());
    /// ```
    #[must_use]
    pub fn constraints(&self) -> &[TableConstraint] {
        &self.constraints
    }

    /// The partitioning, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).partitioning().is_none());
    /// ```
    #[must_use]
    pub const fn partitioning(&self) -> Option<&Partitioning> {
        self.partition_by.as_ref()
    }

    /// The table comment, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateTable, TableRef};
    /// assert!(CreateTable::new(TableRef::from_static("t")).comment_text().is_none());
    /// ```
    #[must_use]
    pub fn comment_text(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// One column of a `CREATE TABLE` or an `ADD COLUMN`.
///
/// ```
/// use moso_sql::ddl::ColumnSpec;
/// use moso_sql::{DataType, Expr, Ident};
///
/// let created_at = ColumnSpec::new(
///     Ident::from_static("created_at"),
///     DataType::Timestamp { with_time_zone: true },
/// )
/// .not_null()
/// .default(Expr::Function(moso_sql::Function::Now));
/// assert!(!created_at.is_nullable());
/// assert!(created_at.default_value().is_some());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct ColumnSpec {
    name: Ident,
    data_type: DataType,
    nullable: bool,
    default: Option<Expr>,
    primary_key: bool,
    unique: bool,
    generated: Option<Generated>,
    identity: Option<Identity>,
    collation: Option<Ident>,
    check: Option<Expr>,
    references: Option<ForeignKey>,
    comment: Option<String>,
}

impl ColumnSpec {
    /// A nullable column of the given type.
    ///
    /// Nullable is the default because it is the only value that is always
    /// safe to add to a table that already has rows.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).is_nullable());
    /// ```
    #[must_use]
    pub const fn new(name: Ident, data_type: DataType) -> Self {
        Self {
            name,
            data_type,
            nullable: true,
            default: None,
            primary_key: false,
            unique: false,
            generated: None,
            identity: None,
            collation: None,
            check: None,
            references: None,
            comment: None,
        }
    }

    /// `NOT NULL`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(!ColumnSpec::new(Ident::from_static("a"), DataType::Text).not_null().is_nullable());
    /// ```
    #[must_use]
    pub const fn not_null(mut self) -> Self {
        self.nullable = false;
        self
    }

    /// Sets nullability explicitly.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("a"), DataType::Text).nullable(false);
    /// assert!(!c.is_nullable());
    /// ```
    #[must_use]
    pub const fn nullable(mut self, nullable: bool) -> Self {
        self.nullable = nullable;
        self
    }

    /// `DEFAULT expr`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Expr, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("a"), DataType::Boolean)
    ///     .default(Expr::value(false));
    /// assert!(c.default_value().is_some());
    /// ```
    #[must_use]
    pub fn default(mut self, value: Expr) -> Self {
        self.default = Some(value);
        self
    }

    /// `PRIMARY KEY` on this column alone. A composite key needs
    /// [`TableConstraint::primary_key`] instead.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("id"), DataType::Uuid).primary_key().is_primary_key());
    /// ```
    #[must_use]
    pub const fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self
    }

    /// `UNIQUE` on this column alone.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("e"), DataType::Text).unique().is_unique());
    /// ```
    #[must_use]
    pub const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// `GENERATED ALWAYS AS (expr) STORED`.
    ///
    /// A stored generated `tsvector` column plus a GIN index is the shape a
    /// full-text search should have: the vector is maintained by the server
    /// and cannot drift from the columns it summarises.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ColumnSpec, Generated}, DataType, Expr, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("search"), DataType::TsVector)
    ///     .generated(Generated::stored(Expr::col(Ident::from_static("title"))));
    /// assert!(c.generation().is_some());
    /// ```
    #[must_use]
    pub fn generated(mut self, generated: Generated) -> Self {
        self.generated = Some(generated);
        self
    }

    /// `GENERATED … AS IDENTITY` — the standard replacement for `serial`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ColumnSpec, Identity}, DataType, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("id"), DataType::BigInt)
    ///     .identity(Identity::Always);
    /// assert!(c.identity_kind().is_some());
    /// ```
    #[must_use]
    pub const fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// `COLLATE c`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("a"), DataType::Text)
    ///     .collate(Ident::from_static("C"));
    /// assert!(c.collation().is_some());
    /// ```
    #[must_use]
    pub fn collate(mut self, collation: Ident) -> Self {
        self.collation = Some(collation);
        self
    }

    /// A column-level `CHECK`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Expr, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("n"), DataType::Integer)
    ///     .check(Expr::col(Ident::from_static("n")).ge(Expr::value(0)));
    /// assert!(c.check_expr().is_some());
    /// ```
    #[must_use]
    pub fn check(mut self, predicate: Expr) -> Self {
        self.check = Some(predicate);
        self
    }

    /// An inline `REFERENCES`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ColumnSpec, ForeignKey}, DataType, Ident, TableRef};
    /// let c = ColumnSpec::new(Ident::from_static("author_id"), DataType::Uuid).references(
    ///     ForeignKey::new(
    ///         None,
    ///         [Ident::from_static("author_id")],
    ///         TableRef::from_static("users"),
    ///         [Ident::from_static("id")],
    ///     ),
    /// );
    /// assert!(c.foreign_key().is_some());
    /// ```
    #[must_use]
    pub fn references(mut self, foreign_key: ForeignKey) -> Self {
        self.references = Some(foreign_key);
        self
    }

    /// Attaches a comment, emitted as a following `COMMENT ON COLUMN`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("a"), DataType::Text).comment("The handle.");
    /// assert!(c.comment_text().is_some());
    /// ```
    #[must_use]
    pub fn comment(mut self, text: impl Into<String>) -> Self {
        self.comment = Some(text.into());
        self
    }

    /// The column name.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert_eq!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).name().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The column type.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// let c = ColumnSpec::new(Ident::from_static("a"), DataType::Text);
    /// assert_eq!(c.data_type(), &DataType::Text);
    /// ```
    #[must_use]
    pub const fn data_type(&self) -> &DataType {
        &self.data_type
    }

    /// Whether the column accepts `NULL`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).is_nullable());
    /// ```
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// The `DEFAULT`, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).default_value().is_none());
    /// ```
    #[must_use]
    pub const fn default_value(&self) -> Option<&Expr> {
        self.default.as_ref()
    }

    /// Whether the column is the single-column primary key.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(!ColumnSpec::new(Ident::from_static("a"), DataType::Text).is_primary_key());
    /// ```
    #[must_use]
    pub const fn is_primary_key(&self) -> bool {
        self.primary_key
    }

    /// Whether the column has a column-level `UNIQUE`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(!ColumnSpec::new(Ident::from_static("a"), DataType::Text).is_unique());
    /// ```
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// The generation expression, if the column is generated.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).generation().is_none());
    /// ```
    #[must_use]
    pub const fn generation(&self) -> Option<&Generated> {
        self.generated.as_ref()
    }

    /// The identity kind, if the column is an identity column.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::BigInt).identity_kind().is_none());
    /// ```
    #[must_use]
    pub const fn identity_kind(&self) -> Option<Identity> {
        self.identity
    }

    /// The collation, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).collation().is_none());
    /// ```
    #[must_use]
    pub const fn collation(&self) -> Option<&Ident> {
        self.collation.as_ref()
    }

    /// The column-level `CHECK`, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).check_expr().is_none());
    /// ```
    #[must_use]
    pub const fn check_expr(&self) -> Option<&Expr> {
        self.check.as_ref()
    }

    /// The inline foreign key, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Uuid).foreign_key().is_none());
    /// ```
    #[must_use]
    pub const fn foreign_key(&self) -> Option<&ForeignKey> {
        self.references.as_ref()
    }

    /// The column comment, if any.
    ///
    /// ```
    /// # use moso_sql::{ddl::ColumnSpec, DataType, Ident};
    /// assert!(ColumnSpec::new(Ident::from_static("a"), DataType::Text).comment_text().is_none());
    /// ```
    #[must_use]
    pub fn comment_text(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// A generated column's expression and storage.
///
/// ```
/// use moso_sql::ddl::Generated;
/// use moso_sql::{Expr, Ident};
///
/// let g = Generated::stored(Expr::col(Ident::from_static("title")));
/// assert!(g.is_stored());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Generated {
    expr: Expr,
    stored: bool,
}

impl Generated {
    /// `GENERATED ALWAYS AS (expr) STORED` — the only form PostgreSQL has.
    ///
    /// ```
    /// # use moso_sql::{ddl::Generated, Expr};
    /// assert!(Generated::stored(Expr::value(1)).is_stored());
    /// ```
    #[must_use]
    pub const fn stored(expr: Expr) -> Self {
        Self { expr, stored: true }
    }

    /// `GENERATED ALWAYS AS (expr) VIRTUAL` — SQLite computes it on read.
    ///
    /// ```
    /// # use moso_sql::{ddl::Generated, Expr};
    /// assert!(!Generated::virtual_(Expr::value(1)).is_stored());
    /// ```
    #[must_use]
    pub const fn virtual_(expr: Expr) -> Self {
        Self {
            expr,
            stored: false,
        }
    }

    /// The generation expression.
    ///
    /// ```
    /// # use moso_sql::{ddl::Generated, Expr};
    /// assert_eq!(Generated::stored(Expr::value(1)).expr(), &Expr::value(1));
    /// ```
    #[must_use]
    pub const fn expr(&self) -> &Expr {
        &self.expr
    }

    /// Whether the value is stored rather than recomputed on read.
    ///
    /// ```
    /// # use moso_sql::{ddl::Generated, Expr};
    /// assert!(Generated::stored(Expr::value(1)).is_stored());
    /// ```
    #[must_use]
    pub const fn is_stored(&self) -> bool {
        self.stored
    }
}

/// Which form of `GENERATED … AS IDENTITY` a column uses.
///
/// ```
/// use moso_sql::ddl::Identity;
///
/// assert_ne!(Identity::Always, Identity::ByDefault);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Identity {
    /// `GENERATED ALWAYS AS IDENTITY` — an explicit value is rejected, which
    /// is what stops an application from desynchronising the sequence.
    Always,
    /// `GENERATED BY DEFAULT AS IDENTITY` — an explicit value wins.
    ByDefault,
}

/// A table-level constraint.
///
/// ```
/// use moso_sql::ddl::TableConstraint;
/// use moso_sql::Ident;
///
/// let pk = TableConstraint::primary_key(
///     Some(Ident::from_static("orders_pkey")),
///     [Ident::from_static("order_id"), Ident::from_static("line_no")],
/// );
/// assert!(matches!(pk, TableConstraint::PrimaryKey { .. }));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TableConstraint {
    /// `PRIMARY KEY (…)`.
    PrimaryKey {
        /// The constraint name, if it is not left to the server.
        name: Option<Ident>,
        /// The key columns, in order.
        columns: Vec<Ident>,
    },
    /// `UNIQUE (…)`.
    Unique {
        /// The constraint name.
        name: Option<Ident>,
        /// The columns.
        columns: Vec<Ident>,
        /// `NULLS NOT DISTINCT` — PostgreSQL 15 and later. Without it two
        /// rows with `NULL` in a unique column are both allowed, which
        /// surprises everyone exactly once.
        nulls_not_distinct: bool,
    },
    /// `FOREIGN KEY (…) REFERENCES …`.
    ForeignKey(ForeignKey),
    /// `CHECK (…)`.
    Check {
        /// The constraint name.
        name: Option<Ident>,
        /// The predicate.
        expr: Expr,
        /// `NOT VALID` — add the constraint without scanning the existing
        /// rows, then `VALIDATE CONSTRAINT` separately under a weaker lock.
        not_valid: bool,
    },
    /// `EXCLUDE USING …` — PostgreSQL only. The way to say "these ranges may
    /// not overlap" without an application-level lock.
    Exclude {
        /// The constraint name.
        name: Option<Ident>,
        /// The index method, usually `gist`.
        method: Option<Ident>,
        /// Each element and the operator it must not satisfy with another row.
        elements: Vec<(Expr, Ident)>,
        /// An optional predicate limiting the rows the constraint covers.
        predicate: Option<Expr>,
    },
}

impl TableConstraint {
    /// A `PRIMARY KEY` constraint.
    ///
    /// ```
    /// # use moso_sql::{ddl::TableConstraint, Ident};
    /// let pk = TableConstraint::primary_key(None, [Ident::from_static("id")]);
    /// assert_eq!(pk.name(), None);
    /// ```
    #[must_use]
    pub fn primary_key(name: Option<Ident>, columns: impl IntoIterator<Item = Ident>) -> Self {
        Self::PrimaryKey {
            name,
            columns: columns.into_iter().collect(),
        }
    }

    /// A `UNIQUE` constraint, with `NULLS DISTINCT` — the SQL default.
    ///
    /// ```
    /// # use moso_sql::{ddl::TableConstraint, Ident};
    /// let unique = TableConstraint::unique(None, [Ident::from_static("email")]);
    /// assert!(matches!(unique, TableConstraint::Unique { .. }));
    /// ```
    #[must_use]
    pub fn unique(name: Option<Ident>, columns: impl IntoIterator<Item = Ident>) -> Self {
        Self::Unique {
            name,
            columns: columns.into_iter().collect(),
            nulls_not_distinct: false,
        }
    }

    /// A `CHECK` constraint that is validated immediately.
    ///
    /// ```
    /// # use moso_sql::{ddl::TableConstraint, Expr, Ident};
    /// let check = TableConstraint::check(None, Expr::col(Ident::from_static("n")).is_not_null());
    /// assert!(matches!(check, TableConstraint::Check { not_valid: false, .. }));
    /// ```
    #[must_use]
    pub const fn check(name: Option<Ident>, expr: Expr) -> Self {
        Self::Check {
            name,
            expr,
            not_valid: false,
        }
    }

    /// The constraint's name, if it has one.
    ///
    /// ```
    /// # use moso_sql::{ddl::TableConstraint, Ident};
    /// let pk = TableConstraint::primary_key(Some(Ident::from_static("k")), [Ident::from_static("id")]);
    /// assert_eq!(pk.name().map(Ident::as_str), Some("k"));
    /// ```
    #[must_use]
    pub const fn name(&self) -> Option<&Ident> {
        match self {
            Self::PrimaryKey { name, .. }
            | Self::Unique { name, .. }
            | Self::Check { name, .. }
            | Self::Exclude { name, .. } => name.as_ref(),
            Self::ForeignKey(foreign_key) => foreign_key.name(),
        }
    }
}

/// A foreign-key constraint.
///
/// # The zero-downtime idiom
///
/// Adding a foreign key to a table with rows takes a lock while every row is
/// checked. The two-step form — `ADD CONSTRAINT … NOT VALID`, then
/// `VALIDATE CONSTRAINT` — takes the strong lock only for the catalogue
/// change, and the migration generator emits it that way
/// (`docs/02-data/23-migrations.md`).
///
/// ```
/// use moso_sql::ddl::{ForeignKey, ReferentialAction};
/// use moso_sql::{Ident, TableRef};
///
/// let author = ForeignKey::new(
///     Some(Ident::from_static("posts_author_id_fkey")),
///     [Ident::from_static("author_id")],
///     TableRef::from_static("users"),
///     [Ident::from_static("id")],
/// )
/// .on_delete(ReferentialAction::Cascade)
/// .not_valid();
/// assert!(author.is_not_valid());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct ForeignKey {
    name: Option<Ident>,
    columns: Vec<Ident>,
    references_table: TableRef,
    references_columns: Vec<Ident>,
    on_delete: Option<ReferentialAction>,
    on_update: Option<ReferentialAction>,
    deferrable: bool,
    initially_deferred: bool,
    not_valid: bool,
}

impl ForeignKey {
    /// A foreign key from `columns` to `references_columns` of another table.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(
    ///     None,
    ///     [Ident::from_static("a_id")],
    ///     TableRef::from_static("a"),
    ///     [Ident::from_static("id")],
    /// );
    /// assert_eq!(fk.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn new(
        name: Option<Ident>,
        columns: impl IntoIterator<Item = Ident>,
        references_table: TableRef,
        references_columns: impl IntoIterator<Item = Ident>,
    ) -> Self {
        Self {
            name,
            columns: columns.into_iter().collect(),
            references_table,
            references_columns: references_columns.into_iter().collect(),
            on_delete: None,
            on_update: None,
            deferrable: false,
            initially_deferred: false,
            not_valid: false,
        }
    }

    /// `ON DELETE …`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ForeignKey, ReferentialAction}, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")])
    ///     .on_delete(ReferentialAction::SetNull);
    /// assert_eq!(fk.delete_action(), Some(ReferentialAction::SetNull));
    /// ```
    #[must_use]
    pub const fn on_delete(mut self, action: ReferentialAction) -> Self {
        self.on_delete = Some(action);
        self
    }

    /// `ON UPDATE …`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{ForeignKey, ReferentialAction}, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")])
    ///     .on_update(ReferentialAction::Cascade);
    /// assert_eq!(fk.update_action(), Some(ReferentialAction::Cascade));
    /// ```
    #[must_use]
    pub const fn on_update(mut self, action: ReferentialAction) -> Self {
        self.on_update = Some(action);
        self
    }

    /// `DEFERRABLE`, so the check can be postponed to commit.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")])
    ///     .deferrable(true);
    /// assert!(fk.is_deferrable());
    /// ```
    #[must_use]
    pub const fn deferrable(mut self, initially_deferred: bool) -> Self {
        self.deferrable = true;
        self.initially_deferred = initially_deferred;
        self
    }

    /// `NOT VALID` — add without checking the existing rows.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")])
    ///     .not_valid();
    /// assert!(fk.is_not_valid());
    /// ```
    #[must_use]
    pub const fn not_valid(mut self) -> Self {
        self.not_valid = true;
        self
    }

    /// The constraint name, if it has one.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert!(fk.name().is_none());
    /// ```
    #[must_use]
    pub const fn name(&self) -> Option<&Ident> {
        self.name.as_ref()
    }

    /// The referencing columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert_eq!(fk.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// The referenced table.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert_eq!(fk.target_table().name().as_str(), "b");
    /// ```
    #[must_use]
    pub const fn target_table(&self) -> &TableRef {
        &self.references_table
    }

    /// The referenced columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert_eq!(fk.target_columns().len(), 1);
    /// ```
    #[must_use]
    pub fn target_columns(&self) -> &[Ident] {
        &self.references_columns
    }

    /// The `ON DELETE` action, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert!(fk.delete_action().is_none());
    /// ```
    #[must_use]
    pub const fn delete_action(&self) -> Option<ReferentialAction> {
        self.on_delete
    }

    /// The `ON UPDATE` action, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert!(fk.update_action().is_none());
    /// ```
    #[must_use]
    pub const fn update_action(&self) -> Option<ReferentialAction> {
        self.on_update
    }

    /// Whether the constraint is deferrable.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert!(!fk.is_deferrable());
    /// ```
    #[must_use]
    pub const fn is_deferrable(&self) -> bool {
        self.deferrable
    }

    /// Whether a deferrable constraint starts deferred.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert!(!fk.is_initially_deferred());
    /// ```
    #[must_use]
    pub const fn is_initially_deferred(&self) -> bool {
        self.initially_deferred
    }

    /// Whether the constraint is added `NOT VALID`.
    ///
    /// ```
    /// # use moso_sql::{ddl::ForeignKey, Ident, TableRef};
    /// let fk = ForeignKey::new(None, [Ident::from_static("a")], TableRef::from_static("b"), [Ident::from_static("id")]);
    /// assert!(!fk.is_not_valid());
    /// ```
    #[must_use]
    pub const fn is_not_valid(&self) -> bool {
        self.not_valid
    }
}

/// What happens to a referencing row when the referenced one changes.
///
/// ```
/// use moso_sql::ddl::ReferentialAction;
///
/// assert_ne!(ReferentialAction::Cascade, ReferentialAction::Restrict);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ReferentialAction {
    /// `NO ACTION` — the check is deferred to the end of the statement.
    NoAction,
    /// `RESTRICT` — the change is refused immediately.
    Restrict,
    /// `CASCADE` — the referencing rows go too.
    Cascade,
    /// `SET NULL`.
    SetNull,
    /// `SET DEFAULT`.
    SetDefault,
}

/// `ALTER TABLE`, as a list of actions applied in one statement.
///
/// PostgreSQL takes one lock for the whole statement, so grouping the actions
/// is not cosmetic: three separate `ALTER TABLE`s take the lock three times.
///
/// ```
/// use moso_sql::ddl::{AlterTable, AlterTableAction, ColumnSpec};
/// use moso_sql::{DataType, Ident, TableRef};
///
/// let alter = AlterTable::new(TableRef::from_static("users"))
///     .add_column(ColumnSpec::new(Ident::from_static("locale"), DataType::Text))
///     .action(AlterTableAction::SetNotNull(Ident::from_static("locale")));
/// assert_eq!(alter.actions().len(), 2);
/// assert!(!alter.is_destructive());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct AlterTable {
    table: TableRef,
    actions: Vec<AlterTableAction>,
}

impl AlterTable {
    /// An `ALTER TABLE` with no actions yet.
    ///
    /// ```
    /// # use moso_sql::{ddl::AlterTable, TableRef};
    /// assert!(AlterTable::new(TableRef::from_static("t")).actions().is_empty());
    /// ```
    #[must_use]
    pub const fn new(table: TableRef) -> Self {
        Self {
            table,
            actions: Vec::new(),
        }
    }

    /// Adds an action.
    ///
    /// ```
    /// # use moso_sql::{ddl::{AlterTable, AlterTableAction}, Ident, TableRef};
    /// let a = AlterTable::new(TableRef::from_static("t"))
    ///     .action(AlterTableAction::DropDefault(Ident::from_static("x")));
    /// assert_eq!(a.actions().len(), 1);
    /// ```
    #[must_use]
    pub fn action(mut self, action: AlterTableAction) -> Self {
        self.actions.push(action);
        self
    }

    /// `ADD COLUMN`.
    ///
    /// ```
    /// # use moso_sql::{ddl::{AlterTable, ColumnSpec}, DataType, Ident, TableRef};
    /// let a = AlterTable::new(TableRef::from_static("t"))
    ///     .add_column(ColumnSpec::new(Ident::from_static("c"), DataType::Text));
    /// assert_eq!(a.actions().len(), 1);
    /// ```
    #[must_use]
    pub fn add_column(self, column: ColumnSpec) -> Self {
        self.action(AlterTableAction::AddColumn {
            column: Box::new(column),
            if_not_exists: false,
        })
    }

    /// `DROP COLUMN` — destructive.
    ///
    /// ```
    /// # use moso_sql::{ddl::AlterTable, Ident, TableRef};
    /// let a = AlterTable::new(TableRef::from_static("t")).drop_column(Ident::from_static("c"));
    /// assert!(a.is_destructive());
    /// ```
    #[must_use]
    pub fn drop_column(self, name: Ident) -> Self {
        self.action(AlterTableAction::DropColumn {
            name,
            if_exists: false,
            cascade: false,
        })
    }

    /// The table.
    ///
    /// ```
    /// # use moso_sql::{ddl::AlterTable, TableRef};
    /// assert_eq!(AlterTable::new(TableRef::from_static("t")).table().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// The actions, in order.
    ///
    /// ```
    /// # use moso_sql::{ddl::AlterTable, TableRef};
    /// assert!(AlterTable::new(TableRef::from_static("t")).actions().is_empty());
    /// ```
    #[must_use]
    pub fn actions(&self) -> &[AlterTableAction] {
        &self.actions
    }

    /// Whether any action can destroy data.
    ///
    /// ```
    /// # use moso_sql::{ddl::AlterTable, Ident, TableRef};
    /// assert!(AlterTable::new(TableRef::from_static("t")).drop_column(Ident::from_static("c")).is_destructive());
    /// ```
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        self.actions.iter().any(AlterTableAction::is_destructive)
    }
}

/// One action of an [`AlterTable`].
///
/// ```
/// use moso_sql::ddl::AlterTableAction;
/// use moso_sql::Ident;
///
/// let action = AlterTableAction::SetNotNull(Ident::from_static("locale"));
/// assert!(!action.is_destructive());
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum AlterTableAction {
    /// `ADD COLUMN`.
    ///
    /// The column is boxed: a [`ColumnSpec`] is by far the largest thing an
    /// action can carry, and every other variant would pay for it.
    AddColumn {
        /// The new column.
        column: Box<ColumnSpec>,
        /// `IF NOT EXISTS`.
        if_not_exists: bool,
    },
    /// `DROP COLUMN` — destructive.
    DropColumn {
        /// The column.
        name: Ident,
        /// `IF EXISTS`.
        if_exists: bool,
        /// `CASCADE` — also drop whatever depends on it.
        cascade: bool,
    },
    /// `RENAME COLUMN … TO …`.
    RenameColumn {
        /// The current name.
        from: Ident,
        /// The new name.
        to: Ident,
    },
    /// `ALTER COLUMN … TYPE …` — destructive when the conversion is lossy.
    AlterColumnType {
        /// The column.
        name: Ident,
        /// The new type.
        data_type: DataType,
        /// The `USING` expression, required for any conversion the server
        /// cannot do implicitly.
        using: Option<Expr>,
        /// Whether the conversion can lose data, which is what makes the whole
        /// migration require an acknowledgement.
        lossy: bool,
    },
    /// `ALTER COLUMN … SET NOT NULL`.
    SetNotNull(Ident),
    /// `ALTER COLUMN … DROP NOT NULL`.
    DropNotNull(Ident),
    /// `ALTER COLUMN … SET DEFAULT …`.
    SetDefault {
        /// The column.
        name: Ident,
        /// The new default.
        value: Expr,
    },
    /// `ALTER COLUMN … DROP DEFAULT`.
    DropDefault(Ident),
    /// `ADD CONSTRAINT …`.
    AddConstraint(TableConstraint),
    /// `DROP CONSTRAINT …` — destructive in the sense that it removes a
    /// guarantee the application may depend on.
    DropConstraint {
        /// The constraint.
        name: Ident,
        /// `IF EXISTS`.
        if_exists: bool,
        /// `CASCADE`.
        cascade: bool,
    },
    /// `VALIDATE CONSTRAINT …` — the second half of the `NOT VALID` idiom.
    ValidateConstraint(Ident),
    /// `RENAME CONSTRAINT … TO …`.
    RenameConstraint {
        /// The current name.
        from: Ident,
        /// The new name.
        to: Ident,
    },
    /// `ADD PRIMARY KEY USING INDEX …` — promote an index that was built
    /// concurrently, so the strong lock is held for a moment rather than for
    /// the whole build.
    AddPrimaryKeyUsingIndex {
        /// The constraint name.
        name: Option<Ident>,
        /// The existing unique index.
        index: Ident,
    },
    /// `ADD UNIQUE USING INDEX …` — the same trick for a unique constraint.
    AddUniqueUsingIndex {
        /// The constraint name.
        name: Option<Ident>,
        /// The existing unique index.
        index: Ident,
    },
    /// `SET SCHEMA …`.
    SetSchema(Ident),
    /// `ATTACH PARTITION … FOR VALUES …`.
    AttachPartition {
        /// The partition table.
        partition: TableRef,
        /// The bound clause, written out because its grammar depends on the
        /// partitioning strategy.
        bounds: String,
    },
    /// `DETACH PARTITION …`.
    DetachPartition {
        /// The partition table.
        partition: TableRef,
        /// `CONCURRENTLY`.
        concurrently: bool,
    },
}

impl AlterTableAction {
    /// Whether the action can destroy data or remove a guarantee.
    ///
    /// ```
    /// use moso_sql::ddl::AlterTableAction;
    /// use moso_sql::Ident;
    ///
    /// assert!(AlterTableAction::DropColumn {
    ///     name: Ident::from_static("c"),
    ///     if_exists: false,
    ///     cascade: false,
    /// }.is_destructive());
    /// assert!(!AlterTableAction::SetNotNull(Ident::from_static("c")).is_destructive());
    /// ```
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        match self {
            Self::DropColumn { .. } | Self::DropConstraint { .. } => true,
            Self::AlterColumnType { lossy, .. } => *lossy,
            _ => false,
        }
    }
}

/// `DROP TABLE`.
///
/// ```
/// use moso_sql::ddl::DropTable;
/// use moso_sql::TableRef;
///
/// let drop = DropTable::new([TableRef::from_static("legacy")]).if_exists();
/// assert!(drop.is_if_exists());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct DropTable {
    tables: Vec<TableRef>,
    if_exists: bool,
    cascade: bool,
}

impl DropTable {
    /// Drops the given tables.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropTable, TableRef};
    /// assert_eq!(DropTable::new([TableRef::from_static("t")]).tables().len(), 1);
    /// ```
    #[must_use]
    pub fn new(tables: impl IntoIterator<Item = TableRef>) -> Self {
        Self {
            tables: tables.into_iter().collect(),
            if_exists: false,
            cascade: false,
        }
    }

    /// `IF EXISTS`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropTable, TableRef};
    /// assert!(DropTable::new([TableRef::from_static("t")]).if_exists().is_if_exists());
    /// ```
    #[must_use]
    pub const fn if_exists(mut self) -> Self {
        self.if_exists = true;
        self
    }

    /// `CASCADE`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropTable, TableRef};
    /// assert!(DropTable::new([TableRef::from_static("t")]).cascade().is_cascade());
    /// ```
    #[must_use]
    pub const fn cascade(mut self) -> Self {
        self.cascade = true;
        self
    }

    /// The tables.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropTable, TableRef};
    /// assert_eq!(DropTable::new([TableRef::from_static("t")]).tables().len(), 1);
    /// ```
    #[must_use]
    pub fn tables(&self) -> &[TableRef] {
        &self.tables
    }

    /// Whether `IF EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropTable, TableRef};
    /// assert!(!DropTable::new([TableRef::from_static("t")]).is_if_exists());
    /// ```
    #[must_use]
    pub const fn is_if_exists(&self) -> bool {
        self.if_exists
    }

    /// Whether `CASCADE` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropTable, TableRef};
    /// assert!(!DropTable::new([TableRef::from_static("t")]).is_cascade());
    /// ```
    #[must_use]
    pub const fn is_cascade(&self) -> bool {
        self.cascade
    }
}

/// `ALTER TABLE … RENAME TO …`.
///
/// ```
/// use moso_sql::ddl::RenameTable;
/// use moso_sql::{Ident, TableRef};
///
/// let rename = RenameTable::new(TableRef::from_static("user"), Ident::from_static("users"));
/// assert_eq!(rename.to().as_str(), "users");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct RenameTable {
    from: TableRef,
    to: Ident,
}

impl RenameTable {
    /// Renames a table within its schema.
    ///
    /// ```
    /// # use moso_sql::{ddl::RenameTable, Ident, TableRef};
    /// let r = RenameTable::new(TableRef::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(r.from().name().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn new(from: TableRef, to: Ident) -> Self {
        Self { from, to }
    }

    /// The current table.
    ///
    /// ```
    /// # use moso_sql::{ddl::RenameTable, Ident, TableRef};
    /// let r = RenameTable::new(TableRef::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(r.from().name().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn from(&self) -> &TableRef {
        &self.from
    }

    /// The new name.
    ///
    /// ```
    /// # use moso_sql::{ddl::RenameTable, Ident, TableRef};
    /// let r = RenameTable::new(TableRef::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(r.to().as_str(), "b");
    /// ```
    #[must_use]
    pub const fn to(&self) -> &Ident {
        &self.to
    }
}

/// A table's partitioning.
///
/// ```
/// use moso_sql::ddl::{PartitionStrategy, Partitioning};
/// use moso_sql::Ident;
///
/// let by_month = Partitioning::new(PartitionStrategy::Range, [Ident::from_static("created_at")]);
/// assert_eq!(by_month.strategy(), PartitionStrategy::Range);
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Partitioning {
    strategy: PartitionStrategy,
    columns: Vec<Ident>,
}

impl Partitioning {
    /// Partitions by the given columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::{PartitionStrategy, Partitioning}, Ident};
    /// let p = Partitioning::new(PartitionStrategy::Hash, [Ident::from_static("id")]);
    /// assert_eq!(p.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn new(strategy: PartitionStrategy, columns: impl IntoIterator<Item = Ident>) -> Self {
        Self {
            strategy,
            columns: columns.into_iter().collect(),
        }
    }

    /// The strategy.
    ///
    /// ```
    /// # use moso_sql::{ddl::{PartitionStrategy, Partitioning}, Ident};
    /// let p = Partitioning::new(PartitionStrategy::List, [Ident::from_static("region")]);
    /// assert_eq!(p.strategy(), PartitionStrategy::List);
    /// ```
    #[must_use]
    pub const fn strategy(&self) -> PartitionStrategy {
        self.strategy
    }

    /// The partition key columns.
    ///
    /// ```
    /// # use moso_sql::{ddl::{PartitionStrategy, Partitioning}, Ident};
    /// let p = Partitioning::new(PartitionStrategy::Range, [Ident::from_static("at")]);
    /// assert_eq!(p.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }
}

/// How a partitioned table divides its rows.
///
/// ```
/// use moso_sql::ddl::PartitionStrategy;
///
/// assert_ne!(PartitionStrategy::Range, PartitionStrategy::Hash);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PartitionStrategy {
    /// `PARTITION BY RANGE` — the usual choice for a time series.
    Range,
    /// `PARTITION BY LIST`.
    List,
    /// `PARTITION BY HASH`.
    Hash,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_column_is_nullable_until_told_otherwise() {
        let column = ColumnSpec::new(Ident::from_static("locale"), DataType::Text);
        assert!(column.is_nullable());
        assert!(!column.not_null().is_nullable());
    }

    #[test]
    fn the_destructive_actions_are_the_ones_the_generator_comments_out() {
        let lossy = AlterTableAction::AlterColumnType {
            name: Ident::from_static("n"),
            data_type: DataType::SmallInt,
            using: None,
            lossy: true,
        };
        let widening = AlterTableAction::AlterColumnType {
            name: Ident::from_static("n"),
            data_type: DataType::BigInt,
            using: None,
            lossy: false,
        };
        assert!(lossy.is_destructive());
        assert!(!widening.is_destructive());
        assert!(
            AlterTable::new(TableRef::from_static("t"))
                .action(lossy)
                .is_destructive()
        );
    }

    #[test]
    fn a_constraint_reports_its_own_name() {
        let named = TableConstraint::unique(
            Some(Ident::from_static("users_email_key")),
            [Ident::from_static("email")],
        );
        assert_eq!(named.name().map(Ident::as_str), Some("users_email_key"));
        let anonymous = TableConstraint::primary_key(None, [Ident::from_static("id")]);
        assert!(anonymous.name().is_none());
        let foreign = TableConstraint::ForeignKey(ForeignKey::new(
            Some(Ident::from_static("fk")),
            [Ident::from_static("a")],
            TableRef::from_static("b"),
            [Ident::from_static("id")],
        ));
        assert_eq!(foreign.name().map(Ident::as_str), Some("fk"));
    }
}
