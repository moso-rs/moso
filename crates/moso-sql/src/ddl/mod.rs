//! Data-definition statements: everything `moso-migrate` needs to write a
//! migration from an entity graph.
//!
//! The coverage here is the operation table in `docs/02-data/23-migrations.md`,
//! including the parts that make a migration safe to run on a live database and
//! that generic query builders usually omit: `CREATE INDEX CONCURRENTLY`,
//! `ADD CONSTRAINT … NOT VALID` followed by `VALIDATE CONSTRAINT`,
//! `ADD CONSTRAINT … USING INDEX`, and `ALTER TYPE … ADD VALUE`. Those four are
//! the difference between a schema change that takes a lock for a millisecond
//! and one that takes the site down.

mod enum_type;
mod index;
mod table;

pub use self::enum_type::{AlterType, AlterTypeAction, CreateType, DropType, TypeBody};
pub use self::index::{CreateIndex, DropIndex, IndexMethod, IndexTarget, RenameIndex};
pub use self::table::{
    AlterTable, AlterTableAction, ColumnSpec, CreateTable, DropTable, ForeignKey, Generated,
    Identity, PartitionStrategy, Partitioning, ReferentialAction, RenameTable, TableConstraint,
};

use crate::dialect::Dialect;
use crate::error::Error;
use crate::ident::{Ident, TableRef};
use crate::sql::Sql;
use crate::statement::{RawStatement, Statement, StatementRef};

/// A schema-changing statement.
///
/// ```
/// use moso_sql::ddl::{CreateTable, Ddl};
/// use moso_sql::TableRef;
///
/// let create = Ddl::CreateTable(CreateTable::new(TableRef::from_static("users")));
/// assert!(!create.is_destructive());
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Ddl {
    /// `CREATE TABLE`.
    CreateTable(CreateTable),
    /// `ALTER TABLE`.
    AlterTable(AlterTable),
    /// `DROP TABLE`.
    DropTable(DropTable),
    /// `ALTER TABLE … RENAME TO`.
    RenameTable(RenameTable),
    /// `TRUNCATE`.
    Truncate(Truncate),
    /// `CREATE INDEX`.
    CreateIndex(CreateIndex),
    /// `DROP INDEX`.
    DropIndex(DropIndex),
    /// `ALTER INDEX … RENAME TO`.
    RenameIndex(RenameIndex),
    /// `CREATE TYPE`.
    CreateType(CreateType),
    /// `ALTER TYPE`.
    AlterType(AlterType),
    /// `DROP TYPE`.
    DropType(DropType),
    /// `CREATE SCHEMA`.
    CreateSchema(CreateSchema),
    /// `DROP SCHEMA`.
    DropSchema(DropSchema),
    /// `CREATE EXTENSION`.
    CreateExtension(CreateExtension),
    /// `COMMENT ON`.
    Comment(CommentOn),
    /// Anything else, written out.
    Raw(RawStatement),
}

impl Ddl {
    /// Whether running this statement can destroy data.
    ///
    /// `moso db make-migration` emits a destructive operation commented out,
    /// with a header, and `moso db migrate` refuses to apply it until a human
    /// has uncommented it or passed `--allow-destructive`
    /// (`docs/02-data/23-migrations.md` § safety policy). This is the predicate
    /// that decides.
    ///
    /// ```
    /// use moso_sql::ddl::{Ddl, DropTable};
    /// use moso_sql::TableRef;
    ///
    /// let drop = Ddl::DropTable(DropTable::new([TableRef::from_static("users")]));
    /// assert!(drop.is_destructive());
    /// ```
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        match self {
            Self::DropTable(_) | Self::Truncate(_) | Self::DropSchema(_) | Self::DropType(_) => {
                true
            }
            Self::AlterTable(alter) => alter.is_destructive(),
            // A raw statement is not parsed, so it is treated as destructive:
            // guessing the other way is how data disappears.
            Self::Raw(_) => true,
            _ => false,
        }
    }

    /// Whether the statement must run outside a transaction.
    ///
    /// `CREATE INDEX CONCURRENTLY` and, before PostgreSQL 12,
    /// `ALTER TYPE … ADD VALUE` cannot run inside one. A migration containing
    /// either has to be marked non-transactional, and a runner that gets this
    /// wrong fails at apply time on a production database.
    ///
    /// ```
    /// use moso_sql::ddl::{CreateIndex, Ddl, IndexTarget};
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let index = CreateIndex::new(
    ///     Ident::from_static("idx_users_email"),
    ///     TableRef::from_static("users"),
    ///     [IndexTarget::column(Ident::from_static("email"))],
    /// )
    /// .concurrently();
    /// assert!(Ddl::CreateIndex(index).requires_no_transaction());
    /// ```
    #[must_use]
    pub fn requires_no_transaction(&self) -> bool {
        match self {
            Self::CreateIndex(index) => index.is_concurrent(),
            Self::DropIndex(index) => index.is_concurrent(),
            Self::AlterType(alter) => alter.requires_no_transaction(),
            _ => false,
        }
    }

    /// Renders the statement for a dialect.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] for a construct the dialect does not have —
    /// SQLite has no enum types, no concurrent indexes and no
    /// `ALTER COLUMN … TYPE`, and the migration generator is expected to
    /// substitute a table rebuild rather than to ignore the error.
    ///
    /// ```
    /// use moso_sql::ddl::{ColumnSpec, CreateTable, Ddl};
    /// use moso_sql::{DataType, Ident, Postgres, TableRef};
    ///
    /// let create = CreateTable::new(TableRef::from_static("t"))
    ///     .column(ColumnSpec::new(Ident::from_static("id"), DataType::Uuid).primary_key());
    /// let sql = Ddl::CreateTable(create).build(&Postgres)?;
    /// assert_eq!(sql.text, r#"CREATE TABLE "t" ("id" uuid PRIMARY KEY)"#);
    /// // Schema changes bind no parameters: the catalogue stores the text.
    /// assert!(sql.args.is_empty());
    /// # Ok::<(), moso_sql::Error>(())
    /// ```
    pub fn build(&self, dialect: &dyn Dialect) -> Result<Sql, Error> {
        dialect.build(StatementRef::Ddl(self))
    }

    /// Wraps the statement as a [`Statement`].
    ///
    /// ```
    /// use moso_sql::ddl::{CreateTable, Ddl};
    /// use moso_sql::{Statement, TableRef};
    ///
    /// let s = Ddl::CreateTable(CreateTable::new(TableRef::from_static("t"))).into_statement();
    /// assert!(matches!(s, Statement::Ddl(_)));
    /// ```
    #[must_use]
    pub fn into_statement(self) -> Statement {
        Statement::Ddl(self)
    }
}

/// `TRUNCATE`.
///
/// ```
/// use moso_sql::ddl::Truncate;
/// use moso_sql::TableRef;
///
/// let truncate = Truncate::new([TableRef::from_static("events")]).restart_identity();
/// assert!(truncate.restarts_identity());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct Truncate {
    tables: Vec<TableRef>,
    restart_identity: bool,
    cascade: bool,
}

impl Truncate {
    /// Truncates the given tables in one statement, which is the only way to
    /// truncate a set of tables joined by foreign keys.
    ///
    /// ```
    /// # use moso_sql::{ddl::Truncate, TableRef};
    /// assert_eq!(Truncate::new([TableRef::from_static("t")]).tables().len(), 1);
    /// ```
    #[must_use]
    pub fn new(tables: impl IntoIterator<Item = TableRef>) -> Self {
        Self {
            tables: tables.into_iter().collect(),
            restart_identity: false,
            cascade: false,
        }
    }

    /// `RESTART IDENTITY` — reset the tables' sequences too.
    ///
    /// ```
    /// # use moso_sql::{ddl::Truncate, TableRef};
    /// assert!(Truncate::new([TableRef::from_static("t")]).restart_identity().restarts_identity());
    /// ```
    #[must_use]
    pub const fn restart_identity(mut self) -> Self {
        self.restart_identity = true;
        self
    }

    /// `CASCADE` — also truncate every table with a foreign key into these.
    ///
    /// ```
    /// # use moso_sql::{ddl::Truncate, TableRef};
    /// assert!(Truncate::new([TableRef::from_static("t")]).cascade().is_cascade());
    /// ```
    #[must_use]
    pub const fn cascade(mut self) -> Self {
        self.cascade = true;
        self
    }

    /// The tables.
    ///
    /// ```
    /// # use moso_sql::{ddl::Truncate, TableRef};
    /// assert_eq!(Truncate::new([TableRef::from_static("t")]).tables().len(), 1);
    /// ```
    #[must_use]
    pub fn tables(&self) -> &[TableRef] {
        &self.tables
    }

    /// Whether `RESTART IDENTITY` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::Truncate, TableRef};
    /// assert!(!Truncate::new([TableRef::from_static("t")]).restarts_identity());
    /// ```
    #[must_use]
    pub const fn restarts_identity(&self) -> bool {
        self.restart_identity
    }

    /// Whether `CASCADE` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::Truncate, TableRef};
    /// assert!(!Truncate::new([TableRef::from_static("t")]).is_cascade());
    /// ```
    #[must_use]
    pub const fn is_cascade(&self) -> bool {
        self.cascade
    }
}

/// `CREATE SCHEMA`.
///
/// ```
/// use moso_sql::ddl::CreateSchema;
/// use moso_sql::Ident;
///
/// let schema = CreateSchema::new(Ident::from_static("billing")).if_not_exists();
/// assert!(schema.is_if_not_exists());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CreateSchema {
    name: Ident,
    if_not_exists: bool,
    authorization: Option<Ident>,
}

impl CreateSchema {
    /// A schema.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateSchema, Ident};
    /// assert_eq!(CreateSchema::new(Ident::from_static("s")).name().as_str(), "s");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self {
            name,
            if_not_exists: false,
            authorization: None,
        }
    }

    /// `IF NOT EXISTS`, which is what makes a migration re-runnable after a
    /// partial failure.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateSchema, Ident};
    /// assert!(CreateSchema::new(Ident::from_static("s")).if_not_exists().is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn if_not_exists(mut self) -> Self {
        self.if_not_exists = true;
        self
    }

    /// `AUTHORIZATION role`.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateSchema, Ident};
    /// let s = CreateSchema::new(Ident::from_static("s")).authorization(Ident::from_static("app"));
    /// assert!(s.owner().is_some());
    /// ```
    #[must_use]
    pub fn authorization(mut self, role: Ident) -> Self {
        self.authorization = Some(role);
        self
    }

    /// The schema name.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateSchema, Ident};
    /// assert_eq!(CreateSchema::new(Ident::from_static("s")).name().as_str(), "s");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// Whether `IF NOT EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateSchema, Ident};
    /// assert!(!CreateSchema::new(Ident::from_static("s")).is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn is_if_not_exists(&self) -> bool {
        self.if_not_exists
    }

    /// The owning role, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateSchema, Ident};
    /// assert!(CreateSchema::new(Ident::from_static("s")).owner().is_none());
    /// ```
    #[must_use]
    pub const fn owner(&self) -> Option<&Ident> {
        self.authorization.as_ref()
    }
}

/// `DROP SCHEMA`.
///
/// ```
/// use moso_sql::ddl::DropSchema;
/// use moso_sql::Ident;
///
/// assert!(DropSchema::new(Ident::from_static("s")).if_exists().is_if_exists());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct DropSchema {
    name: Ident,
    if_exists: bool,
    cascade: bool,
}

impl DropSchema {
    /// Drops a schema.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropSchema, Ident};
    /// assert_eq!(DropSchema::new(Ident::from_static("s")).name().as_str(), "s");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self {
            name,
            if_exists: false,
            cascade: false,
        }
    }

    /// `IF EXISTS`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropSchema, Ident};
    /// assert!(DropSchema::new(Ident::from_static("s")).if_exists().is_if_exists());
    /// ```
    #[must_use]
    pub const fn if_exists(mut self) -> Self {
        self.if_exists = true;
        self
    }

    /// `CASCADE` — drop everything in the schema too.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropSchema, Ident};
    /// assert!(DropSchema::new(Ident::from_static("s")).cascade().is_cascade());
    /// ```
    #[must_use]
    pub const fn cascade(mut self) -> Self {
        self.cascade = true;
        self
    }

    /// The schema name.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropSchema, Ident};
    /// assert_eq!(DropSchema::new(Ident::from_static("s")).name().as_str(), "s");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// Whether `IF EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropSchema, Ident};
    /// assert!(!DropSchema::new(Ident::from_static("s")).is_if_exists());
    /// ```
    #[must_use]
    pub const fn is_if_exists(&self) -> bool {
        self.if_exists
    }

    /// Whether `CASCADE` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropSchema, Ident};
    /// assert!(!DropSchema::new(Ident::from_static("s")).is_cascade());
    /// ```
    #[must_use]
    pub const fn is_cascade(&self) -> bool {
        self.cascade
    }
}

/// `CREATE EXTENSION`.
///
/// Declared through the application's `database.extensions` setting; `pgcrypto`
/// for `gen_random_uuid()` and `pg_trgm` for trigram indexes are the two most
/// applications need.
///
/// ```
/// use moso_sql::ddl::CreateExtension;
/// use moso_sql::Ident;
///
/// let extension = CreateExtension::new(Ident::from_static("pg_trgm")).if_not_exists();
/// assert_eq!(extension.name().as_str(), "pg_trgm");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CreateExtension {
    name: Ident,
    if_not_exists: bool,
    schema: Option<Ident>,
    version: Option<String>,
}

impl CreateExtension {
    /// An extension.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// assert_eq!(CreateExtension::new(Ident::from_static("citext")).name().as_str(), "citext");
    /// ```
    #[must_use]
    pub const fn new(name: Ident) -> Self {
        Self {
            name,
            if_not_exists: false,
            schema: None,
            version: None,
        }
    }

    /// `IF NOT EXISTS`.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// assert!(CreateExtension::new(Ident::from_static("x")).if_not_exists().is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn if_not_exists(mut self) -> Self {
        self.if_not_exists = true;
        self
    }

    /// `SCHEMA s` — install the extension's objects into a named schema.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// let e = CreateExtension::new(Ident::from_static("x")).schema(Ident::from_static("ext"));
    /// assert!(e.target_schema().is_some());
    /// ```
    #[must_use]
    pub fn schema(mut self, schema: Ident) -> Self {
        self.schema = Some(schema);
        self
    }

    /// `VERSION '…'`.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// let e = CreateExtension::new(Ident::from_static("x")).version("1.1");
    /// assert_eq!(e.required_version(), Some("1.1"));
    /// ```
    #[must_use]
    pub fn version(mut self, version: impl Into<String>) -> Self {
        self.version = Some(version.into());
        self
    }

    /// The extension name.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// assert_eq!(CreateExtension::new(Ident::from_static("x")).name().as_str(), "x");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// Whether `IF NOT EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// assert!(!CreateExtension::new(Ident::from_static("x")).is_if_not_exists());
    /// ```
    #[must_use]
    pub const fn is_if_not_exists(&self) -> bool {
        self.if_not_exists
    }

    /// The target schema, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// assert!(CreateExtension::new(Ident::from_static("x")).target_schema().is_none());
    /// ```
    #[must_use]
    pub const fn target_schema(&self) -> Option<&Ident> {
        self.schema.as_ref()
    }

    /// The required version, if one was given.
    ///
    /// ```
    /// # use moso_sql::{ddl::CreateExtension, Ident};
    /// assert!(CreateExtension::new(Ident::from_static("x")).required_version().is_none());
    /// ```
    #[must_use]
    pub fn required_version(&self) -> Option<&str> {
        self.version.as_deref()
    }
}

/// `COMMENT ON`.
///
/// Comments are how `moso-admin` gets its field help and how a DBA reading
/// `\d+ users` in `psql` sees what the Rust doc comment said.
///
/// ```
/// use moso_sql::ddl::{CommentOn, CommentTarget};
/// use moso_sql::TableRef;
///
/// let comment = CommentOn::new(
///     CommentTarget::Table(TableRef::from_static("users")),
///     Some("Everyone who can sign in.".to_owned()),
/// );
/// assert!(comment.text().is_some());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CommentOn {
    target: CommentTarget,
    text: Option<String>,
}

impl CommentOn {
    /// Sets, or with `None` removes, a comment.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CommentOn, CommentTarget}, TableRef};
    /// let c = CommentOn::new(CommentTarget::Table(TableRef::from_static("t")), None);
    /// assert!(c.text().is_none());
    /// ```
    #[must_use]
    pub const fn new(target: CommentTarget, text: Option<String>) -> Self {
        Self { target, text }
    }

    /// What the comment is attached to.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CommentOn, CommentTarget}, TableRef};
    /// let c = CommentOn::new(CommentTarget::Table(TableRef::from_static("t")), None);
    /// assert!(matches!(c.target(), CommentTarget::Table(_)));
    /// ```
    #[must_use]
    pub const fn target(&self) -> &CommentTarget {
        &self.target
    }

    /// The comment text, if the statement sets one.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CommentOn, CommentTarget}, TableRef};
    /// let c = CommentOn::new(CommentTarget::Table(TableRef::from_static("t")), None);
    /// assert!(c.text().is_none());
    /// ```
    #[must_use]
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
}

/// What a [`CommentOn`] is attached to.
///
/// ```
/// use moso_sql::ddl::CommentTarget;
/// use moso_sql::TableRef;
///
/// assert!(matches!(CommentTarget::Table(TableRef::from_static("t")), CommentTarget::Table(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum CommentTarget {
    /// A table.
    Table(TableRef),
    /// A column of a table.
    Column {
        /// The table.
        table: TableRef,
        /// The column.
        column: Ident,
    },
    /// An index.
    Index(Ident),
    /// A user-defined type.
    Type(crate::ident::TypeRef),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_destructive_set_is_the_one_the_runner_must_block() {
        assert!(Ddl::DropTable(DropTable::new([TableRef::from_static("t")])).is_destructive());
        assert!(Ddl::Truncate(Truncate::new([TableRef::from_static("t")])).is_destructive());
        assert!(Ddl::DropSchema(DropSchema::new(Ident::from_static("s"))).is_destructive());
        assert!(Ddl::Raw(RawStatement::new("drop table users")).is_destructive());
        assert!(!Ddl::CreateTable(CreateTable::new(TableRef::from_static("t"))).is_destructive());
        assert!(!Ddl::CreateSchema(CreateSchema::new(Ident::from_static("s"))).is_destructive());
    }
}
