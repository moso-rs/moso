//! The schema snapshot: `migrations/.schema.json`.
//!
//! # Why a snapshot at all
//!
//! Django's model, not Rails'. The entities are the source of truth; the
//! snapshot records what the schema looked like as of the last generated
//! migration. Diffing entities against a *file* rather than against a live
//! database means the generator works offline, produces the same migration on
//! every developer's machine, and does not need a database that is already in
//! the right state to tell you how to get there.
//!
//! The file is committed, and it is meant to be read in review. That is why a
//! column's type is `"varchar(255)"` and not a nested object, why every map is
//! ordered, and why the JSON is pretty-printed: a schema change should show up
//! in a diff as the two lines it is.
//!
//! ```
//! use moso_migrate::schema::{Column, Schema, Table};
//! use moso_sql::DataType;
//!
//! let mut users = Table::new("users");
//! users.add_column(Column::new("id", DataType::BigSerial));
//! users.add_column(Column::new("email", DataType::Text));
//! users.set_primary_key(["id"]);
//!
//! let mut schema = Schema::empty();
//! schema.add_table(users);
//!
//! assert_eq!(schema.tables().count(), 1);
//! assert!(schema.table("users").is_some());
//! ```

mod build;
mod json;
mod types;

pub use self::build::EntityGraphError;
pub use self::types::{is_lossy, normalise_expression, parse, spell, using_expression};

use std::collections::{BTreeMap, BTreeSet};

use moso_sql::DataType;

use crate::error::Result;
use crate::hash::Checksum;

/// The format version written into every snapshot.
///
/// It is checked on read. A snapshot from a future Moso is refused with a
/// message rather than misread: silently ignoring a field a newer version added
/// would produce a migration that drops whatever that field described.
///
/// ```
/// assert_eq!(moso_migrate::schema::FORMAT_VERSION, 1);
/// ```
pub const FORMAT_VERSION: u32 = 1;

/// A whole database schema, as the entities describe it.
///
/// ```
/// use moso_migrate::Schema;
///
/// let schema = Schema::empty();
/// assert!(schema.is_empty());
/// assert_eq!(schema.format_version(), moso_migrate::schema::FORMAT_VERSION);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    /// The snapshot format, so a newer file is refused rather than misread.
    #[serde(default = "default_format_version")]
    format: u32,
    /// Named schemas that must exist before the tables in them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    schemas: BTreeSet<String>,
    /// Extensions the application declares, created before anything uses them.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    extensions: BTreeSet<String>,
    /// User-defined enum types, keyed by their qualified name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    enums: BTreeMap<String, EnumType>,
    /// Tables, keyed by their qualified name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    tables: BTreeMap<String, Table>,
}

const fn default_format_version() -> u32 {
    FORMAT_VERSION
}

impl Schema {
    /// An empty schema — the state before the first migration.
    ///
    /// ```
    /// assert!(moso_migrate::Schema::empty().is_empty());
    /// ```
    #[must_use]
    pub fn empty() -> Self {
        Self {
            format: FORMAT_VERSION,
            ..Self::default()
        }
    }

    /// The snapshot format version this schema declares.
    ///
    /// ```
    /// assert_eq!(moso_migrate::Schema::empty().format_version(), 1);
    /// ```
    #[must_use]
    pub const fn format_version(&self) -> u32 {
        self.format
    }

    /// Whether there is nothing in it.
    ///
    /// ```
    /// assert!(moso_migrate::Schema::empty().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
            && self.enums.is_empty()
            && self.extensions.is_empty()
            && self.schemas.is_empty()
    }

    /// The tables, in name order.
    ///
    /// ```
    /// # use moso_migrate::schema::{Schema, Table};
    /// let mut schema = Schema::empty();
    /// schema.add_table(Table::new("b"));
    /// schema.add_table(Table::new("a"));
    /// let names: Vec<&str> = schema.tables().map(|t| t.name()).collect();
    /// assert_eq!(names, ["a", "b"]);
    /// ```
    pub fn tables(&self) -> impl ExactSizeIterator<Item = &Table> {
        self.tables.values()
    }

    /// One table by its qualified name.
    ///
    /// ```
    /// # use moso_migrate::schema::{Schema, Table};
    /// let mut schema = Schema::empty();
    /// schema.add_table(Table::new("users"));
    /// assert!(schema.table("users").is_some());
    /// assert!(schema.table("posts").is_none());
    /// ```
    #[must_use]
    pub fn table(&self, qualified_name: &str) -> Option<&Table> {
        self.tables.get(qualified_name)
    }

    /// One table, mutably.
    ///
    /// ```
    /// # use moso_migrate::schema::{Column, Schema, Table};
    /// # use moso_sql::DataType;
    /// let mut schema = Schema::empty();
    /// schema.add_table(Table::new("users"));
    /// schema.table_mut("users").expect("added above").add_column(Column::new("id", DataType::BigSerial));
    /// assert_eq!(schema.table("users").expect("added above").columns().len(), 1);
    /// ```
    pub fn table_mut(&mut self, qualified_name: &str) -> Option<&mut Table> {
        self.tables.get_mut(qualified_name)
    }

    /// Adds a table, replacing one of the same name.
    ///
    /// ```
    /// # use moso_migrate::schema::{Schema, Table};
    /// let mut schema = Schema::empty();
    /// schema.add_table(Table::new("users"));
    /// assert_eq!(schema.tables().len(), 1);
    /// ```
    pub fn add_table(&mut self, table: Table) {
        if let Some(schema) = table.schema_name() {
            self.schemas.insert(schema.to_owned());
        }
        self.tables.insert(table.qualified_name(), table);
    }

    /// Removes a table.
    ///
    /// ```
    /// # use moso_migrate::schema::{Schema, Table};
    /// let mut schema = Schema::empty();
    /// schema.add_table(Table::new("users"));
    /// assert!(schema.remove_table("users").is_some());
    /// assert!(schema.is_empty());
    /// ```
    pub fn remove_table(&mut self, qualified_name: &str) -> Option<Table> {
        self.tables.remove(qualified_name)
    }

    /// The enum types, in name order.
    ///
    /// ```
    /// # use moso_migrate::schema::{EnumType, Schema};
    /// let mut schema = Schema::empty();
    /// schema.add_enum(EnumType::new("user_role", ["admin", "member"]));
    /// assert_eq!(schema.enums().len(), 1);
    /// ```
    pub fn enums(&self) -> impl ExactSizeIterator<Item = &EnumType> {
        self.enums.values()
    }

    /// One enum type by its qualified name.
    ///
    /// ```
    /// # use moso_migrate::schema::{EnumType, Schema};
    /// let mut schema = Schema::empty();
    /// schema.add_enum(EnumType::new("user_role", ["admin"]));
    /// assert!(schema.enum_type("user_role").is_some());
    /// ```
    #[must_use]
    pub fn enum_type(&self, qualified_name: &str) -> Option<&EnumType> {
        self.enums.get(qualified_name)
    }

    /// Adds an enum type.
    ///
    /// ```
    /// # use moso_migrate::schema::{EnumType, Schema};
    /// let mut schema = Schema::empty();
    /// schema.add_enum(EnumType::new("user_role", ["admin"]));
    /// assert!(!schema.is_empty());
    /// ```
    pub fn add_enum(&mut self, enum_type: EnumType) {
        if let Some(schema) = enum_type.schema_name() {
            self.schemas.insert(schema.to_owned());
        }
        self.enums.insert(enum_type.qualified_name(), enum_type);
    }

    /// Removes an enum type.
    ///
    /// ```
    /// # use moso_migrate::schema::{EnumType, Schema};
    /// let mut schema = Schema::empty();
    /// schema.add_enum(EnumType::new("user_role", ["admin"]));
    /// assert!(schema.remove_enum("user_role").is_some());
    /// ```
    pub fn remove_enum(&mut self, qualified_name: &str) -> Option<EnumType> {
        self.enums.remove(qualified_name)
    }

    /// The named schemas, in order.
    ///
    /// ```
    /// # use moso_migrate::Schema;
    /// let mut schema = Schema::empty();
    /// schema.add_schema("analytics");
    /// assert_eq!(schema.schemas().collect::<Vec<_>>(), ["analytics"]);
    /// ```
    pub fn schemas(&self) -> impl ExactSizeIterator<Item = &str> {
        self.schemas.iter().map(String::as_str)
    }

    /// Declares a named schema.
    ///
    /// ```
    /// # use moso_migrate::Schema;
    /// let mut schema = Schema::empty();
    /// schema.add_schema("analytics");
    /// assert!(!schema.is_empty());
    /// ```
    pub fn add_schema(&mut self, name: impl Into<String>) {
        self.schemas.insert(name.into());
    }

    /// The declared extensions, in order.
    ///
    /// ```
    /// # use moso_migrate::Schema;
    /// let mut schema = Schema::empty();
    /// schema.add_extension("pg_trgm");
    /// assert_eq!(schema.extensions().collect::<Vec<_>>(), ["pg_trgm"]);
    /// ```
    pub fn extensions(&self) -> impl ExactSizeIterator<Item = &str> {
        self.extensions.iter().map(String::as_str)
    }

    /// Declares an extension, which the generator creates before anything that
    /// could need it.
    ///
    /// ```
    /// # use moso_migrate::Schema;
    /// let mut schema = Schema::empty();
    /// schema.add_extension("pgcrypto");
    /// assert_eq!(schema.extensions().len(), 1);
    /// ```
    pub fn add_extension(&mut self, name: impl Into<String>) {
        self.extensions.insert(name.into());
    }

    /// A content hash of the whole schema.
    ///
    /// This is the `a91f2c` in a generated migration's
    /// `-- moso:generated-from .schema.json@a91f2c` header, and it is what makes
    /// "this migration was generated from a different snapshot than the one on
    /// disk" a detectable condition rather than a mystery.
    ///
    /// ```
    /// # use moso_migrate::schema::{Schema, Table};
    /// let mut a = Schema::empty();
    /// let b = Schema::empty();
    /// assert_eq!(a.checksum(), b.checksum());
    /// a.add_table(Table::new("users"));
    /// assert_ne!(a.checksum(), b.checksum());
    /// ```
    #[must_use]
    pub fn checksum(&self) -> Checksum {
        Checksum::of(self.to_json().as_bytes())
    }

    /// The tables in an order where a table's foreign-key targets come first.
    ///
    /// A cycle — two tables referring to each other — cannot be topologically
    /// sorted, and is not an error: the tables come out in name order and the
    /// generator emits the foreign keys as separate `ALTER TABLE` statements
    /// after every `CREATE TABLE`. That is the correct SQL for a cycle anyway.
    ///
    /// ```
    /// use moso_migrate::schema::{ForeignKey, Schema, Table};
    ///
    /// let mut posts = Table::new("posts");
    /// posts.add_foreign_key(ForeignKey::new("posts_author_fkey", ["author_id"], "users", ["id"]));
    ///
    /// let mut schema = Schema::empty();
    /// schema.add_table(posts);
    /// schema.add_table(Table::new("users"));
    ///
    /// let order: Vec<&str> = schema.creation_order().iter().map(|t| t.name()).collect();
    /// assert_eq!(order, ["users", "posts"]);
    /// ```
    #[must_use]
    pub fn creation_order(&self) -> Vec<&Table> {
        let mut visited: BTreeSet<&str> = BTreeSet::new();
        let mut in_progress: BTreeSet<&str> = BTreeSet::new();
        let mut order: Vec<&Table> = Vec::with_capacity(self.tables.len());

        for name in self.tables.keys() {
            self.visit(name, &mut visited, &mut in_progress, &mut order);
        }
        order
    }

    fn visit<'a>(
        &'a self,
        name: &'a str,
        visited: &mut BTreeSet<&'a str>,
        in_progress: &mut BTreeSet<&'a str>,
        order: &mut Vec<&'a Table>,
    ) {
        if visited.contains(name) || in_progress.contains(name) {
            return;
        }
        let Some((key, table)) = self.tables.get_key_value(name) else {
            return;
        };
        in_progress.insert(key.as_str());
        for foreign_key in table.foreign_keys() {
            // A self-reference is not a dependency on another table.
            if foreign_key.target_table() != name {
                self.visit(foreign_key.target_table(), visited, in_progress, order);
            }
        }
        in_progress.remove(key.as_str());
        visited.insert(key.as_str());
        order.push(table);
    }

    /// Serialises to the exact bytes that belong in `migrations/.schema.json`.
    ///
    /// Pretty-printed with a trailing newline, because it is a file people read
    /// and `git diff` should show one changed line for one changed column.
    ///
    /// ```
    /// let json = moso_migrate::Schema::empty().to_json();
    /// assert!(json.ends_with('\n'));
    /// assert!(json.contains("\"format\": 1"));
    /// ```
    #[must_use]
    pub fn to_json(&self) -> String {
        json::to_json(self)
    }

    /// Parses `migrations/.schema.json`.
    ///
    /// # Errors
    ///
    /// [`Error::Snapshot`](crate::Error::Snapshot) when the JSON is malformed
    /// or its `format` is newer than this build.
    ///
    /// ```
    /// use moso_migrate::Schema;
    ///
    /// let original = Schema::empty();
    /// assert_eq!(Schema::from_json(&original.to_json())?, original);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn from_json(text: &str) -> Result<Self> {
        json::from_json(text)
    }
}

/// One table.
///
/// ```
/// use moso_migrate::schema::{Column, Table};
/// use moso_sql::DataType;
///
/// let mut users = Table::new("users");
/// users.add_column(Column::new("id", DataType::BigSerial));
/// users.set_primary_key(["id"]);
/// assert_eq!(users.primary_key(), ["id"]);
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    /// The Rust type this came from. Not part of the schema; used to say
    /// "`User` gained a field" instead of "`users` gained a column".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entity: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    columns: Vec<Column>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    primary_key: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    indexes: BTreeMap<String, Index>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    foreign_keys: BTreeMap<String, ForeignKey>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    checks: BTreeMap<String, Check>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    partition_by: Option<Partition>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
}

impl Table {
    /// An empty table with a name.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").name(), "users");
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    /// Puts the table in a named schema.
    ///
    /// ```
    /// let table = moso_migrate::schema::Table::new("events").in_schema("analytics");
    /// assert_eq!(table.qualified_name(), "analytics.events");
    /// ```
    #[must_use]
    pub fn in_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// Records which Rust type the table came from.
    ///
    /// ```
    /// let table = moso_migrate::schema::Table::new("users").for_entity("User");
    /// assert_eq!(table.entity(), Some("User"));
    /// ```
    #[must_use]
    pub fn for_entity(mut self, entity: impl Into<String>) -> Self {
        self.entity = Some(entity.into());
        self
    }

    /// The unqualified name.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").name(), "users");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The schema it lives in, if it is not the default one.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").schema_name(), None);
    /// ```
    #[must_use]
    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// `schema.name`, or just `name`.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").qualified_name(), "users");
    /// ```
    #[must_use]
    pub fn qualified_name(&self) -> String {
        qualify(self.schema.as_deref(), &self.name)
    }

    /// The Rust type, when it is known.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").entity(), None);
    /// ```
    #[must_use]
    pub fn entity(&self) -> Option<&str> {
        self.entity.as_deref()
    }

    /// How error messages should name this table: the entity if there is one,
    /// the table name otherwise.
    ///
    /// ```
    /// # use moso_migrate::schema::Table;
    /// assert_eq!(Table::new("users").for_entity("User").label(), "User");
    /// assert_eq!(Table::new("users").label(), "users");
    /// ```
    #[must_use]
    pub fn label(&self) -> &str {
        self.entity.as_deref().unwrap_or(&self.name)
    }

    /// The columns, in declaration order.
    ///
    /// ```
    /// assert!(moso_migrate::schema::Table::new("users").columns().is_empty());
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// One column by name.
    ///
    /// ```
    /// # use moso_migrate::schema::{Column, Table};
    /// # use moso_sql::DataType;
    /// let mut table = Table::new("users");
    /// table.add_column(Column::new("email", DataType::Text));
    /// assert!(table.column("email").is_some());
    /// ```
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|column| column.name() == name)
    }

    /// Appends a column, replacing one of the same name in place.
    ///
    /// ```
    /// # use moso_migrate::schema::{Column, Table};
    /// # use moso_sql::DataType;
    /// let mut table = Table::new("users");
    /// table.add_column(Column::new("email", DataType::Text));
    /// table.add_column(Column::new("email", DataType::VarChar(Some(320))));
    /// assert_eq!(table.columns().len(), 1);
    /// ```
    pub fn add_column(&mut self, column: Column) {
        match self
            .columns
            .iter_mut()
            .find(|existing| existing.name() == column.name())
        {
            Some(existing) => *existing = column,
            None => self.columns.push(column),
        }
    }

    /// Removes a column.
    ///
    /// ```
    /// # use moso_migrate::schema::{Column, Table};
    /// # use moso_sql::DataType;
    /// let mut table = Table::new("users");
    /// table.add_column(Column::new("email", DataType::Text));
    /// assert!(table.remove_column("email").is_some());
    /// ```
    pub fn remove_column(&mut self, name: &str) -> Option<Column> {
        let at = self
            .columns
            .iter()
            .position(|column| column.name() == name)?;
        Some(self.columns.remove(at))
    }

    /// The primary-key columns, in key order.
    ///
    /// ```
    /// assert!(moso_migrate::schema::Table::new("users").primary_key().is_empty());
    /// ```
    #[must_use]
    pub fn primary_key(&self) -> &[String] {
        &self.primary_key
    }

    /// Sets the primary key.
    ///
    /// ```
    /// # use moso_migrate::schema::Table;
    /// let mut table = Table::new("order_lines");
    /// table.set_primary_key(["order_id", "line_no"]);
    /// assert_eq!(table.primary_key().len(), 2);
    /// ```
    pub fn set_primary_key(&mut self, columns: impl IntoIterator<Item = impl Into<String>>) {
        self.primary_key = columns.into_iter().map(Into::into).collect();
    }

    /// The indexes, in name order. A `UNIQUE` constraint is an index here,
    /// because that is what every supported database makes it.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").indexes().len(), 0);
    /// ```
    pub fn indexes(&self) -> impl ExactSizeIterator<Item = &Index> {
        self.indexes.values()
    }

    /// One index by name.
    ///
    /// ```
    /// # use moso_migrate::schema::{Index, Table};
    /// let mut table = Table::new("users");
    /// table.add_index(Index::new("idx_users_email", ["email"]));
    /// assert!(table.index("idx_users_email").is_some());
    /// ```
    #[must_use]
    pub fn index(&self, name: &str) -> Option<&Index> {
        self.indexes.get(name)
    }

    /// Adds an index.
    ///
    /// ```
    /// # use moso_migrate::schema::{Index, Table};
    /// let mut table = Table::new("users");
    /// table.add_index(Index::new("idx_users_email", ["email"]).unique());
    /// assert!(table.index("idx_users_email").expect("just added").is_unique());
    /// ```
    pub fn add_index(&mut self, index: Index) {
        self.indexes.insert(index.name().to_owned(), index);
    }

    /// The foreign keys, in name order.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("posts").foreign_keys().len(), 0);
    /// ```
    pub fn foreign_keys(&self) -> impl ExactSizeIterator<Item = &ForeignKey> {
        self.foreign_keys.values()
    }

    /// One foreign key by name.
    ///
    /// ```
    /// # use moso_migrate::schema::{ForeignKey, Table};
    /// let mut table = Table::new("posts");
    /// table.add_foreign_key(ForeignKey::new("fk", ["author_id"], "users", ["id"]));
    /// assert!(table.foreign_key("fk").is_some());
    /// ```
    #[must_use]
    pub fn foreign_key(&self, name: &str) -> Option<&ForeignKey> {
        self.foreign_keys.get(name)
    }

    /// Adds a foreign key.
    ///
    /// ```
    /// # use moso_migrate::schema::{ForeignKey, Table};
    /// let mut table = Table::new("posts");
    /// table.add_foreign_key(ForeignKey::new("fk", ["author_id"], "users", ["id"]));
    /// assert_eq!(table.foreign_keys().len(), 1);
    /// ```
    pub fn add_foreign_key(&mut self, foreign_key: ForeignKey) {
        self.foreign_keys
            .insert(foreign_key.name().to_owned(), foreign_key);
    }

    /// The check constraints, in name order.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Table::new("users").checks().len(), 0);
    /// ```
    pub fn checks(&self) -> impl ExactSizeIterator<Item = &Check> {
        self.checks.values()
    }

    /// One check constraint by name.
    ///
    /// ```
    /// # use moso_migrate::schema::{Check, Table};
    /// let mut table = Table::new("users");
    /// table.add_check(Check::new("age_positive", "age > 0"));
    /// assert!(table.check("age_positive").is_some());
    /// ```
    #[must_use]
    pub fn check(&self, name: &str) -> Option<&Check> {
        self.checks.get(name)
    }

    /// Adds a check constraint.
    ///
    /// ```
    /// # use moso_migrate::schema::{Check, Table};
    /// let mut table = Table::new("users");
    /// table.add_check(Check::new("age_positive", "age > 0"));
    /// assert_eq!(table.checks().len(), 1);
    /// ```
    pub fn add_check(&mut self, check: Check) {
        self.checks.insert(check.name().to_owned(), check);
    }

    /// The partitioning declaration, if the table is partitioned.
    ///
    /// ```
    /// assert!(moso_migrate::schema::Table::new("events").partitioning().is_none());
    /// ```
    #[must_use]
    pub const fn partitioning(&self) -> Option<&Partition> {
        self.partition_by.as_ref()
    }

    /// Declares partitioning.
    ///
    /// ```
    /// # use moso_migrate::schema::{Partition, Table};
    /// let table = Table::new("events").partitioned_by(Partition::range(["created_at"]));
    /// assert!(table.partitioning().is_some());
    /// ```
    #[must_use]
    pub fn partitioned_by(mut self, partition: Partition) -> Self {
        self.partition_by = Some(partition);
        self
    }

    /// The table comment.
    ///
    /// ```
    /// assert!(moso_migrate::schema::Table::new("users").comment().is_none());
    /// ```
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Sets the table comment.
    ///
    /// ```
    /// # use moso_migrate::schema::Table;
    /// let table = Table::new("users").with_comment("People who can log in");
    /// assert!(table.comment().is_some());
    /// ```
    #[must_use]
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }
}

/// One column.
///
/// ```
/// use moso_migrate::schema::Column;
/// use moso_sql::DataType;
///
/// let locale = Column::new("locale", DataType::Text).with_default("'en'");
/// assert!(!locale.is_nullable());
/// assert_eq!(locale.default(), Some("'en'"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    name: String,
    /// The canonical spelling. Stored as a string so `.schema.json` reads like
    /// SQL; see [`spell`].
    #[serde(rename = "type")]
    data_type: String,
    /// The Rust field, for messages that name the user's code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    field: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    nullable: bool,
    /// The default, as SQL text: `'en'`, `now()`, `0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generated: Option<Generated>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    identity: Option<IdentityKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
}

const fn is_false(value: &bool) -> bool {
    !*value
}

impl Column {
    /// A `NOT NULL` column with no default.
    ///
    /// Not-null is the default because an entity field is `T` unless it is
    /// `Option<T>`, and the schema should follow the type rather than the other
    /// way round.
    ///
    /// ```
    /// use moso_migrate::schema::Column;
    /// use moso_sql::DataType;
    ///
    /// assert!(!Column::new("id", DataType::BigSerial).is_nullable());
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>, data_type: DataType) -> Self {
        Self {
            name: name.into(),
            data_type: spell(&data_type),
            field: None,
            nullable: false,
            default: None,
            generated: None,
            identity: None,
            collation: None,
            comment: None,
        }
    }

    /// Makes the column nullable.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("bio", DataType::Text).nullable().is_nullable());
    /// ```
    #[must_use]
    pub const fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    /// Sets the default, as SQL text.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let created = Column::new("created_at", DataType::Timestamp { with_time_zone: true })
    ///     .with_default("now()");
    /// assert_eq!(created.default(), Some("now()"));
    /// ```
    #[must_use]
    pub fn with_default(mut self, sql: impl Into<String>) -> Self {
        self.default = Some(sql.into());
        self
    }

    /// Records the Rust field this column came from.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let column = Column::new("created_at", DataType::Date).for_field("created");
    /// assert_eq!(column.field(), Some("created"));
    /// ```
    #[must_use]
    pub fn for_field(mut self, field: impl Into<String>) -> Self {
        self.field = Some(field.into());
        self
    }

    /// Makes the column generated.
    ///
    /// ```
    /// # use moso_migrate::schema::{Column, Generated};
    /// # use moso_sql::DataType;
    /// let column = Column::new("search", DataType::TsVector)
    ///     .generated_as(Generated::stored("to_tsvector('english', title)"));
    /// assert!(column.generation().is_some());
    /// ```
    #[must_use]
    pub fn generated_as(mut self, generated: Generated) -> Self {
        self.generated = Some(generated);
        self
    }

    /// Makes the column an identity column.
    ///
    /// ```
    /// # use moso_migrate::schema::{Column, IdentityKind};
    /// # use moso_sql::DataType;
    /// let column = Column::new("id", DataType::BigInt).identity(IdentityKind::Always);
    /// assert_eq!(column.identity_kind(), Some(IdentityKind::Always));
    /// ```
    #[must_use]
    pub const fn identity(mut self, kind: IdentityKind) -> Self {
        self.identity = Some(kind);
        self
    }

    /// Sets the collation.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let column = Column::new("name", DataType::Text).collate("C");
    /// assert_eq!(column.collation(), Some("C"));
    /// ```
    #[must_use]
    pub fn collate(mut self, collation: impl Into<String>) -> Self {
        self.collation = Some(collation.into());
        self
    }

    /// Sets the column comment.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let column = Column::new("locale", DataType::Text).with_comment("BCP 47");
    /// assert_eq!(column.comment(), Some("BCP 47"));
    /// ```
    #[must_use]
    pub fn with_comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = Some(comment.into());
        self
    }

    /// The column name.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert_eq!(Column::new("id", DataType::BigSerial).name(), "id");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The canonical type spelling, as stored.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert_eq!(Column::new("id", DataType::BigSerial).type_name(), "bigserial");
    /// ```
    #[must_use]
    pub fn type_name(&self) -> &str {
        &self.data_type
    }

    /// The parsed type.
    ///
    /// # Errors
    ///
    /// [`Error::Snapshot`](crate::Error::Snapshot) when the snapshot names a
    /// type this build does not know.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert_eq!(Column::new("id", DataType::BigSerial).data_type()?, DataType::BigSerial);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn data_type(&self) -> Result<DataType> {
        parse(&self.data_type)
    }

    /// The Rust field, when it is known.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert_eq!(Column::new("id", DataType::BigSerial).field(), None);
    /// ```
    #[must_use]
    pub fn field(&self) -> Option<&str> {
        self.field.as_deref()
    }

    /// How a message should name this column: the Rust field if there is one.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let column = Column::new("created_at", DataType::Date).for_field("created");
    /// assert_eq!(column.label(), "created");
    /// ```
    #[must_use]
    pub fn label(&self) -> &str {
        self.field.as_deref().unwrap_or(&self.name)
    }

    /// Whether it accepts `NULL`.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("bio", DataType::Text).nullable().is_nullable());
    /// ```
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// The default, as SQL text.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert_eq!(Column::new("n", DataType::Integer).default(), None);
    /// ```
    #[must_use]
    pub fn default(&self) -> Option<&str> {
        self.default.as_deref()
    }

    /// The generation expression, if the column is generated.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("n", DataType::Integer).generation().is_none());
    /// ```
    #[must_use]
    pub const fn generation(&self) -> Option<&Generated> {
        self.generated.as_ref()
    }

    /// The identity kind, if the column is an identity column.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("n", DataType::Integer).identity_kind().is_none());
    /// ```
    #[must_use]
    pub const fn identity_kind(&self) -> Option<IdentityKind> {
        self.identity
    }

    /// The collation.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("n", DataType::Text).collation().is_none());
    /// ```
    #[must_use]
    pub fn collation(&self) -> Option<&str> {
        self.collation.as_deref()
    }

    /// The column comment.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("n", DataType::Integer).comment().is_none());
    /// ```
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// Whether the column carries its own sequence, so `ADD COLUMN` must not
    /// ask for a fill value.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("id", DataType::BigSerial).is_auto_populated());
    /// assert!(!Column::new("n", DataType::BigInt).is_auto_populated());
    /// ```
    #[must_use]
    pub fn is_auto_populated(&self) -> bool {
        self.identity.is_some()
            || self.generated.is_some()
            || self
                .data_type()
                .is_ok_and(|data_type| data_type.is_auto_increment())
    }

    /// Whether adding this column to a table with rows needs a fill value.
    ///
    /// The one case the operation table marks ⚠: `NOT NULL` with no default and
    /// nothing to generate the value from.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert!(Column::new("locale", DataType::Text).needs_a_fill_value());
    /// assert!(!Column::new("locale", DataType::Text).with_default("'en'").needs_a_fill_value());
    /// assert!(!Column::new("bio", DataType::Text).nullable().needs_a_fill_value());
    /// ```
    #[must_use]
    pub fn needs_a_fill_value(&self) -> bool {
        !self.nullable && self.default.is_none() && !self.is_auto_populated()
    }

    /// Replaces the type, keeping everything else.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let column = Column::new("n", DataType::Integer).with_type(DataType::BigInt);
    /// assert_eq!(column.type_name(), "bigint");
    /// ```
    #[must_use]
    pub fn with_type(mut self, data_type: DataType) -> Self {
        self.data_type = spell(&data_type);
        self
    }

    /// Renames the column, keeping everything else.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// assert_eq!(Column::new("name", DataType::Text).renamed_to("full_name").name(), "full_name");
    /// ```
    #[must_use]
    pub fn renamed_to(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Whether two columns differ in anything but their name.
    ///
    /// This is the predicate rename detection runs on: a dropped column and an
    /// added column that are otherwise identical are almost certainly one
    /// renamed column.
    ///
    /// ```
    /// # use moso_migrate::schema::Column;
    /// # use moso_sql::DataType;
    /// let before = Column::new("name", DataType::Text);
    /// let after = Column::new("full_name", DataType::Text);
    /// assert!(before.matches_ignoring_name(&after));
    /// ```
    #[must_use]
    pub fn matches_ignoring_name(&self, other: &Self) -> bool {
        self.data_type == other.data_type
            && self.nullable == other.nullable
            && self.default == other.default
            && self.generated == other.generated
            && self.identity == other.identity
            && self.collation == other.collation
    }
}

/// A generated column's expression.
///
/// ```
/// use moso_migrate::schema::Generated;
///
/// let search = Generated::stored("to_tsvector('english', title)");
/// assert!(search.is_stored());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generated {
    expression: String,
    #[serde(default = "default_true")]
    stored: bool,
}

const fn default_true() -> bool {
    true
}

impl Generated {
    /// `GENERATED ALWAYS AS (…) STORED` — the only form PostgreSQL has.
    ///
    /// ```
    /// assert!(moso_migrate::schema::Generated::stored("1 + 1").is_stored());
    /// ```
    #[must_use]
    pub fn stored(expression: impl Into<String>) -> Self {
        Self {
            expression: expression.into(),
            stored: true,
        }
    }

    /// `GENERATED ALWAYS AS (…) VIRTUAL` — SQLite only.
    ///
    /// ```
    /// assert!(!moso_migrate::schema::Generated::virtual_("1 + 1").is_stored());
    /// ```
    #[must_use]
    pub fn virtual_(expression: impl Into<String>) -> Self {
        Self {
            expression: expression.into(),
            stored: false,
        }
    }

    /// The expression, as SQL text.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Generated::stored("1 + 1").expression(), "1 + 1");
    /// ```
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }

    /// Whether the value is materialised.
    ///
    /// ```
    /// assert!(moso_migrate::schema::Generated::stored("1").is_stored());
    /// ```
    #[must_use]
    pub const fn is_stored(&self) -> bool {
        self.stored
    }
}

/// Which form of `GENERATED … AS IDENTITY` a column uses.
///
/// ```
/// use moso_migrate::schema::IdentityKind;
///
/// assert_ne!(IdentityKind::Always, IdentityKind::ByDefault);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IdentityKind {
    /// `GENERATED ALWAYS AS IDENTITY`.
    Always,
    /// `GENERATED BY DEFAULT AS IDENTITY`.
    ByDefault,
}

/// An index, which is also how a `UNIQUE` constraint is recorded.
///
/// ```
/// use moso_migrate::schema::Index;
///
/// let email = Index::new("users_email_key", ["email"]).unique();
/// assert!(email.is_unique());
/// assert_eq!(email.columns().len(), 1);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Index {
    name: String,
    columns: Vec<IndexPart>,
    #[serde(default, skip_serializing_if = "is_false")]
    unique: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    method: Option<String>,
    /// A partial index's predicate, as SQL text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    predicate: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    include: Vec<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    nulls_not_distinct: bool,
    /// Whether this index backs a `UNIQUE`/`PRIMARY KEY` *constraint* rather
    /// than standing on its own. The distinction matters at drop time: you
    /// cannot `DROP INDEX` an index a constraint owns.
    #[serde(default, skip_serializing_if = "is_false")]
    constraint: bool,
}

impl Index {
    /// A non-unique index over columns.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(!Index::new("idx_users_email", ["email"]).is_unique());
    /// ```
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        columns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            columns: columns.into_iter().map(IndexPart::column).collect(),
            unique: false,
            method: None,
            predicate: None,
            include: Vec::new(),
            nulls_not_distinct: false,
            constraint: false,
        }
    }

    /// An index over arbitrary parts: expressions, sort orders, operator
    /// classes.
    ///
    /// ```
    /// # use moso_migrate::schema::{Index, IndexPart, Sort};
    /// let index = Index::over("idx_posts_recent", [IndexPart::column("created_at").sorted(Sort::Desc)]);
    /// assert_eq!(index.columns()[0].sort(), Some(Sort::Desc));
    /// ```
    #[must_use]
    pub fn over(name: impl Into<String>, parts: impl IntoIterator<Item = IndexPart>) -> Self {
        Self {
            name: name.into(),
            columns: parts.into_iter().collect(),
            unique: false,
            method: None,
            predicate: None,
            include: Vec::new(),
            nulls_not_distinct: false,
            constraint: false,
        }
    }

    /// Makes it unique.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(Index::new("k", ["email"]).unique().is_unique());
    /// ```
    #[must_use]
    pub const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Marks it as the index a `UNIQUE` or `PRIMARY KEY` constraint owns.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(Index::new("k", ["email"]).unique().backing_a_constraint().backs_a_constraint());
    /// ```
    #[must_use]
    pub const fn backing_a_constraint(mut self) -> Self {
        self.constraint = true;
        self
    }

    /// Sets the index method: `btree`, `gin`, `gist`, `hash`, `brin`, `spgist`.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert_eq!(Index::new("k", ["doc"]).using("gin").method(), Some("gin"));
    /// ```
    #[must_use]
    pub fn using(mut self, method: impl Into<String>) -> Self {
        self.method = Some(method.into());
        self
    }

    /// Makes it partial.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// let index = Index::new("k", ["email"]).unique().r#where("deleted_at is null");
    /// assert!(index.predicate().is_some());
    /// ```
    #[must_use]
    pub fn r#where(mut self, predicate: impl Into<String>) -> Self {
        self.predicate = Some(predicate.into());
        self
    }

    /// Adds non-key `INCLUDE` columns.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// let index = Index::new("k", ["author_id"]).include(["title"]);
    /// assert_eq!(index.included(), ["title"]);
    /// ```
    #[must_use]
    pub fn include(mut self, columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.include = columns.into_iter().map(Into::into).collect();
        self
    }

    /// `NULLS NOT DISTINCT`.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(Index::new("k", ["a"]).unique().nulls_not_distinct().has_nulls_not_distinct());
    /// ```
    #[must_use]
    pub const fn nulls_not_distinct(mut self) -> Self {
        self.nulls_not_distinct = true;
        self
    }

    /// The index name.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert_eq!(Index::new("k", ["a"]).name(), "k");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The key parts, in order.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert_eq!(Index::new("k", ["a", "b"]).columns().len(), 2);
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[IndexPart] {
        &self.columns
    }

    /// Whether it is unique.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(!Index::new("k", ["a"]).is_unique());
    /// ```
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// Whether a constraint owns it.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(!Index::new("k", ["a"]).backs_a_constraint());
    /// ```
    #[must_use]
    pub const fn backs_a_constraint(&self) -> bool {
        self.constraint
    }

    /// The index method.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert_eq!(Index::new("k", ["a"]).method(), None);
    /// ```
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    /// The partial-index predicate.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert_eq!(Index::new("k", ["a"]).predicate(), None);
    /// ```
    #[must_use]
    pub fn predicate(&self) -> Option<&str> {
        self.predicate.as_deref()
    }

    /// The `INCLUDE` columns.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(Index::new("k", ["a"]).included().is_empty());
    /// ```
    #[must_use]
    pub fn included(&self) -> &[String] {
        &self.include
    }

    /// Whether `NULLS NOT DISTINCT` is set.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(!Index::new("k", ["a"]).has_nulls_not_distinct());
    /// ```
    #[must_use]
    pub const fn has_nulls_not_distinct(&self) -> bool {
        self.nulls_not_distinct
    }

    /// Renames it, keeping everything else.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert_eq!(Index::new("old", ["a"]).renamed_to("new").name(), "new");
    /// ```
    #[must_use]
    pub fn renamed_to(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Whether two indexes differ in anything but their name.
    ///
    /// ```
    /// # use moso_migrate::schema::Index;
    /// assert!(Index::new("a", ["email"]).matches_ignoring_name(&Index::new("b", ["email"])));
    /// ```
    #[must_use]
    pub fn matches_ignoring_name(&self, other: &Self) -> bool {
        self.columns == other.columns
            && self.unique == other.unique
            && self.method == other.method
            && self.predicate == other.predicate
            && self.include == other.include
            && self.nulls_not_distinct == other.nulls_not_distinct
    }
}

/// One key part of an index: a column or an expression, with its sort order.
///
/// ```
/// use moso_migrate::schema::{IndexPart, Sort};
///
/// let recent = IndexPart::column("created_at").sorted(Sort::Desc);
/// assert_eq!(recent.column_name(), Some("created_at"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexPart {
    /// A bare column name, or an expression in parentheses.
    expr: String,
    /// `true` when `expr` is a column name rather than an expression.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    column: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    sort: Option<Sort>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    nulls: Option<NullsOrder>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ops: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collation: Option<String>,
}

const fn is_true(value: &bool) -> bool {
    *value
}

impl IndexPart {
    /// A plain column.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::IndexPart::column("email").column_name(), Some("email"));
    /// ```
    #[must_use]
    pub fn column(name: impl Into<String>) -> Self {
        Self {
            expr: name.into(),
            column: true,
            sort: None,
            nulls: None,
            ops: None,
            collation: None,
        }
    }

    /// An expression, as SQL text.
    ///
    /// ```
    /// let part = moso_migrate::schema::IndexPart::expression("lower(email)");
    /// assert_eq!(part.column_name(), None);
    /// ```
    #[must_use]
    pub fn expression(sql: impl Into<String>) -> Self {
        Self {
            expr: sql.into(),
            column: false,
            sort: None,
            nulls: None,
            ops: None,
            collation: None,
        }
    }

    /// Sets the sort direction.
    ///
    /// `ASC` is stored as "unset", because it is SQL's default and because a
    /// database reports it as no option at all. Recording it would make every
    /// such index look permanently drifted.
    ///
    /// ```
    /// # use moso_migrate::schema::{IndexPart, Sort};
    /// assert_eq!(IndexPart::column("a").sorted(Sort::Desc).sort(), Some(Sort::Desc));
    /// assert_eq!(IndexPart::column("a").sorted(Sort::Asc).sort(), None);
    /// ```
    #[must_use]
    pub fn sorted(mut self, sort: Sort) -> Self {
        self.sort = Some(sort);
        self.normalised()
    }

    /// Sets where nulls go.
    ///
    /// As with the direction, the value SQL would have chosen anyway is stored
    /// as "unset": `NULLS LAST` for an ascending key, `NULLS FIRST` for a
    /// descending one.
    ///
    /// ```
    /// # use moso_migrate::schema::{IndexPart, NullsOrder, Sort};
    /// // The default for an ascending key, so it is not recorded.
    /// assert_eq!(IndexPart::column("a").nulls(NullsOrder::Last).nulls_order(), None);
    /// // Not the default, so it is.
    /// assert_eq!(
    ///     IndexPart::column("a").nulls(NullsOrder::First).nulls_order(),
    ///     Some(NullsOrder::First),
    /// );
    /// assert_eq!(
    ///     IndexPart::column("a").sorted(Sort::Desc).nulls(NullsOrder::First).nulls_order(),
    ///     None,
    /// );
    /// ```
    #[must_use]
    pub fn nulls(mut self, nulls: NullsOrder) -> Self {
        self.nulls = Some(nulls);
        self.normalised()
    }

    /// Drops the direction and the nulls placement when they are what SQL would
    /// do anyway, so that the snapshot records only what differs.
    fn normalised(mut self) -> Self {
        if self.sort == Some(Sort::Asc) {
            self.sort = None;
        }
        let default_nulls = if self.sort == Some(Sort::Desc) {
            NullsOrder::First
        } else {
            NullsOrder::Last
        };
        if self.nulls == Some(default_nulls) {
            self.nulls = None;
        }
        self
    }

    /// Sets the operator class: `gin_trgm_ops`, `jsonb_path_ops`.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::column("doc").operator_class("jsonb_path_ops").ops(), Some("jsonb_path_ops"));
    /// ```
    #[must_use]
    pub fn operator_class(mut self, ops: impl Into<String>) -> Self {
        self.ops = Some(ops.into());
        self
    }

    /// Sets the collation.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::column("name").collate("C").collation(), Some("C"));
    /// ```
    #[must_use]
    pub fn collate(mut self, collation: impl Into<String>) -> Self {
        self.collation = Some(collation.into());
        self
    }

    /// The column name, if this part is a plain column.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::expression("lower(a)").column_name(), None);
    /// ```
    #[must_use]
    pub fn column_name(&self) -> Option<&str> {
        self.column.then_some(self.expr.as_str())
    }

    /// The raw text — a column name or an expression.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::expression("lower(a)").expr(), "lower(a)");
    /// ```
    #[must_use]
    pub fn expr(&self) -> &str {
        &self.expr
    }

    /// Whether it is a plain column rather than an expression.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert!(IndexPart::column("a").is_column());
    /// ```
    #[must_use]
    pub const fn is_column(&self) -> bool {
        self.column
    }

    /// The sort direction.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::column("a").sort(), None);
    /// ```
    #[must_use]
    pub const fn sort(&self) -> Option<Sort> {
        self.sort
    }

    /// Where nulls go.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::column("a").nulls_order(), None);
    /// ```
    #[must_use]
    pub const fn nulls_order(&self) -> Option<NullsOrder> {
        self.nulls
    }

    /// The operator class.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::column("a").ops(), None);
    /// ```
    #[must_use]
    pub fn ops(&self) -> Option<&str> {
        self.ops.as_deref()
    }

    /// The collation.
    ///
    /// ```
    /// # use moso_migrate::schema::IndexPart;
    /// assert_eq!(IndexPart::column("a").collation(), None);
    /// ```
    #[must_use]
    pub fn collation(&self) -> Option<&str> {
        self.collation.as_deref()
    }
}

/// A sort direction.
///
/// ```
/// assert_ne!(moso_migrate::schema::Sort::Asc, moso_migrate::schema::Sort::Desc);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sort {
    /// Ascending.
    Asc,
    /// Descending.
    Desc,
}

/// Where `NULL`s sort.
///
/// ```
/// assert_ne!(moso_migrate::schema::NullsOrder::First, moso_migrate::schema::NullsOrder::Last);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NullsOrder {
    /// `NULLS FIRST`.
    First,
    /// `NULLS LAST`.
    Last,
}

/// A foreign-key constraint.
///
/// ```
/// use moso_migrate::schema::ForeignKey;
///
/// let author = ForeignKey::new("posts_author_id_fkey", ["author_id"], "users", ["id"]);
/// assert_eq!(author.target_table(), "users");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    name: String,
    columns: Vec<String>,
    target: String,
    target_columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_delete: Option<Action>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    on_update: Option<Action>,
    #[serde(default, skip_serializing_if = "is_false")]
    deferrable: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    initially_deferred: bool,
}

impl ForeignKey {
    /// A foreign key with no referential actions.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// let fk = ForeignKey::new("fk", ["author_id"], "users", ["id"]);
    /// assert_eq!(fk.columns(), ["author_id"]);
    /// ```
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        columns: impl IntoIterator<Item = impl Into<String>>,
        target: impl Into<String>,
        target_columns: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            columns: columns.into_iter().map(Into::into).collect(),
            target: target.into(),
            target_columns: target_columns.into_iter().map(Into::into).collect(),
            on_delete: None,
            on_update: None,
            deferrable: false,
            initially_deferred: false,
        }
    }

    /// Sets `ON DELETE`.
    ///
    /// ```
    /// # use moso_migrate::schema::{Action, ForeignKey};
    /// let fk = ForeignKey::new("fk", ["a"], "t", ["id"]).on_delete(Action::Cascade);
    /// assert_eq!(fk.delete_action(), Some(Action::Cascade));
    /// ```
    #[must_use]
    pub const fn on_delete(mut self, action: Action) -> Self {
        self.on_delete = Some(action);
        self
    }

    /// Sets `ON UPDATE`.
    ///
    /// ```
    /// # use moso_migrate::schema::{Action, ForeignKey};
    /// let fk = ForeignKey::new("fk", ["a"], "t", ["id"]).on_update(Action::Restrict);
    /// assert_eq!(fk.update_action(), Some(Action::Restrict));
    /// ```
    #[must_use]
    pub const fn on_update(mut self, action: Action) -> Self {
        self.on_update = Some(action);
        self
    }

    /// Makes it deferrable.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// let fk = ForeignKey::new("fk", ["a"], "t", ["id"]).deferrable(true);
    /// assert!(fk.is_initially_deferred());
    /// ```
    #[must_use]
    pub const fn deferrable(mut self, initially_deferred: bool) -> Self {
        self.deferrable = true;
        self.initially_deferred = initially_deferred;
        self
    }

    /// The constraint name.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("fk", ["a"], "t", ["id"]).name(), "fk");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The referencing columns.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("fk", ["a"], "t", ["id"]).columns(), ["a"]);
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// The referenced table's qualified name.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("fk", ["a"], "t", ["id"]).target_table(), "t");
    /// ```
    #[must_use]
    pub fn target_table(&self) -> &str {
        &self.target
    }

    /// The referenced columns.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("fk", ["a"], "t", ["id"]).target_columns(), ["id"]);
    /// ```
    #[must_use]
    pub fn target_columns(&self) -> &[String] {
        &self.target_columns
    }

    /// The `ON DELETE` action.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("fk", ["a"], "t", ["id"]).delete_action(), None);
    /// ```
    #[must_use]
    pub const fn delete_action(&self) -> Option<Action> {
        self.on_delete
    }

    /// The `ON UPDATE` action.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("fk", ["a"], "t", ["id"]).update_action(), None);
    /// ```
    #[must_use]
    pub const fn update_action(&self) -> Option<Action> {
        self.on_update
    }

    /// Whether the constraint is deferrable.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert!(!ForeignKey::new("fk", ["a"], "t", ["id"]).is_deferrable());
    /// ```
    #[must_use]
    pub const fn is_deferrable(&self) -> bool {
        self.deferrable
    }

    /// Whether it starts deferred.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert!(!ForeignKey::new("fk", ["a"], "t", ["id"]).is_initially_deferred());
    /// ```
    #[must_use]
    pub const fn is_initially_deferred(&self) -> bool {
        self.initially_deferred
    }

    /// Renames it, keeping everything else.
    ///
    /// ```
    /// # use moso_migrate::schema::ForeignKey;
    /// assert_eq!(ForeignKey::new("a", ["c"], "t", ["id"]).renamed_to("b").name(), "b");
    /// ```
    #[must_use]
    pub fn renamed_to(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }
}

/// A referential action.
///
/// ```
/// assert_ne!(moso_migrate::schema::Action::Cascade, moso_migrate::schema::Action::SetNull);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Action {
    /// `NO ACTION`.
    NoAction,
    /// `RESTRICT`.
    Restrict,
    /// `CASCADE`.
    Cascade,
    /// `SET NULL`.
    SetNull,
    /// `SET DEFAULT`.
    SetDefault,
}

impl Action {
    /// The SQL spelling.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Action::SetNull.as_sql(), "SET NULL");
    /// ```
    #[must_use]
    pub const fn as_sql(self) -> &'static str {
        match self {
            Self::NoAction => "NO ACTION",
            Self::Restrict => "RESTRICT",
            Self::Cascade => "CASCADE",
            Self::SetNull => "SET NULL",
            Self::SetDefault => "SET DEFAULT",
        }
    }

    /// Converts from the SQL facade's spelling.
    ///
    /// ```
    /// use moso_migrate::schema::Action;
    /// use moso_sql::ddl::ReferentialAction;
    ///
    /// assert_eq!(Action::from_sql_action(ReferentialAction::Cascade), Action::Cascade);
    /// ```
    #[must_use]
    pub const fn from_sql_action(action: moso_sql::ddl::ReferentialAction) -> Self {
        use moso_sql::ddl::ReferentialAction as R;
        match action {
            R::Restrict => Self::Restrict,
            R::Cascade => Self::Cascade,
            R::SetNull => Self::SetNull,
            R::SetDefault => Self::SetDefault,
            _ => Self::NoAction,
        }
    }

    /// Converts to the SQL facade's spelling.
    ///
    /// ```
    /// use moso_migrate::schema::Action;
    /// use moso_sql::ddl::ReferentialAction;
    ///
    /// assert_eq!(Action::Cascade.to_sql_action(), ReferentialAction::Cascade);
    /// ```
    #[must_use]
    pub const fn to_sql_action(self) -> moso_sql::ddl::ReferentialAction {
        use moso_sql::ddl::ReferentialAction as R;
        match self {
            Self::NoAction => R::NoAction,
            Self::Restrict => R::Restrict,
            Self::Cascade => R::Cascade,
            Self::SetNull => R::SetNull,
            Self::SetDefault => R::SetDefault,
        }
    }
}

/// A check constraint.
///
/// ```
/// use moso_migrate::schema::Check;
///
/// let positive = Check::new("users_age_check", "age > 0");
/// assert_eq!(positive.expression(), "age > 0");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    name: String,
    expression: String,
}

impl Check {
    /// A check constraint.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Check::new("c", "n > 0").name(), "c");
    /// ```
    #[must_use]
    pub fn new(name: impl Into<String>, expression: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            expression: expression.into(),
        }
    }

    /// The constraint name.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Check::new("c", "n > 0").name(), "c");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The predicate, as SQL text.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Check::new("c", "n > 0").expression(), "n > 0");
    /// ```
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }
}

/// A partitioning declaration.
///
/// ```
/// use moso_migrate::schema::Partition;
///
/// let by_month = Partition::range(["created_at"]);
/// assert_eq!(by_month.strategy(), "range");
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Partition {
    strategy: String,
    columns: Vec<String>,
}

impl Partition {
    /// `PARTITION BY RANGE (…)`.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Partition::range(["created_at"]).strategy(), "range");
    /// ```
    #[must_use]
    pub fn range(columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new("range", columns)
    }

    /// `PARTITION BY LIST (…)`.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Partition::list(["region"]).strategy(), "list");
    /// ```
    #[must_use]
    pub fn list(columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new("list", columns)
    }

    /// `PARTITION BY HASH (…)`.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Partition::hash(["id"]).strategy(), "hash");
    /// ```
    #[must_use]
    pub fn hash(columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::new("hash", columns)
    }

    /// Parses `range(created_at)`, the form `#[entity(partition_by = ..)]`
    /// takes.
    ///
    /// # Errors
    ///
    /// [`Error::Snapshot`](crate::Error::Snapshot) when the strategy is not
    /// `range`, `list` or `hash`.
    ///
    /// ```
    /// use moso_migrate::schema::Partition;
    ///
    /// let parsed = Partition::parse("range(created_at)")?;
    /// assert_eq!(parsed.columns(), ["created_at"]);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn parse(declaration: &str) -> Result<Self> {
        let declaration = declaration.trim();
        let (strategy, columns) =
            declaration
                .split_once('(')
                .ok_or_else(|| crate::Error::Snapshot {
                    path: "entity attribute".into(),
                    reason: format!(
                        "`{declaration}` is not a partitioning declaration; write \
                     `range(created_at)`"
                    ),
                })?;
        let strategy = strategy.trim().to_ascii_lowercase();
        if !matches!(strategy.as_str(), "range" | "list" | "hash") {
            return Err(crate::Error::Snapshot {
                path: "entity attribute".into(),
                reason: format!(
                    "`{strategy}` is not a partitioning strategy; PostgreSQL has `range`, `list` \
                     and `hash`"
                ),
            });
        }
        let columns: Vec<String> = columns
            .trim_end()
            .trim_end_matches(')')
            .split(',')
            .map(|column| column.trim().to_owned())
            .filter(|column| !column.is_empty())
            .collect();
        Ok(Self { strategy, columns })
    }

    fn new(strategy: &str, columns: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            strategy: strategy.to_owned(),
            columns: columns.into_iter().map(Into::into).collect(),
        }
    }

    /// The strategy: `range`, `list` or `hash`.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Partition::hash(["id"]).strategy(), "hash");
    /// ```
    #[must_use]
    pub fn strategy(&self) -> &str {
        &self.strategy
    }

    /// The partition key columns.
    ///
    /// ```
    /// assert_eq!(moso_migrate::schema::Partition::hash(["id"]).columns(), ["id"]);
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[String] {
        &self.columns
    }
}

/// A user-defined enum type.
///
/// ```
/// use moso_migrate::schema::EnumType;
///
/// let role = EnumType::new("user_role", ["admin", "member"]);
/// assert_eq!(role.labels(), ["admin", "member"]);
/// ```
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnumType {
    name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    schema: Option<String>,
    labels: Vec<String>,
}

impl EnumType {
    /// An enum type with its labels, in declaration order.
    ///
    /// Order matters: PostgreSQL sorts values of an enum type by it, so
    /// reordering is a schema change even though the set is the same.
    ///
    /// ```
    /// # use moso_migrate::schema::EnumType;
    /// assert_eq!(EnumType::new("t", ["a", "b"]).labels().len(), 2);
    /// ```
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        labels: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            name: name.into(),
            schema: None,
            labels: labels.into_iter().map(Into::into).collect(),
        }
    }

    /// Puts it in a named schema.
    ///
    /// ```
    /// # use moso_migrate::schema::EnumType;
    /// assert_eq!(EnumType::new("t", ["a"]).in_schema("app").qualified_name(), "app.t");
    /// ```
    #[must_use]
    pub fn in_schema(mut self, schema: impl Into<String>) -> Self {
        self.schema = Some(schema.into());
        self
    }

    /// The unqualified name.
    ///
    /// ```
    /// # use moso_migrate::schema::EnumType;
    /// assert_eq!(EnumType::new("t", ["a"]).name(), "t");
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The schema it lives in.
    ///
    /// ```
    /// # use moso_migrate::schema::EnumType;
    /// assert_eq!(EnumType::new("t", ["a"]).schema_name(), None);
    /// ```
    #[must_use]
    pub fn schema_name(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// `schema.name`, or just `name`.
    ///
    /// ```
    /// # use moso_migrate::schema::EnumType;
    /// assert_eq!(EnumType::new("t", ["a"]).qualified_name(), "t");
    /// ```
    #[must_use]
    pub fn qualified_name(&self) -> String {
        qualify(self.schema.as_deref(), &self.name)
    }

    /// The labels, in order.
    ///
    /// ```
    /// # use moso_migrate::schema::EnumType;
    /// assert_eq!(EnumType::new("t", ["a", "b"]).labels(), ["a", "b"]);
    /// ```
    #[must_use]
    pub fn labels(&self) -> &[String] {
        &self.labels
    }

    /// The labels this type has and `other` does not.
    ///
    /// ```
    /// use moso_migrate::schema::EnumType;
    ///
    /// let after = EnumType::new("t", ["a", "b", "c"]);
    /// let before = EnumType::new("t", ["a", "b"]);
    /// assert_eq!(after.labels_missing_from(&before), ["c"]);
    /// ```
    #[must_use]
    pub fn labels_missing_from(&self, other: &Self) -> Vec<String> {
        self.labels
            .iter()
            .filter(|label| !other.labels.contains(label))
            .cloned()
            .collect()
    }

    /// Whether `other`'s labels are a prefix-preserving subset, which is the
    /// case `ALTER TYPE … ADD VALUE` can handle.
    ///
    /// Removing a label, or reordering existing ones, cannot be done with
    /// `ADD VALUE` and needs the manual plan the generator emits as a template.
    ///
    /// ```
    /// use moso_migrate::schema::EnumType;
    ///
    /// let before = EnumType::new("t", ["a", "b"]);
    /// assert!(EnumType::new("t", ["a", "b", "c"]).is_additive_over(&before));
    /// assert!(!EnumType::new("t", ["b", "a"]).is_additive_over(&before));
    /// assert!(!EnumType::new("t", ["a"]).is_additive_over(&before));
    /// ```
    #[must_use]
    pub fn is_additive_over(&self, other: &Self) -> bool {
        other.labels.len() <= self.labels.len()
            && other
                .labels
                .iter()
                .zip(self.labels.iter())
                .all(|(before, after)| before == after)
    }
}

/// `schema.name`, or just `name`.
pub(crate) fn qualify(schema: Option<&str>, name: &str) -> String {
    schema.map_or_else(|| name.to_owned(), |schema| format!("{schema}.{name}"))
}

/// Splits a qualified name back into its parts.
pub(crate) fn unqualify(qualified: &str) -> (Option<&str>, &str) {
    match qualified.split_once('.') {
        Some((schema, name)) => (Some(schema), name),
        None => (None, qualified),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> Table {
        let mut table = Table::new("users").for_entity("User");
        table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        table.add_column(Column::new("email", DataType::Text).for_field("email"));
        table.add_column(
            Column::new("bio", DataType::Text)
                .nullable()
                .for_field("bio"),
        );
        table.set_primary_key(["id"]);
        table.add_index(Index::new("users_email_key", ["email"]).unique());
        table
    }

    #[test]
    fn tables_and_columns_round_trip_through_json() {
        let mut schema = Schema::empty();
        schema.add_table(users());
        schema.add_enum(EnumType::new("user_role", ["admin", "member"]));
        schema.add_extension("pg_trgm");

        let json = schema.to_json();
        let back = Schema::from_json(&json).expect("round trip");
        assert_eq!(back, schema);
    }

    #[test]
    fn json_is_stable_across_insertion_order() {
        let mut first = Schema::empty();
        first.add_table(Table::new("b"));
        first.add_table(Table::new("a"));

        let mut second = Schema::empty();
        second.add_table(Table::new("a"));
        second.add_table(Table::new("b"));

        assert_eq!(first.to_json(), second.to_json());
        assert_eq!(first.checksum(), second.checksum());
    }

    #[test]
    fn creation_order_puts_targets_first() {
        let mut posts = Table::new("posts");
        posts.add_foreign_key(ForeignKey::new("fk", ["author_id"], "users", ["id"]));
        let mut comments = Table::new("comments");
        comments.add_foreign_key(ForeignKey::new("fk_post", ["post_id"], "posts", ["id"]));

        let mut schema = Schema::empty();
        schema.add_table(comments);
        schema.add_table(posts);
        schema.add_table(users());

        let order: Vec<&str> = schema
            .creation_order()
            .iter()
            .map(|table| table.name())
            .collect();
        assert_eq!(order, ["users", "posts", "comments"]);
    }

    #[test]
    fn creation_order_survives_a_cycle() {
        let mut a = Table::new("a");
        a.add_foreign_key(ForeignKey::new("a_b", ["b_id"], "b", ["id"]));
        let mut b = Table::new("b");
        b.add_foreign_key(ForeignKey::new("b_a", ["a_id"], "a", ["id"]));

        let mut schema = Schema::empty();
        schema.add_table(a);
        schema.add_table(b);

        let order = schema.creation_order();
        assert_eq!(order.len(), 2, "both tables are still emitted");
    }

    #[test]
    fn a_self_reference_is_not_a_cycle() {
        let mut categories = Table::new("categories");
        categories.add_foreign_key(ForeignKey::new(
            "parent",
            ["parent_id"],
            "categories",
            ["id"],
        ));
        let mut schema = Schema::empty();
        schema.add_table(categories);
        assert_eq!(schema.creation_order().len(), 1);
    }

    #[test]
    fn columns_keep_their_position_when_replaced() {
        let mut table = users();
        table.add_column(Column::new("email", DataType::VarChar(Some(320))));
        assert_eq!(table.columns()[1].name(), "email");
        assert_eq!(table.columns()[1].type_name(), "varchar(320)");
        assert_eq!(table.columns().len(), 3);
    }

    #[test]
    fn a_fill_value_is_needed_only_for_a_bare_not_null() {
        assert!(Column::new("locale", DataType::Text).needs_a_fill_value());
        assert!(
            !Column::new("locale", DataType::Text)
                .nullable()
                .needs_a_fill_value()
        );
        assert!(
            !Column::new("locale", DataType::Text)
                .with_default("'en'")
                .needs_a_fill_value()
        );
        assert!(!Column::new("id", DataType::BigSerial).needs_a_fill_value());
        assert!(
            !Column::new("id", DataType::BigInt)
                .identity(IdentityKind::Always)
                .needs_a_fill_value()
        );
    }

    #[test]
    fn enum_additivity_is_order_sensitive() {
        let before = EnumType::new("t", ["a", "b"]);
        assert!(EnumType::new("t", ["a", "b", "c"]).is_additive_over(&before));
        assert!(!EnumType::new("t", ["a", "c", "b"]).is_additive_over(&before));
        assert!(!EnumType::new("t", ["a"]).is_additive_over(&before));
        assert_eq!(
            EnumType::new("t", ["a", "b", "c"]).labels_missing_from(&before),
            ["c"]
        );
    }

    #[test]
    fn partition_declarations_parse() {
        let parsed = Partition::parse("range(created_at)").expect("valid");
        assert_eq!(parsed.strategy(), "range");
        assert_eq!(parsed.columns(), ["created_at"]);

        let two = Partition::parse("LIST( region , tier )").expect("valid");
        assert_eq!(two.columns(), ["region", "tier"]);

        assert!(Partition::parse("created_at").is_err());
        assert!(Partition::parse("cluster(created_at)").is_err());
    }

    #[test]
    fn qualified_names_round_trip() {
        assert_eq!(qualify(Some("app"), "users"), "app.users");
        assert_eq!(qualify(None, "users"), "users");
        assert_eq!(unqualify("app.users"), (Some("app"), "users"));
        assert_eq!(unqualify("users"), (None, "users"));
    }

    #[test]
    fn adding_a_qualified_table_declares_its_schema() {
        let mut schema = Schema::empty();
        schema.add_table(Table::new("events").in_schema("analytics"));
        assert_eq!(schema.schemas().collect::<Vec<_>>(), ["analytics"]);
        assert!(schema.table("analytics.events").is_some());
    }

    #[test]
    fn matching_ignoring_name_sees_through_a_rename_only() {
        let before = Column::new("name", DataType::Text);
        assert!(before.matches_ignoring_name(&Column::new("full_name", DataType::Text)));
        assert!(
            !before.matches_ignoring_name(&Column::new("full_name", DataType::Text).nullable())
        );
        assert!(!before.matches_ignoring_name(&Column::new("full_name", DataType::Integer)));
    }

    #[test]
    fn referential_actions_round_trip_through_the_facade() {
        for action in [
            Action::NoAction,
            Action::Restrict,
            Action::Cascade,
            Action::SetNull,
            Action::SetDefault,
        ] {
            assert_eq!(Action::from_sql_action(action.to_sql_action()), action);
        }
    }
}
