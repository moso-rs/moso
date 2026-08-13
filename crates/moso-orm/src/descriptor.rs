//! The reflective description of an entity: everything the migration generator
//! has to diff, and everything the admin has to render.
//!
//! # Why one description and not two
//!
//! `moso-migrate` needs types, nullability, defaults, uniqueness, indexes,
//! checks and foreign keys. `moso-admin` needs labels, relations, enum
//! variants, which column is the soft-delete flag and which is the tenant.
//! Those overlap by about eighty percent, and two descriptions that overlap by
//! eighty percent drift.
//!
//! So there is one [`EntityDescriptor`], built once per entity behind a
//! `OnceLock` by the code `#[derive(Entity)]` writes, and reachable from
//! [`Entity::descriptor`](crate::Entity::descriptor).
//!
//! ```
//! use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
//! use moso_sql::{DataType, Ident, TableRef};
//!
//! let users = EntityDescriptor::builder("User", TableRef::from_static("users"))
//!     .column(
//!         ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid)
//!             .field("id")
//!             .primary_key()
//!             .build(),
//!     )
//!     .column(
//!         ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
//!             .field("email")
//!             .unique()
//!             .build(),
//!     )
//!     .build();
//!
//! assert_eq!(users.table().name().as_str(), "users");
//! assert_eq!(users.primary_key().len(), 1);
//! assert!(users.column("email").is_some_and(ColumnDescriptor::is_unique));
//! ```

use moso_sql::ddl::{IndexMethod, ReferentialAction};
use moso_sql::{DataType, Expr, Ident, Nulls, Order, TableRef, TypeRef, Value};

use crate::sqltype::EnumStorage;

/// Everything that is true of an entity, as data.
///
/// Built by the derive and read by `moso-migrate`, `moso-admin`, `moso doctor`
/// and the query builder's own diagnostics — which is why the columns keep both
/// the SQL name and the Rust field name.
///
/// ```
/// use moso_orm::descriptor::EntityDescriptor;
/// use moso_sql::TableRef;
///
/// let posts = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
///     .soft_delete("deleted_at")
///     .timestamps("created_at", "updated_at")
///     .build();
///
/// assert_eq!(posts.soft_delete().map(|c| c.as_str()), Some("deleted_at"));
/// assert!(posts.is_soft_deletable());
/// ```
#[derive(Clone, Debug)]
pub struct EntityDescriptor {
    entity: &'static str,
    table: TableRef,
    columns: Vec<ColumnDescriptor>,
    indexes: Vec<IndexDescriptor>,
    checks: Vec<CheckDescriptor>,
    foreign_keys: Vec<ForeignKeyDescriptor>,
    relations: Vec<RelationDescriptor>,
    enum_types: Vec<EnumTypeDescriptor>,
    soft_delete: Option<Ident>,
    created_at: Option<Ident>,
    updated_at: Option<Ident>,
    tenant: Option<Ident>,
    version: Option<Ident>,
    audited: bool,
    exposed: bool,
    row_level_security: bool,
    comment: Option<String>,
}

impl EntityDescriptor {
    /// Starts building a description of `entity`, stored in `table`.
    ///
    /// ```
    /// use moso_orm::descriptor::EntityDescriptor;
    /// use moso_sql::TableRef;
    ///
    /// let d = EntityDescriptor::builder("Tag", TableRef::from_static("tags")).build();
    /// assert_eq!(d.entity(), "Tag");
    /// ```
    #[must_use]
    pub fn builder(entity: &'static str, table: TableRef) -> EntityDescriptorBuilder {
        EntityDescriptorBuilder {
            descriptor: Self {
                entity,
                table,
                columns: Vec::new(),
                indexes: Vec::new(),
                checks: Vec::new(),
                foreign_keys: Vec::new(),
                relations: Vec::new(),
                enum_types: Vec::new(),
                soft_delete: None,
                created_at: None,
                updated_at: None,
                tenant: None,
                version: None,
                audited: false,
                exposed: false,
                row_level_security: false,
                comment: None,
            },
        }
    }

    /// The Rust type's name, as the user wrote it.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users")).build();
    /// assert_eq!(d.entity(), "User");
    /// ```
    #[must_use]
    pub const fn entity(&self) -> &'static str {
        self.entity
    }

    /// The table, with its schema when one was declared.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users")).build();
    /// assert_eq!(d.table().name().as_str(), "users");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// Every column, in declaration order — which is the order
    /// [`Entity::from_row`](crate::Entity::from_row) reads them in.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users")).build();
    /// assert!(d.columns().is_empty());
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[ColumnDescriptor] {
        &self.columns
    }

    /// The column with this SQL name.
    ///
    /// ```
    /// use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
    /// use moso_sql::{DataType, Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .column(ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build())
    ///     .build();
    /// assert!(d.column("id").is_some());
    /// assert!(d.column("nope").is_none());
    /// ```
    #[must_use]
    pub fn column(&self, name: &str) -> Option<&ColumnDescriptor> {
        self.columns
            .iter()
            .find(|column| column.name().as_str() == name)
    }

    /// The column backing this Rust field.
    ///
    /// ```
    /// use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
    /// use moso_sql::{DataType, Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .column(
    ///         ColumnDescriptor::builder(Ident::from_static("password_hash"), DataType::Text)
    ///             .field("password")
    ///             .build(),
    ///     )
    ///     .build();
    /// assert_eq!(d.column_for_field("password").map(|c| c.name().as_str()), Some("password_hash"));
    /// ```
    #[must_use]
    pub fn column_for_field(&self, field: &str) -> Option<&ColumnDescriptor> {
        self.columns
            .iter()
            .find(|column| column.field() == Some(field))
    }

    /// The primary-key columns, in key order. Composite keys have more than one.
    ///
    /// ```
    /// use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
    /// use moso_sql::{DataType, Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("Membership", TableRef::from_static("memberships"))
    ///     .column(ColumnDescriptor::builder(Ident::from_static("user_id"), DataType::Uuid).primary_key().build())
    ///     .column(ColumnDescriptor::builder(Ident::from_static("team_id"), DataType::Uuid).primary_key().build())
    ///     .build();
    /// assert_eq!(d.primary_key().len(), 2);
    /// ```
    #[must_use]
    pub fn primary_key(&self) -> Vec<&ColumnDescriptor> {
        self.columns
            .iter()
            .filter(|column| column.is_primary_key())
            .collect()
    }

    /// Every declared index, including the ones implied by `#[entity(index)]`.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users")).build();
    /// assert!(d.indexes().is_empty());
    /// ```
    #[must_use]
    pub fn indexes(&self) -> &[IndexDescriptor] {
        &self.indexes
    }

    /// Every table-level `CHECK`.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Order", TableRef::from_static("orders")).build();
    /// assert!(d.checks().is_empty());
    /// ```
    #[must_use]
    pub fn checks(&self) -> &[CheckDescriptor] {
        &self.checks
    }

    /// Every foreign key this table owns.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Post", TableRef::from_static("posts")).build();
    /// assert!(d.foreign_keys().is_empty());
    /// ```
    #[must_use]
    pub fn foreign_keys(&self) -> &[ForeignKeyDescriptor] {
        &self.foreign_keys
    }

    /// Every declared relation, whether or not it owns a foreign key.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Post", TableRef::from_static("posts")).build();
    /// assert!(d.relations().is_empty());
    /// ```
    #[must_use]
    pub fn relations(&self) -> &[RelationDescriptor] {
        &self.relations
    }

    /// The relation with this name, as the field is spelled.
    ///
    /// ```
    /// use moso_orm::descriptor::{EntityDescriptor, RelationDescriptor, RelationKind};
    /// use moso_sql::TableRef;
    ///
    /// let d = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
    ///     .relation(
    ///         RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
    ///             .build(),
    ///     )
    ///     .build();
    /// assert!(d.relation("author").is_some());
    /// ```
    #[must_use]
    pub fn relation(&self, name: &str) -> Option<&RelationDescriptor> {
        self.relations
            .iter()
            .find(|relation| relation.name() == name)
    }

    /// Every PostgreSQL enum type this entity's columns need.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Order", TableRef::from_static("orders")).build();
    /// assert!(d.enum_types().is_empty());
    /// ```
    #[must_use]
    pub fn enum_types(&self) -> &[EnumTypeDescriptor] {
        &self.enum_types
    }

    /// The soft-delete column, when the entity has one.
    ///
    /// Every query on a soft-deletable entity adds `WHERE <column> IS NULL`
    /// unless `.with_deleted()` opts out.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .soft_delete("deleted_at")
    ///     .build();
    /// assert_eq!(d.soft_delete().map(|c| c.as_str()), Some("deleted_at"));
    /// ```
    #[must_use]
    pub const fn soft_delete(&self) -> Option<&Ident> {
        self.soft_delete.as_ref()
    }

    /// Whether `delete()` on this entity writes a timestamp instead of removing
    /// the row.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users")).build();
    /// assert!(!d.is_soft_deletable());
    /// ```
    #[must_use]
    pub const fn is_soft_deletable(&self) -> bool {
        self.soft_delete.is_some()
    }

    /// The `created_at` column, when the entity has one.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .timestamps("created_at", "updated_at")
    ///     .build();
    /// assert_eq!(d.created_at().map(|c| c.as_str()), Some("created_at"));
    /// ```
    #[must_use]
    pub const fn created_at(&self) -> Option<&Ident> {
        self.created_at.as_ref()
    }

    /// The `updated_at` column, when the entity has one.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .timestamps("created_at", "updated_at")
    ///     .build();
    /// assert_eq!(d.updated_at().map(|c| c.as_str()), Some("updated_at"));
    /// ```
    #[must_use]
    pub const fn updated_at(&self) -> Option<&Ident> {
        self.updated_at.as_ref()
    }

    /// The tenant discriminator column, when the entity is tenant-scoped.
    ///
    /// Its presence is what makes an unscoped query a compile error — see
    /// [`crate::NeedsTenant`].
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Invoice", TableRef::from_static("invoices"))
    ///     .tenant("tenant_id")
    ///     .build();
    /// assert!(d.is_tenant_scoped());
    /// ```
    #[must_use]
    pub const fn tenant(&self) -> Option<&Ident> {
        self.tenant.as_ref()
    }

    /// Whether every query for this entity must name a tenant.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Country", TableRef::from_static("countries")).build();
    /// assert!(!d.is_tenant_scoped());
    /// ```
    #[must_use]
    pub const fn is_tenant_scoped(&self) -> bool {
        self.tenant.is_some()
    }

    /// The optimistic-locking column, when the entity has one.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Order", TableRef::from_static("orders"))
    ///     .versioned("version")
    ///     .build();
    /// assert_eq!(d.version().map(|c| c.as_str()), Some("version"));
    /// ```
    #[must_use]
    pub const fn version(&self) -> Option<&Ident> {
        self.version.as_ref()
    }

    /// Whether changes are recorded into the audit table.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users")).build();
    /// assert!(!d.is_audited());
    /// ```
    #[must_use]
    pub const fn is_audited(&self) -> bool {
        self.audited
    }

    /// Whether the entity opted out of the "entities are not schemas" rule
    /// (ADR-0008) with `#[entity(expose)]`.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Country", TableRef::from_static("countries"))
    ///     .exposed()
    ///     .build();
    /// assert!(d.is_exposed());
    /// ```
    #[must_use]
    pub const fn is_exposed(&self) -> bool {
        self.exposed
    }

    /// Whether the migration generator emits a row-level-security policy.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Invoice", TableRef::from_static("invoices")).build();
    /// assert!(!d.has_row_level_security());
    /// ```
    #[must_use]
    pub const fn has_row_level_security(&self) -> bool {
        self.row_level_security
    }

    /// The table comment, which the admin uses as the section's description.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .comment("People who can sign in")
    ///     .build();
    /// assert_eq!(d.comment(), Some("People who can sign in"));
    /// ```
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// The columns an `INSERT` writes: everything except read-only, generated
    /// and relation fields.
    ///
    /// ```
    /// use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
    /// use moso_sql::{DataType, Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .column(ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build())
    ///     .column(
    ///         ColumnDescriptor::builder(Ident::from_static("search"), DataType::TsVector)
    ///             .readonly()
    ///             .build(),
    ///     )
    ///     .build();
    /// assert_eq!(d.insertable().len(), 1);
    /// ```
    #[must_use]
    pub fn insertable(&self) -> Vec<&ColumnDescriptor> {
        self.columns
            .iter()
            .filter(|column| column.is_writable())
            .collect()
    }
}

/// Assembles an [`EntityDescriptor`].
///
/// Every method takes and returns `self`, so the derive emits one expression.
///
/// ```
/// use moso_orm::descriptor::EntityDescriptor;
/// use moso_sql::TableRef;
///
/// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
///     .comment("People")
///     .audited()
///     .build();
/// assert!(d.is_audited());
/// ```
#[derive(Clone, Debug)]
pub struct EntityDescriptorBuilder {
    descriptor: EntityDescriptor,
}

impl EntityDescriptorBuilder {
    /// Appends a column.
    ///
    /// ```
    /// use moso_orm::descriptor::{ColumnDescriptor, EntityDescriptor};
    /// use moso_sql::{DataType, Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("Tag", TableRef::from_static("tags"))
    ///     .column(ColumnDescriptor::builder(Ident::from_static("id"), DataType::BigInt).build())
    ///     .build();
    /// assert_eq!(d.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn column(mut self, column: ColumnDescriptor) -> Self {
        self.descriptor.columns.push(column);
        self
    }

    /// Appends an index.
    ///
    /// ```
    /// use moso_orm::descriptor::{EntityDescriptor, IndexDescriptor};
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .index(IndexDescriptor::builder("users_email_idx").column(Ident::from_static("email")).build())
    ///     .build();
    /// assert_eq!(d.indexes().len(), 1);
    /// ```
    #[must_use]
    pub fn index(mut self, index: IndexDescriptor) -> Self {
        self.descriptor.indexes.push(index);
        self
    }

    /// Appends a table-level check.
    ///
    /// ```
    /// use moso_orm::descriptor::{CheckDescriptor, EntityDescriptor};
    /// use moso_sql::TableRef;
    ///
    /// let d = EntityDescriptor::builder("Order", TableRef::from_static("orders"))
    ///     .check(CheckDescriptor::new("orders_total_positive", "total >= 0"))
    ///     .build();
    /// assert_eq!(d.checks().len(), 1);
    /// ```
    #[must_use]
    pub fn check(mut self, check: CheckDescriptor) -> Self {
        self.descriptor.checks.push(check);
        self
    }

    /// Appends a foreign key.
    ///
    /// ```
    /// use moso_orm::descriptor::{EntityDescriptor, ForeignKeyDescriptor};
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let d = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
    ///     .foreign_key(
    ///         ForeignKeyDescriptor::builder("posts_author_id_fkey", TableRef::from_static("users"))
    ///             .column(Ident::from_static("author_id"), Ident::from_static("id"))
    ///             .build(),
    ///     )
    ///     .build();
    /// assert_eq!(d.foreign_keys().len(), 1);
    /// ```
    #[must_use]
    pub fn foreign_key(mut self, foreign_key: ForeignKeyDescriptor) -> Self {
        self.descriptor.foreign_keys.push(foreign_key);
        self
    }

    /// Appends a relation.
    ///
    /// ```
    /// use moso_orm::descriptor::{EntityDescriptor, RelationDescriptor, RelationKind};
    /// use moso_sql::TableRef;
    ///
    /// let d = EntityDescriptor::builder("Post", TableRef::from_static("posts"))
    ///     .relation(RelationDescriptor::builder("comments", RelationKind::HasMany, "Comment").build())
    ///     .build();
    /// assert_eq!(d.relations().len(), 1);
    /// ```
    #[must_use]
    pub fn relation(mut self, relation: RelationDescriptor) -> Self {
        self.descriptor.relations.push(relation);
        self
    }

    /// Appends an enum type this entity's columns need.
    ///
    /// ```
    /// use moso_orm::EnumStorage;
    /// use moso_orm::descriptor::{EntityDescriptor, EnumTypeDescriptor};
    /// use moso_sql::TableRef;
    ///
    /// let d = EntityDescriptor::builder("Order", TableRef::from_static("orders"))
    ///     .enum_type(EnumTypeDescriptor::new("order_status", EnumStorage::PgEnum, ["pending", "paid"]))
    ///     .build();
    /// assert_eq!(d.enum_types().len(), 1);
    /// ```
    #[must_use]
    pub fn enum_type(mut self, enum_type: EnumTypeDescriptor) -> Self {
        self.descriptor.enum_types.push(enum_type);
        self
    }

    /// Declares the soft-delete column.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .soft_delete("deleted_at")
    ///     .build();
    /// assert!(d.is_soft_deletable());
    /// ```
    #[must_use]
    pub fn soft_delete(mut self, column: &'static str) -> Self {
        self.descriptor.soft_delete = Some(Ident::from_static(column));
        self
    }

    /// Declares the two managed timestamp columns.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("User", TableRef::from_static("users"))
    ///     .timestamps("created_at", "updated_at")
    ///     .build();
    /// assert!(d.created_at().is_some() && d.updated_at().is_some());
    /// ```
    #[must_use]
    pub fn timestamps(mut self, created_at: &'static str, updated_at: &'static str) -> Self {
        self.descriptor.created_at = Some(Ident::from_static(created_at));
        self.descriptor.updated_at = Some(Ident::from_static(updated_at));
        self
    }

    /// Declares the tenant discriminator column.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Invoice", TableRef::from_static("invoices"))
    ///     .tenant("tenant_id")
    ///     .build();
    /// assert!(d.is_tenant_scoped());
    /// ```
    #[must_use]
    pub fn tenant(mut self, column: &'static str) -> Self {
        self.descriptor.tenant = Some(Ident::from_static(column));
        self
    }

    /// Declares the optimistic-locking column.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("Order", TableRef::from_static("orders"))
    ///     .versioned("version")
    ///     .build();
    /// assert!(d.version().is_some());
    /// ```
    #[must_use]
    pub fn versioned(mut self, column: &'static str) -> Self {
        self.descriptor.version = Some(Ident::from_static(column));
        self
    }

    /// Marks the entity as audited.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// assert!(
    ///     EntityDescriptor::builder("U", TableRef::from_static("u")).audited().build().is_audited()
    /// );
    /// ```
    #[must_use]
    pub const fn audited(mut self) -> Self {
        self.descriptor.audited = true;
        self
    }

    /// Marks the entity as deliberately exposable (ADR-0008's opt-out).
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// assert!(
    ///     EntityDescriptor::builder("C", TableRef::from_static("c")).exposed().build().is_exposed()
    /// );
    /// ```
    #[must_use]
    pub const fn exposed(mut self) -> Self {
        self.descriptor.exposed = true;
        self
    }

    /// Asks the migration generator for a row-level-security policy.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("I", TableRef::from_static("i")).row_level_security().build();
    /// assert!(d.has_row_level_security());
    /// ```
    #[must_use]
    pub const fn row_level_security(mut self) -> Self {
        self.descriptor.row_level_security = true;
        self
    }

    /// Sets the table comment.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("U", TableRef::from_static("u")).comment("People").build();
    /// assert_eq!(d.comment(), Some("People"));
    /// ```
    #[must_use]
    pub fn comment(mut self, text: impl Into<String>) -> Self {
        self.descriptor.comment = Some(text.into());
        self
    }

    /// Finishes the description.
    ///
    /// ```
    /// # use moso_orm::descriptor::EntityDescriptor;
    /// # use moso_sql::TableRef;
    /// let d = EntityDescriptor::builder("U", TableRef::from_static("users")).build();
    /// assert_eq!(d.entity(), "U");
    /// ```
    #[must_use]
    pub fn build(self) -> EntityDescriptor {
        self.descriptor
    }
}

/// One column, described.
///
/// ```
/// use moso_orm::descriptor::ColumnDescriptor;
/// use moso_sql::{DataType, Ident};
///
/// let email = ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
///     .field("email")
///     .unique()
///     .comment("Login identity")
///     .build();
///
/// assert!(email.is_unique());
/// assert!(!email.is_nullable());
/// assert!(email.is_writable());
/// ```
#[derive(Clone, Debug)]
pub struct ColumnDescriptor {
    name: Ident,
    field: Option<&'static str>,
    data_type: DataType,
    nullable: bool,
    primary_key: bool,
    unique: bool,
    readonly: bool,
    encrypted: bool,
    role: ColumnRole,
    default: Option<ColumnDefault>,
    generated: Option<String>,
    max_length: Option<u32>,
    numeric: Option<(u8, u8)>,
    enum_type: Option<TypeRef>,
    comment: Option<String>,
}

impl ColumnDescriptor {
    /// Starts describing a column.
    ///
    /// ```
    /// use moso_orm::descriptor::ColumnDescriptor;
    /// use moso_sql::{DataType, Ident};
    ///
    /// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build();
    /// assert_eq!(c.name().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn builder(name: Ident, data_type: DataType) -> ColumnDescriptorBuilder {
        ColumnDescriptorBuilder {
            column: Self {
                name,
                field: None,
                data_type,
                nullable: false,
                primary_key: false,
                unique: false,
                readonly: false,
                encrypted: false,
                role: ColumnRole::Data,
                default: None,
                generated: None,
                max_length: None,
                numeric: None,
                enum_type: None,
                comment: None,
            },
        }
    }

    /// The SQL column name.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build();
    /// assert_eq!(c.name().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The Rust field name, when it differs from the column name or when the
    /// admin needs a label.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("password_hash"), DataType::Text)
    ///     .field("password")
    ///     .build();
    /// assert_eq!(c.field(), Some("password"));
    /// ```
    #[must_use]
    pub const fn field(&self) -> Option<&'static str> {
        self.field
    }

    /// The column's SQL type.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build();
    /// assert_eq!(c.data_type(), &DataType::Uuid);
    /// ```
    #[must_use]
    pub const fn data_type(&self) -> &DataType {
        &self.data_type
    }

    /// Whether the column accepts `NULL`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("bio"), DataType::Text).nullable().build();
    /// assert!(c.is_nullable());
    /// ```
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// Whether the column is part of the primary key.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).primary_key().build();
    /// assert!(c.is_primary_key());
    /// ```
    #[must_use]
    pub const fn is_primary_key(&self) -> bool {
        self.primary_key
    }

    /// Whether the column carries a single-column `UNIQUE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text).unique().build();
    /// assert!(c.is_unique());
    /// ```
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// Whether the column is never written by an `INSERT` or an `UPDATE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("tsv"), DataType::TsVector).readonly().build();
    /// assert!(c.is_readonly());
    /// ```
    #[must_use]
    pub const fn is_readonly(&self) -> bool {
        self.readonly
    }

    /// Whether the value is encrypted at rest with the application key.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("ssn"), DataType::Bytea).encrypted().build();
    /// assert!(c.is_encrypted());
    /// ```
    #[must_use]
    pub const fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// Whether an `INSERT` or an `UPDATE` may write this column.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build();
    /// assert!(c.is_writable());
    /// ```
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        !self.readonly && self.generated.is_none()
    }

    /// What the column is *for*, beyond holding data.
    ///
    /// ```
    /// use moso_orm::descriptor::{ColumnDescriptor, ColumnRole};
    /// use moso_sql::{DataType, Ident};
    ///
    /// let c = ColumnDescriptor::builder(Ident::from_static("created_at"), DataType::Date)
    ///     .role(ColumnRole::CreatedAt)
    ///     .build();
    /// assert_eq!(c.role(), ColumnRole::CreatedAt);
    /// ```
    #[must_use]
    pub const fn role(&self) -> ColumnRole {
        self.role
    }

    /// The database default, when there is one.
    ///
    /// ```
    /// # use moso_orm::descriptor::{ColumnDefault, ColumnDescriptor};
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("is_admin"), DataType::Boolean)
    ///     .default(ColumnDefault::sql("false"))
    ///     .build();
    /// assert!(c.default().is_some());
    /// ```
    #[must_use]
    pub const fn default(&self) -> Option<&ColumnDefault> {
        self.default.as_ref()
    }

    /// The generation expression, when the column is `GENERATED ALWAYS AS`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("full"), DataType::Text)
    ///     .generated("first || ' ' || last")
    ///     .build();
    /// assert!(c.generated().is_some());
    /// ```
    #[must_use]
    pub fn generated(&self) -> Option<&str> {
        self.generated.as_deref()
    }

    /// The declared `varchar` length, when `#[entity(len = ..)]` set one.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("name"), DataType::Text)
    ///     .max_length(255)
    ///     .build();
    /// assert_eq!(c.max_length(), Some(255));
    /// ```
    #[must_use]
    pub const fn max_length(&self) -> Option<u32> {
        self.max_length
    }

    /// The declared `numeric(p, s)`, when `#[entity(precision(..))]` set one.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("total"), DataType::Text)
    ///     .numeric(10, 2)
    ///     .build();
    /// assert_eq!(c.numeric(), Some((10, 2)));
    /// ```
    #[must_use]
    pub const fn numeric(&self) -> Option<(u8, u8)> {
        self.numeric
    }

    /// The PostgreSQL enum type this column stores, when it stores one.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident, TypeRef};
    /// let c = ColumnDescriptor::builder(Ident::from_static("status"), DataType::Text)
    ///     .enum_type(TypeRef::from_static("order_status"))
    ///     .build();
    /// assert!(c.enum_type().is_some());
    /// ```
    #[must_use]
    pub const fn enum_type(&self) -> Option<&TypeRef> {
        self.enum_type.as_ref()
    }

    /// The column comment, which the admin renders as help text.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
    ///     .comment("Login identity")
    ///     .build();
    /// assert_eq!(c.comment(), Some("Login identity"));
    /// ```
    #[must_use]
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }
}

/// Assembles a [`ColumnDescriptor`].
///
/// ```
/// use moso_orm::descriptor::ColumnDescriptor;
/// use moso_sql::{DataType, Ident};
///
/// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid)
///     .primary_key()
///     .build();
/// assert!(c.is_primary_key());
/// ```
#[derive(Clone, Debug)]
pub struct ColumnDescriptorBuilder {
    column: ColumnDescriptor,
}

impl ColumnDescriptorBuilder {
    /// Records the Rust field name.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).field("f").build();
    /// assert_eq!(c.field(), Some("f"));
    /// ```
    #[must_use]
    pub const fn field(mut self, field: &'static str) -> Self {
        self.column.field = Some(field);
        self
    }

    /// Marks the column nullable.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// assert!(
    ///     ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).nullable().build().is_nullable()
    /// );
    /// ```
    #[must_use]
    pub const fn nullable(mut self) -> Self {
        self.column.nullable = true;
        self
    }

    /// Marks the column part of the primary key.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Uuid).primary_key().build();
    /// assert!(c.is_primary_key());
    /// ```
    #[must_use]
    pub const fn primary_key(mut self) -> Self {
        self.column.primary_key = true;
        self
    }

    /// Adds a single-column `UNIQUE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).unique().build();
    /// assert!(c.is_unique());
    /// ```
    #[must_use]
    pub const fn unique(mut self) -> Self {
        self.column.unique = true;
        self
    }

    /// Excludes the column from every `INSERT` and `UPDATE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).readonly().build();
    /// assert!(!c.is_writable());
    /// ```
    #[must_use]
    pub const fn readonly(mut self) -> Self {
        self.column.readonly = true;
        self
    }

    /// Marks the column as encrypted at rest.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Bytea).encrypted().build();
    /// assert!(c.is_encrypted());
    /// ```
    #[must_use]
    pub const fn encrypted(mut self) -> Self {
        self.column.encrypted = true;
        self
    }

    /// Sets what the column is for.
    ///
    /// ```
    /// # use moso_orm::descriptor::{ColumnDescriptor, ColumnRole};
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("tenant_id"), DataType::Uuid)
    ///     .role(ColumnRole::Tenant)
    ///     .build();
    /// assert_eq!(c.role(), ColumnRole::Tenant);
    /// ```
    #[must_use]
    pub const fn role(mut self, role: ColumnRole) -> Self {
        self.column.role = role;
        self
    }

    /// Sets the database default.
    ///
    /// ```
    /// # use moso_orm::descriptor::{ColumnDefault, ColumnDescriptor};
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Boolean)
    ///     .default(ColumnDefault::sql("false"))
    ///     .build();
    /// assert!(c.default().is_some());
    /// ```
    #[must_use]
    pub fn default(mut self, default: ColumnDefault) -> Self {
        self.column.default = Some(default);
        self
    }

    /// Sets the `GENERATED ALWAYS AS` expression.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text)
    ///     .generated("lower(email)")
    ///     .build();
    /// assert!(!c.is_writable());
    /// ```
    #[must_use]
    pub fn generated(mut self, expression: impl Into<String>) -> Self {
        self.column.generated = Some(expression.into());
        self
    }

    /// Sets the `varchar` length.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).max_length(64).build();
    /// assert_eq!(c.max_length(), Some(64));
    /// ```
    #[must_use]
    pub const fn max_length(mut self, length: u32) -> Self {
        self.column.max_length = Some(length);
        self
    }

    /// Sets `numeric(precision, scale)`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).numeric(10, 2).build();
    /// assert_eq!(c.numeric(), Some((10, 2)));
    /// ```
    #[must_use]
    pub const fn numeric(mut self, precision: u8, scale: u8) -> Self {
        self.column.numeric = Some((precision, scale));
        self
    }

    /// Names the PostgreSQL enum type the column stores.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident, TypeRef};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text)
    ///     .enum_type(TypeRef::from_static("mood"))
    ///     .build();
    /// assert!(c.enum_type().is_some());
    /// ```
    #[must_use]
    pub fn enum_type(mut self, type_name: TypeRef) -> Self {
        self.column.enum_type = Some(type_name);
        self
    }

    /// Sets the column comment.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("c"), DataType::Text).comment("x").build();
    /// assert_eq!(c.comment(), Some("x"));
    /// ```
    #[must_use]
    pub fn comment(mut self, text: impl Into<String>) -> Self {
        self.column.comment = Some(text.into());
        self
    }

    /// Finishes the column.
    ///
    /// ```
    /// # use moso_orm::descriptor::ColumnDescriptor;
    /// # use moso_sql::{DataType, Ident};
    /// let c = ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid).build();
    /// assert_eq!(c.name().as_str(), "id");
    /// ```
    #[must_use]
    pub fn build(self) -> ColumnDescriptor {
        self.column
    }
}

/// What a column is for, beyond holding a value.
///
/// The query builder reads this to know which column to write on a soft delete,
/// which to bump on an update, and which one a tenant scope compares against.
///
/// ```
/// use moso_orm::descriptor::ColumnRole;
///
/// assert!(ColumnRole::UpdatedAt.is_framework_managed());
/// assert!(!ColumnRole::Data.is_framework_managed());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ColumnRole {
    /// An ordinary column the application writes.
    #[default]
    Data,
    /// Set on insert and never again.
    CreatedAt,
    /// Set on insert and on every update.
    UpdatedAt,
    /// `NULL` while the row is live; a timestamp once it is soft-deleted.
    SoftDelete,
    /// The tenant discriminator.
    Tenant,
    /// The optimistic-locking counter.
    Version,
    /// A foreign key generated by a `belongs_to` relation.
    ForeignKey,
}

impl ColumnRole {
    /// Whether the framework writes this column rather than the application.
    ///
    /// ```
    /// use moso_orm::descriptor::ColumnRole;
    ///
    /// assert!(ColumnRole::Version.is_framework_managed());
    /// assert!(!ColumnRole::ForeignKey.is_framework_managed());
    /// ```
    #[must_use]
    pub const fn is_framework_managed(self) -> bool {
        matches!(
            self,
            Self::CreatedAt | Self::UpdatedAt | Self::SoftDelete | Self::Version
        )
    }
}

/// A column's database default.
///
/// Kept as text rather than as an [`Expr`] because a default is emitted into a
/// migration file a human reads, and `now()` should stay `now()`.
///
/// ```
/// use moso_orm::descriptor::ColumnDefault;
/// use moso_sql::Value;
///
/// assert_eq!(ColumnDefault::sql("now()").as_sql(), Some("now()"));
/// assert!(ColumnDefault::value(Value::Bool(false)).as_value().is_some());
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum ColumnDefault {
    /// A SQL expression, emitted verbatim into the migration.
    Sql(String),
    /// A literal, bound as a parameter when the default is applied in Rust.
    Literal(Value),
}

impl ColumnDefault {
    /// A SQL-expression default.
    ///
    /// ```
    /// use moso_orm::descriptor::ColumnDefault;
    ///
    /// assert_eq!(ColumnDefault::sql("uuid_generate_v7()").as_sql(), Some("uuid_generate_v7()"));
    /// ```
    #[must_use]
    pub fn sql(expression: impl Into<String>) -> Self {
        Self::Sql(expression.into())
    }

    /// A literal default.
    ///
    /// ```
    /// use moso_orm::descriptor::ColumnDefault;
    /// use moso_sql::Value;
    ///
    /// assert_eq!(ColumnDefault::value(Value::I32(0)).as_value(), Some(&Value::I32(0)));
    /// ```
    #[must_use]
    pub const fn value(value: Value) -> Self {
        Self::Literal(value)
    }

    /// The SQL text, when this is an expression.
    ///
    /// ```
    /// use moso_orm::descriptor::ColumnDefault;
    /// use moso_sql::Value;
    ///
    /// assert!(ColumnDefault::value(Value::Bool(true)).as_sql().is_none());
    /// ```
    #[must_use]
    pub fn as_sql(&self) -> Option<&str> {
        match self {
            Self::Sql(text) => Some(text),
            Self::Literal(_) => None,
        }
    }

    /// The literal, when this is one.
    ///
    /// ```
    /// use moso_orm::descriptor::ColumnDefault;
    ///
    /// assert!(ColumnDefault::sql("now()").as_value().is_none());
    /// ```
    #[must_use]
    pub const fn as_value(&self) -> Option<&Value> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Sql(_) => None,
        }
    }
}

/// An index, described.
///
/// Carries everything the migration generator needs to emit a `CREATE INDEX`
/// that survives a diff: the method, the partial predicate and the included
/// columns, not only the column list.
///
/// ```
/// use moso_orm::descriptor::IndexDescriptor;
/// use moso_sql::ddl::IndexMethod;
/// use moso_sql::{Expr, Ident};
///
/// let live_emails = IndexDescriptor::builder("users_email_live_idx")
///     .column(Ident::from_static("email"))
///     .unique()
///     .method(IndexMethod::BTree)
///     .predicate(Expr::col(Ident::from_static("deleted_at")).is_null())
///     .build();
///
/// assert!(live_emails.is_unique());
/// assert!(live_emails.is_partial());
/// ```
#[derive(Clone, Debug)]
pub struct IndexDescriptor {
    name: Ident,
    columns: Vec<IndexColumn>,
    unique: bool,
    method: Option<IndexMethod>,
    predicate: Option<Expr>,
    include: Vec<Ident>,
    nulls_not_distinct: bool,
}

impl IndexDescriptor {
    /// Starts describing an index.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid SQL identifier. Index names come from the
    /// derive, which generates them, so this is a compile-time check in
    /// practice.
    ///
    /// ```
    /// use moso_orm::descriptor::IndexDescriptor;
    ///
    /// let i = IndexDescriptor::builder("users_email_idx").build();
    /// assert_eq!(i.name().as_str(), "users_email_idx");
    /// ```
    #[must_use]
    pub const fn builder(name: &'static str) -> IndexDescriptorBuilder {
        IndexDescriptorBuilder {
            index: Self {
                name: Ident::from_static(name),
                columns: Vec::new(),
                unique: false,
                method: None,
                predicate: None,
                include: Vec::new(),
                nulls_not_distinct: false,
            },
        }
    }

    /// The index name.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert_eq!(IndexDescriptor::builder("i").build().name().as_str(), "i");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The indexed columns and expressions, in key order.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").build().columns().is_empty());
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[IndexColumn] {
        &self.columns
    }

    /// Whether the index enforces uniqueness.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").unique().build().is_unique());
    /// ```
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// The access method, when one was chosen.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").build().method().is_none());
    /// ```
    #[must_use]
    pub const fn method(&self) -> Option<&IndexMethod> {
        self.method.as_ref()
    }

    /// The partial-index predicate, when there is one.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").build().predicate().is_none());
    /// ```
    #[must_use]
    pub const fn predicate(&self) -> Option<&Expr> {
        self.predicate.as_ref()
    }

    /// Whether the index covers only part of the table.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(!IndexDescriptor::builder("i").build().is_partial());
    /// ```
    #[must_use]
    pub const fn is_partial(&self) -> bool {
        self.predicate.is_some()
    }

    /// Non-key columns stored in the leaf pages (`INCLUDE`).
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").build().included().is_empty());
    /// ```
    #[must_use]
    pub fn included(&self) -> &[Ident] {
        &self.include
    }

    /// Whether two `NULL`s conflict under this unique index.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(!IndexDescriptor::builder("i").build().nulls_not_distinct());
    /// ```
    #[must_use]
    pub const fn nulls_not_distinct(&self) -> bool {
        self.nulls_not_distinct
    }
}

/// Assembles an [`IndexDescriptor`].
///
/// ```
/// use moso_orm::descriptor::IndexDescriptor;
/// use moso_sql::Ident;
///
/// let i = IndexDescriptor::builder("posts_slug_idx")
///     .column(Ident::from_static("slug"))
///     .unique()
///     .build();
/// assert!(i.is_unique());
/// ```
#[derive(Clone, Debug)]
pub struct IndexDescriptorBuilder {
    index: IndexDescriptor,
}

impl IndexDescriptorBuilder {
    /// Appends an indexed column, ascending with the default `NULLS` placement.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// # use moso_sql::Ident;
    /// let i = IndexDescriptor::builder("i").column(Ident::from_static("a")).build();
    /// assert_eq!(i.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn column(mut self, name: Ident) -> Self {
        self.index.columns.push(IndexColumn::column(name));
        self
    }

    /// Appends an indexed column or expression with its sort options.
    ///
    /// ```
    /// # use moso_orm::descriptor::{IndexColumn, IndexDescriptor};
    /// # use moso_sql::{Ident, Order};
    /// let target = IndexColumn::column(Ident::from_static("created_at")).order(Order::Desc);
    /// let i = IndexDescriptor::builder("i").target(target).build();
    /// assert_eq!(i.columns().len(), 1);
    /// ```
    #[must_use]
    pub fn target(mut self, column: IndexColumn) -> Self {
        self.index.columns.push(column);
        self
    }

    /// Makes the index unique.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").unique().build().is_unique());
    /// ```
    #[must_use]
    pub const fn unique(mut self) -> Self {
        self.index.unique = true;
        self
    }

    /// Chooses the access method.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// # use moso_sql::ddl::IndexMethod;
    /// let i = IndexDescriptor::builder("i").method(IndexMethod::Gin).build();
    /// assert!(i.method().is_some());
    /// ```
    #[must_use]
    pub fn method(mut self, method: IndexMethod) -> Self {
        self.index.method = Some(method);
        self
    }

    /// Makes the index partial.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// # use moso_sql::{Expr, Ident};
    /// let i = IndexDescriptor::builder("i")
    ///     .predicate(Expr::col(Ident::from_static("deleted_at")).is_null())
    ///     .build();
    /// assert!(i.is_partial());
    /// ```
    #[must_use]
    pub fn predicate(mut self, predicate: Expr) -> Self {
        self.index.predicate = Some(predicate);
        self
    }

    /// Adds `INCLUDE` columns.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// # use moso_sql::Ident;
    /// let i = IndexDescriptor::builder("i").include([Ident::from_static("title")]).build();
    /// assert_eq!(i.included().len(), 1);
    /// ```
    #[must_use]
    pub fn include(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.index.include.extend(columns);
        self
    }

    /// Makes two `NULL`s conflict under a unique index.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert!(IndexDescriptor::builder("i").nulls_not_distinct().build().nulls_not_distinct());
    /// ```
    #[must_use]
    pub const fn nulls_not_distinct(mut self) -> Self {
        self.index.nulls_not_distinct = true;
        self
    }

    /// Finishes the index.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexDescriptor;
    /// assert_eq!(IndexDescriptor::builder("i").build().name().as_str(), "i");
    /// ```
    #[must_use]
    pub fn build(self) -> IndexDescriptor {
        self.index
    }
}

/// One key of an index: a column or an expression, with its sort options.
///
/// ```
/// use moso_orm::descriptor::IndexColumn;
/// use moso_sql::{Ident, Nulls, Order};
///
/// let newest_first = IndexColumn::column(Ident::from_static("created_at"))
///     .order(Order::Desc)
///     .nulls(Nulls::Last);
/// assert_eq!(newest_first.sort_order(), Some(Order::Desc));
/// ```
#[derive(Clone, Debug)]
pub struct IndexColumn {
    expr: Expr,
    column: Option<Ident>,
    order: Option<Order>,
    nulls: Option<Nulls>,
    operator_class: Option<Ident>,
}

impl IndexColumn {
    /// A plain column key.
    ///
    /// ```
    /// use moso_orm::descriptor::IndexColumn;
    /// use moso_sql::Ident;
    ///
    /// let key = IndexColumn::column(Ident::from_static("email"));
    /// assert_eq!(key.column_name().map(moso_sql::Ident::as_str), Some("email"));
    /// ```
    #[must_use]
    pub fn column(name: Ident) -> Self {
        Self {
            expr: Expr::col(name.clone()),
            column: Some(name),
            order: None,
            nulls: None,
            operator_class: None,
        }
    }

    /// An expression key, such as `lower(email)`.
    ///
    /// ```
    /// use moso_orm::descriptor::IndexColumn;
    /// use moso_sql::RawExpr;
    ///
    /// let key = IndexColumn::expression(RawExpr::new("lower(email)").into_expr());
    /// assert!(key.column_name().is_none());
    /// ```
    #[must_use]
    pub const fn expression(expr: Expr) -> Self {
        Self {
            expr,
            column: None,
            order: None,
            nulls: None,
            operator_class: None,
        }
    }

    /// Sets the sort direction.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::{Ident, Order};
    /// let key = IndexColumn::column(Ident::from_static("a")).order(Order::Desc);
    /// assert_eq!(key.sort_order(), Some(Order::Desc));
    /// ```
    #[must_use]
    pub const fn order(mut self, order: Order) -> Self {
        self.order = Some(order);
        self
    }

    /// Sets where `NULL`s sort.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::{Ident, Nulls};
    /// let key = IndexColumn::column(Ident::from_static("a")).nulls(Nulls::First);
    /// assert_eq!(key.nulls_placement(), Some(Nulls::First));
    /// ```
    #[must_use]
    pub const fn nulls(mut self, nulls: Nulls) -> Self {
        self.nulls = Some(nulls);
        self
    }

    /// Sets the operator class, such as `gin_trgm_ops`.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// let key = IndexColumn::column(Ident::from_static("a"))
    ///     .operator_class(Ident::from_static("gin_trgm_ops"));
    /// assert!(key.operator_class_name().is_some());
    /// ```
    #[must_use]
    pub fn operator_class(mut self, class: Ident) -> Self {
        self.operator_class = Some(class);
        self
    }

    /// The key as an expression, which is what the DDL builder wants.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// let key = IndexColumn::column(Ident::from_static("a"));
    /// assert!(key.expr().as_column().is_some());
    /// ```
    #[must_use]
    pub const fn expr(&self) -> &Expr {
        &self.expr
    }

    /// The key as an owned expression.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// assert!(IndexColumn::column(Ident::from_static("a")).into_expr().as_column().is_some());
    /// ```
    #[must_use]
    pub fn into_expr(self) -> Expr {
        self.expr
    }

    /// The column name, when the key is a plain column.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// assert!(IndexColumn::column(Ident::from_static("a")).column_name().is_some());
    /// ```
    #[must_use]
    pub const fn column_name(&self) -> Option<&Ident> {
        self.column.as_ref()
    }

    /// The sort direction, when one was set.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// assert!(IndexColumn::column(Ident::from_static("a")).sort_order().is_none());
    /// ```
    #[must_use]
    pub const fn sort_order(&self) -> Option<Order> {
        self.order
    }

    /// Where `NULL`s sort, when it was set.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// assert!(IndexColumn::column(Ident::from_static("a")).nulls_placement().is_none());
    /// ```
    #[must_use]
    pub const fn nulls_placement(&self) -> Option<Nulls> {
        self.nulls
    }

    /// The operator class, when one was set.
    ///
    /// ```
    /// # use moso_orm::descriptor::IndexColumn;
    /// # use moso_sql::Ident;
    /// assert!(IndexColumn::column(Ident::from_static("a")).operator_class_name().is_none());
    /// ```
    #[must_use]
    pub const fn operator_class_name(&self) -> Option<&Ident> {
        self.operator_class.as_ref()
    }
}

/// A table-level `CHECK`.
///
/// The expression is text so that the migration file reads the way the entity
/// declared it.
///
/// ```
/// use moso_orm::descriptor::CheckDescriptor;
///
/// let check = CheckDescriptor::new("orders_total_positive", "total >= 0");
/// assert_eq!(check.expression(), "total >= 0");
/// ```
#[derive(Clone, Debug)]
pub struct CheckDescriptor {
    name: Ident,
    expression: String,
}

impl CheckDescriptor {
    /// A named check.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid SQL identifier.
    ///
    /// ```
    /// use moso_orm::descriptor::CheckDescriptor;
    ///
    /// assert_eq!(CheckDescriptor::new("c", "x > 0").name().as_str(), "c");
    /// ```
    #[must_use]
    pub fn new(name: &'static str, expression: impl Into<String>) -> Self {
        Self {
            name: Ident::from_static(name),
            expression: expression.into(),
        }
    }

    /// The constraint name.
    ///
    /// ```
    /// # use moso_orm::descriptor::CheckDescriptor;
    /// assert_eq!(CheckDescriptor::new("c", "x > 0").name().as_str(), "c");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The predicate, as written.
    ///
    /// ```
    /// # use moso_orm::descriptor::CheckDescriptor;
    /// assert_eq!(CheckDescriptor::new("c", "x > 0").expression(), "x > 0");
    /// ```
    #[must_use]
    pub fn expression(&self) -> &str {
        &self.expression
    }
}

/// A foreign key this table owns.
///
/// ```
/// use moso_orm::descriptor::ForeignKeyDescriptor;
/// use moso_sql::ddl::ReferentialAction;
/// use moso_sql::{Ident, TableRef};
///
/// let author = ForeignKeyDescriptor::builder("posts_author_id_fkey", TableRef::from_static("users"))
///     .column(Ident::from_static("author_id"), Ident::from_static("id"))
///     .on_delete(ReferentialAction::Cascade)
///     .build();
///
/// assert_eq!(author.on_delete(), Some(ReferentialAction::Cascade));
/// assert_eq!(author.columns().len(), 1);
/// ```
#[derive(Clone, Debug)]
pub struct ForeignKeyDescriptor {
    name: Ident,
    columns: Vec<Ident>,
    target: TableRef,
    target_columns: Vec<Ident>,
    on_delete: Option<ReferentialAction>,
    on_update: Option<ReferentialAction>,
    deferrable: bool,
    initially_deferred: bool,
}

impl ForeignKeyDescriptor {
    /// Starts describing a foreign key into `target`.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid SQL identifier.
    ///
    /// ```
    /// use moso_orm::descriptor::ForeignKeyDescriptor;
    /// use moso_sql::TableRef;
    ///
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("users")).build();
    /// assert_eq!(fk.target().name().as_str(), "users");
    /// ```
    #[must_use]
    pub const fn builder(name: &'static str, target: TableRef) -> ForeignKeyDescriptorBuilder {
        ForeignKeyDescriptorBuilder {
            foreign_key: Self {
                name: Ident::from_static(name),
                columns: Vec::new(),
                target,
                target_columns: Vec::new(),
                on_delete: None,
                on_update: None,
                deferrable: false,
                initially_deferred: false,
            },
        }
    }

    /// The constraint name.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert_eq!(fk.name().as_str(), "fk");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &Ident {
        &self.name
    }

    /// The local columns, in key order.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert!(fk.columns().is_empty());
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// The referenced table.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("users")).build();
    /// assert_eq!(fk.target().name().as_str(), "users");
    /// ```
    #[must_use]
    pub const fn target(&self) -> &TableRef {
        &self.target
    }

    /// The referenced columns, in the same order as [`Self::columns`].
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert!(fk.target_columns().is_empty());
    /// ```
    #[must_use]
    pub fn target_columns(&self) -> &[Ident] {
        &self.target_columns
    }

    /// What happens to this row when the referenced row is deleted.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert!(fk.on_delete().is_none());
    /// ```
    #[must_use]
    pub const fn on_delete(&self) -> Option<ReferentialAction> {
        self.on_delete
    }

    /// What happens to this row when the referenced key changes.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert!(fk.on_update().is_none());
    /// ```
    #[must_use]
    pub const fn on_update(&self) -> Option<ReferentialAction> {
        self.on_update
    }

    /// Whether the constraint can be deferred to commit time.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert!(!fk.is_deferrable());
    /// ```
    #[must_use]
    pub const fn is_deferrable(&self) -> bool {
        self.deferrable
    }

    /// Whether it is deferred by default.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert!(!fk.is_initially_deferred());
    /// ```
    #[must_use]
    pub const fn is_initially_deferred(&self) -> bool {
        self.initially_deferred
    }
}

/// Assembles a [`ForeignKeyDescriptor`].
///
/// ```
/// use moso_orm::descriptor::ForeignKeyDescriptor;
/// use moso_sql::{Ident, TableRef};
///
/// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("users"))
///     .column(Ident::from_static("author_id"), Ident::from_static("id"))
///     .build();
/// assert_eq!(fk.columns().len(), 1);
/// ```
#[derive(Clone, Debug)]
pub struct ForeignKeyDescriptorBuilder {
    foreign_key: ForeignKeyDescriptor,
}

impl ForeignKeyDescriptorBuilder {
    /// Adds a column pair.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::{Ident, TableRef};
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u"))
    ///     .column(Ident::from_static("a"), Ident::from_static("b"))
    ///     .build();
    /// assert_eq!(fk.target_columns().len(), 1);
    /// ```
    #[must_use]
    pub fn column(mut self, local: Ident, target: Ident) -> Self {
        self.foreign_key.columns.push(local);
        self.foreign_key.target_columns.push(target);
        self
    }

    /// Sets `ON DELETE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::ddl::ReferentialAction;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u"))
    ///     .on_delete(ReferentialAction::SetNull)
    ///     .build();
    /// assert_eq!(fk.on_delete(), Some(ReferentialAction::SetNull));
    /// ```
    #[must_use]
    pub const fn on_delete(mut self, action: ReferentialAction) -> Self {
        self.foreign_key.on_delete = Some(action);
        self
    }

    /// Sets `ON UPDATE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::ddl::ReferentialAction;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u"))
    ///     .on_update(ReferentialAction::Cascade)
    ///     .build();
    /// assert_eq!(fk.on_update(), Some(ReferentialAction::Cascade));
    /// ```
    #[must_use]
    pub const fn on_update(mut self, action: ReferentialAction) -> Self {
        self.foreign_key.on_update = Some(action);
        self
    }

    /// Makes the constraint deferrable.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u"))
    ///     .deferrable(true)
    ///     .build();
    /// assert!(fk.is_initially_deferred());
    /// ```
    #[must_use]
    pub const fn deferrable(mut self, initially_deferred: bool) -> Self {
        self.foreign_key.deferrable = true;
        self.foreign_key.initially_deferred = initially_deferred;
        self
    }

    /// Finishes the foreign key.
    ///
    /// ```
    /// # use moso_orm::descriptor::ForeignKeyDescriptor;
    /// # use moso_sql::TableRef;
    /// let fk = ForeignKeyDescriptor::builder("fk", TableRef::from_static("u")).build();
    /// assert_eq!(fk.name().as_str(), "fk");
    /// ```
    #[must_use]
    pub fn build(self) -> ForeignKeyDescriptor {
        self.foreign_key
    }
}

/// A relation, described.
///
/// The admin renders these as links and as pickers; the query builder reads
/// them to know how to join and how to batch a preload.
///
/// ```
/// use moso_orm::descriptor::{RelationDescriptor, RelationKind};
/// use moso_sql::Ident;
///
/// let comments = RelationDescriptor::builder("comments", RelationKind::HasMany, "Comment")
///     .foreign_key(Ident::from_static("post_id"))
///     .build();
///
/// assert_eq!(comments.kind(), RelationKind::HasMany);
/// assert!(!comments.kind().owns_the_foreign_key());
/// ```
#[derive(Clone, Debug)]
pub struct RelationDescriptor {
    name: &'static str,
    kind: RelationKind,
    target: &'static str,
    target_table: Option<TableRef>,
    foreign_key: Vec<Ident>,
    local_key: Vec<Ident>,
    through: Option<JoinTableDescriptor>,
    on_delete: Option<ReferentialAction>,
    on_update: Option<ReferentialAction>,
    nullable: bool,
    self_referential: bool,
    polymorphic: Option<PolymorphicDescriptor>,
}

impl RelationDescriptor {
    /// Starts describing a relation named `name` to entity `target`.
    ///
    /// ```
    /// use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    ///
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert_eq!(r.name(), "author");
    /// ```
    #[must_use]
    pub const fn builder(
        name: &'static str,
        kind: RelationKind,
        target: &'static str,
    ) -> RelationDescriptorBuilder {
        RelationDescriptorBuilder {
            relation: Self {
                name,
                kind,
                target,
                target_table: None,
                foreign_key: Vec::new(),
                local_key: Vec::new(),
                through: None,
                on_delete: None,
                on_update: None,
                nullable: false,
                self_referential: false,
                polymorphic: None,
            },
        }
    }

    /// The relation's name, as the field is spelled.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert_eq!(r.name(), "author");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// Which of the four shapes this relation is.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("tags", RelationKind::ManyToMany, "Tag").build();
    /// assert_eq!(r.kind(), RelationKind::ManyToMany);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> RelationKind {
        self.kind
    }

    /// The related entity's Rust type name.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert_eq!(r.target(), "User");
    /// ```
    #[must_use]
    pub const fn target(&self) -> &'static str {
        self.target
    }

    /// The related table, when the derive could name it.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert!(r.target_table().is_none());
    /// ```
    #[must_use]
    pub const fn target_table(&self) -> Option<&TableRef> {
        self.target_table.as_ref()
    }

    /// The foreign-key columns, on whichever side owns them.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert!(r.foreign_key().is_empty());
    /// ```
    #[must_use]
    pub fn foreign_key(&self) -> &[Ident] {
        &self.foreign_key
    }

    /// The keys on this side that the foreign key points at — the primary key
    /// unless the relation overrode it.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert!(r.local_key().is_empty());
    /// ```
    #[must_use]
    pub fn local_key(&self) -> &[Ident] {
        &self.local_key
    }

    /// The join table, for a many-to-many.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("tags", RelationKind::ManyToMany, "Tag").build();
    /// assert!(r.through().is_none());
    /// ```
    #[must_use]
    pub const fn through(&self) -> Option<&JoinTableDescriptor> {
        self.through.as_ref()
    }

    /// The `ON DELETE` the migration emits for this relation's constraint.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert!(r.on_delete().is_none());
    /// ```
    #[must_use]
    pub const fn on_delete(&self) -> Option<ReferentialAction> {
        self.on_delete
    }

    /// The `ON UPDATE` the migration emits for this relation's constraint.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert!(r.on_update().is_none());
    /// ```
    #[must_use]
    pub const fn on_update(&self) -> Option<ReferentialAction> {
        self.on_update
    }

    /// Whether the relation may be absent — `Related<Option<T>>`.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("stats", RelationKind::HasOne, "Stats").nullable().build();
    /// assert!(r.is_nullable());
    /// ```
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// Whether the relation points back at the same table.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("parent", RelationKind::BelongsTo, "Category")
    ///     .self_referential()
    ///     .build();
    /// assert!(r.is_self_referential());
    /// ```
    #[must_use]
    pub const fn is_self_referential(&self) -> bool {
        self.self_referential
    }

    /// The polymorphic description, when the relation can point at more than
    /// one entity.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("target", RelationKind::BelongsTo, "Any").build();
    /// assert!(r.polymorphic().is_none());
    /// ```
    #[must_use]
    pub const fn polymorphic(&self) -> Option<&PolymorphicDescriptor> {
        self.polymorphic.as_ref()
    }
}

/// Assembles a [`RelationDescriptor`].
///
/// ```
/// use moso_orm::descriptor::{RelationDescriptor, RelationKind};
/// use moso_sql::Ident;
///
/// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
///     .foreign_key(Ident::from_static("author_id"))
///     .build();
/// assert_eq!(r.foreign_key().len(), 1);
/// ```
#[derive(Clone, Debug)]
pub struct RelationDescriptorBuilder {
    relation: RelationDescriptor,
}

impl RelationDescriptorBuilder {
    /// Names the related table.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// # use moso_sql::TableRef;
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
    ///     .target_table(TableRef::from_static("users"))
    ///     .build();
    /// assert!(r.target_table().is_some());
    /// ```
    #[must_use]
    pub fn target_table(mut self, table: TableRef) -> Self {
        self.relation.target_table = Some(table);
        self
    }

    /// Adds a foreign-key column.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// # use moso_sql::Ident;
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
    ///     .foreign_key(Ident::from_static("author_id"))
    ///     .build();
    /// assert_eq!(r.foreign_key().len(), 1);
    /// ```
    #[must_use]
    pub fn foreign_key(mut self, column: Ident) -> Self {
        self.relation.foreign_key.push(column);
        self
    }

    /// Adds a local key the foreign key points at.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// # use moso_sql::Ident;
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
    ///     .local_key(Ident::from_static("id"))
    ///     .build();
    /// assert_eq!(r.local_key().len(), 1);
    /// ```
    #[must_use]
    pub fn local_key(mut self, column: Ident) -> Self {
        self.relation.local_key.push(column);
        self
    }

    /// Sets the join table for a many-to-many.
    ///
    /// ```
    /// # use moso_orm::descriptor::{JoinTableDescriptor, RelationDescriptor, RelationKind};
    /// # use moso_sql::{Ident, TableRef};
    /// let through = JoinTableDescriptor::new(
    ///     TableRef::from_static("post_tags"),
    ///     Ident::from_static("post_id"),
    ///     Ident::from_static("tag_id"),
    /// );
    /// let r = RelationDescriptor::builder("tags", RelationKind::ManyToMany, "Tag")
    ///     .through(through)
    ///     .build();
    /// assert!(r.through().is_some());
    /// ```
    #[must_use]
    pub fn through(mut self, join_table: JoinTableDescriptor) -> Self {
        self.relation.through = Some(join_table);
        self
    }

    /// Sets `ON DELETE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// # use moso_sql::ddl::ReferentialAction;
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
    ///     .on_delete(ReferentialAction::Cascade)
    ///     .build();
    /// assert!(r.on_delete().is_some());
    /// ```
    #[must_use]
    pub const fn on_delete(mut self, action: ReferentialAction) -> Self {
        self.relation.on_delete = Some(action);
        self
    }

    /// Sets `ON UPDATE`.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// # use moso_sql::ddl::ReferentialAction;
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User")
    ///     .on_update(ReferentialAction::Restrict)
    ///     .build();
    /// assert!(r.on_update().is_some());
    /// ```
    #[must_use]
    pub const fn on_update(mut self, action: ReferentialAction) -> Self {
        self.relation.on_update = Some(action);
        self
    }

    /// Marks the relation optional.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("stats", RelationKind::HasOne, "S").nullable().build();
    /// assert!(r.is_nullable());
    /// ```
    #[must_use]
    pub const fn nullable(mut self) -> Self {
        self.relation.nullable = true;
        self
    }

    /// Marks the relation self-referential.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("parent", RelationKind::BelongsTo, "C")
    ///     .self_referential()
    ///     .build();
    /// assert!(r.is_self_referential());
    /// ```
    #[must_use]
    pub const fn self_referential(mut self) -> Self {
        self.relation.self_referential = true;
        self
    }

    /// Makes the relation polymorphic.
    ///
    /// ```
    /// # use moso_orm::descriptor::{PolymorphicDescriptor, RelationDescriptor, RelationKind};
    /// # use moso_sql::Ident;
    /// let poly = PolymorphicDescriptor::new(
    ///     Ident::from_static("target_type"),
    ///     Ident::from_static("target_id"),
    ///     ["Post", "Comment"],
    /// );
    /// let r = RelationDescriptor::builder("target", RelationKind::BelongsTo, "Any")
    ///     .polymorphic(poly)
    ///     .build();
    /// assert!(r.polymorphic().is_some());
    /// ```
    #[must_use]
    pub fn polymorphic(mut self, polymorphic: PolymorphicDescriptor) -> Self {
        self.relation.polymorphic = Some(polymorphic);
        self
    }

    /// Finishes the relation.
    ///
    /// ```
    /// # use moso_orm::descriptor::{RelationDescriptor, RelationKind};
    /// let r = RelationDescriptor::builder("author", RelationKind::BelongsTo, "User").build();
    /// assert_eq!(r.target(), "User");
    /// ```
    #[must_use]
    pub fn build(self) -> RelationDescriptor {
        self.relation
    }
}

/// Which of the four relation shapes a relation is.
///
/// ```
/// use moso_orm::descriptor::RelationKind;
///
/// assert!(RelationKind::BelongsTo.owns_the_foreign_key());
/// assert!(RelationKind::HasMany.is_collection());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RelationKind {
    /// The foreign key is on this table; the other side is one row.
    BelongsTo,
    /// The foreign key is on the other table; the other side is many rows.
    HasMany,
    /// The foreign key is on the other table; the other side is at most one row.
    HasOne,
    /// A join table holds both foreign keys.
    ManyToMany,
}

impl RelationKind {
    /// Whether this table holds the foreign key.
    ///
    /// ```
    /// use moso_orm::descriptor::RelationKind;
    ///
    /// assert!(RelationKind::BelongsTo.owns_the_foreign_key());
    /// assert!(!RelationKind::HasOne.owns_the_foreign_key());
    /// ```
    #[must_use]
    pub const fn owns_the_foreign_key(self) -> bool {
        matches!(self, Self::BelongsTo)
    }

    /// Whether loading it produces many rows.
    ///
    /// ```
    /// use moso_orm::descriptor::RelationKind;
    ///
    /// assert!(RelationKind::ManyToMany.is_collection());
    /// assert!(!RelationKind::BelongsTo.is_collection());
    /// ```
    #[must_use]
    pub const fn is_collection(self) -> bool {
        matches!(self, Self::HasMany | Self::ManyToMany)
    }

    /// How many statements preloading this relation costs — always one, which
    /// is non-negotiable N3 stated as data.
    ///
    /// ```
    /// use moso_orm::descriptor::RelationKind;
    ///
    /// assert_eq!(RelationKind::HasMany.statements_per_preload(), 1);
    /// ```
    #[must_use]
    pub const fn statements_per_preload(self) -> usize {
        1
    }
}

/// The join table of a many-to-many.
///
/// ```
/// use moso_orm::descriptor::JoinTableDescriptor;
/// use moso_sql::{Ident, TableRef};
///
/// let post_tags = JoinTableDescriptor::new(
///     TableRef::from_static("post_tags"),
///     Ident::from_static("post_id"),
///     Ident::from_static("tag_id"),
/// );
/// assert_eq!(post_tags.left().as_str(), "post_id");
/// ```
#[derive(Clone, Debug)]
pub struct JoinTableDescriptor {
    table: TableRef,
    left: Ident,
    right: Ident,
}

impl JoinTableDescriptor {
    /// A join table with its two foreign-key columns.
    ///
    /// ```
    /// use moso_orm::descriptor::JoinTableDescriptor;
    /// use moso_sql::{Ident, TableRef};
    ///
    /// let t = JoinTableDescriptor::new(
    ///     TableRef::from_static("post_tags"),
    ///     Ident::from_static("post_id"),
    ///     Ident::from_static("tag_id"),
    /// );
    /// assert_eq!(t.table().name().as_str(), "post_tags");
    /// ```
    #[must_use]
    pub const fn new(table: TableRef, left: Ident, right: Ident) -> Self {
        Self { table, left, right }
    }

    /// The join table itself.
    ///
    /// ```
    /// # use moso_orm::descriptor::JoinTableDescriptor;
    /// # use moso_sql::{Ident, TableRef};
    /// let t = JoinTableDescriptor::new(
    ///     TableRef::from_static("pt"), Ident::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(t.table().name().as_str(), "pt");
    /// ```
    #[must_use]
    pub const fn table(&self) -> &TableRef {
        &self.table
    }

    /// The column pointing back at this entity.
    ///
    /// ```
    /// # use moso_orm::descriptor::JoinTableDescriptor;
    /// # use moso_sql::{Ident, TableRef};
    /// let t = JoinTableDescriptor::new(
    ///     TableRef::from_static("pt"), Ident::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(t.left().as_str(), "a");
    /// ```
    #[must_use]
    pub const fn left(&self) -> &Ident {
        &self.left
    }

    /// The column pointing at the related entity.
    ///
    /// ```
    /// # use moso_orm::descriptor::JoinTableDescriptor;
    /// # use moso_sql::{Ident, TableRef};
    /// let t = JoinTableDescriptor::new(
    ///     TableRef::from_static("pt"), Ident::from_static("a"), Ident::from_static("b"));
    /// assert_eq!(t.right().as_str(), "b");
    /// ```
    #[must_use]
    pub const fn right(&self) -> &Ident {
        &self.right
    }
}

/// A relation that can point at more than one entity, discriminated by a type
/// column.
///
/// ```
/// use moso_orm::descriptor::PolymorphicDescriptor;
/// use moso_sql::Ident;
///
/// let target = PolymorphicDescriptor::new(
///     Ident::from_static("target_type"),
///     Ident::from_static("target_id"),
///     ["Post", "Comment"],
/// );
/// assert_eq!(target.targets(), ["Post", "Comment"]);
/// ```
#[derive(Clone, Debug)]
pub struct PolymorphicDescriptor {
    type_column: Ident,
    id_column: Ident,
    targets: Vec<&'static str>,
}

impl PolymorphicDescriptor {
    /// A polymorphic relation over `targets`.
    ///
    /// ```
    /// use moso_orm::descriptor::PolymorphicDescriptor;
    /// use moso_sql::Ident;
    ///
    /// let p = PolymorphicDescriptor::new(
    ///     Ident::from_static("t"), Ident::from_static("i"), ["Post"]);
    /// assert_eq!(p.targets().len(), 1);
    /// ```
    #[must_use]
    pub fn new(
        type_column: Ident,
        id_column: Ident,
        targets: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            type_column,
            id_column,
            targets: targets.into_iter().collect(),
        }
    }

    /// The column holding the target's discriminator.
    ///
    /// ```
    /// # use moso_orm::descriptor::PolymorphicDescriptor;
    /// # use moso_sql::Ident;
    /// let p = PolymorphicDescriptor::new(Ident::from_static("t"), Ident::from_static("i"), ["P"]);
    /// assert_eq!(p.type_column().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn type_column(&self) -> &Ident {
        &self.type_column
    }

    /// The column holding the target's identifier.
    ///
    /// ```
    /// # use moso_orm::descriptor::PolymorphicDescriptor;
    /// # use moso_sql::Ident;
    /// let p = PolymorphicDescriptor::new(Ident::from_static("t"), Ident::from_static("i"), ["P"]);
    /// assert_eq!(p.id_column().as_str(), "i");
    /// ```
    #[must_use]
    pub const fn id_column(&self) -> &Ident {
        &self.id_column
    }

    /// Every entity the relation can point at.
    ///
    /// ```
    /// # use moso_orm::descriptor::PolymorphicDescriptor;
    /// # use moso_sql::Ident;
    /// let p = PolymorphicDescriptor::new(Ident::from_static("t"), Ident::from_static("i"), ["P"]);
    /// assert_eq!(p.targets(), ["P"]);
    /// ```
    #[must_use]
    pub fn targets(&self) -> &[&'static str] {
        &self.targets
    }
}

/// An enum type an entity's columns need.
///
/// ```
/// use moso_orm::EnumStorage;
/// use moso_orm::descriptor::EnumTypeDescriptor;
///
/// let status = EnumTypeDescriptor::new("order_status", EnumStorage::PgEnum, ["pending", "paid"]);
/// assert_eq!(status.variants(), ["pending", "paid"]);
/// assert!(status.needs_a_type());
/// ```
#[derive(Clone, Debug)]
pub struct EnumTypeDescriptor {
    name: TypeRef,
    storage: EnumStorage,
    variants: Vec<&'static str>,
}

impl EnumTypeDescriptor {
    /// An enum type with its variants, in declaration order.
    ///
    /// # Panics
    ///
    /// If `name` is not a valid SQL identifier.
    ///
    /// ```
    /// use moso_orm::EnumStorage;
    /// use moso_orm::descriptor::EnumTypeDescriptor;
    ///
    /// let e = EnumTypeDescriptor::new("mood", EnumStorage::Text, ["happy"]);
    /// assert_eq!(e.name().name().as_str(), "mood");
    /// ```
    #[must_use]
    pub fn new(
        name: &'static str,
        storage: EnumStorage,
        variants: impl IntoIterator<Item = &'static str>,
    ) -> Self {
        Self {
            name: TypeRef::from_static(name),
            storage,
            variants: variants.into_iter().collect(),
        }
    }

    /// The type name.
    ///
    /// ```
    /// # use moso_orm::EnumStorage;
    /// # use moso_orm::descriptor::EnumTypeDescriptor;
    /// let e = EnumTypeDescriptor::new("mood", EnumStorage::Text, ["happy"]);
    /// assert_eq!(e.name().name().as_str(), "mood");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &TypeRef {
        &self.name
    }

    /// How the values are stored.
    ///
    /// ```
    /// # use moso_orm::EnumStorage;
    /// # use moso_orm::descriptor::EnumTypeDescriptor;
    /// let e = EnumTypeDescriptor::new("mood", EnumStorage::Int, ["happy"]);
    /// assert_eq!(e.storage(), EnumStorage::Int);
    /// ```
    #[must_use]
    pub const fn storage(&self) -> EnumStorage {
        self.storage
    }

    /// Every variant's stored spelling.
    ///
    /// ```
    /// # use moso_orm::EnumStorage;
    /// # use moso_orm::descriptor::EnumTypeDescriptor;
    /// let e = EnumTypeDescriptor::new("mood", EnumStorage::Text, ["happy", "sad"]);
    /// assert_eq!(e.variants().len(), 2);
    /// ```
    #[must_use]
    pub fn variants(&self) -> &[&'static str] {
        &self.variants
    }

    /// Whether a `CREATE TYPE` has to be emitted.
    ///
    /// ```
    /// # use moso_orm::EnumStorage;
    /// # use moso_orm::descriptor::EnumTypeDescriptor;
    /// let e = EnumTypeDescriptor::new("mood", EnumStorage::Text, ["happy"]);
    /// assert!(!e.needs_a_type());
    /// ```
    #[must_use]
    pub const fn needs_a_type(&self) -> bool {
        self.storage.needs_a_type()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn users() -> EntityDescriptor {
        EntityDescriptor::builder("User", TableRef::from_static("users"))
            .column(
                ColumnDescriptor::builder(Ident::from_static("id"), DataType::Uuid)
                    .field("id")
                    .primary_key()
                    .default(ColumnDefault::sql("uuid_generate_v7()"))
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(Ident::from_static("email"), DataType::Text)
                    .field("email")
                    .unique()
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(Ident::from_static("search"), DataType::TsVector)
                    .field("search")
                    .readonly()
                    .build(),
            )
            .column(
                ColumnDescriptor::builder(
                    Ident::from_static("deleted_at"),
                    DataType::Timestamp {
                        with_time_zone: true,
                    },
                )
                .field("deleted_at")
                .nullable()
                .role(ColumnRole::SoftDelete)
                .build(),
            )
            .soft_delete("deleted_at")
            .timestamps("created_at", "updated_at")
            .index(
                IndexDescriptor::builder("users_email_live_idx")
                    .column(Ident::from_static("email"))
                    .unique()
                    .predicate(Expr::col(Ident::from_static("deleted_at")).is_null())
                    .build(),
            )
            .relation(
                RelationDescriptor::builder("posts", RelationKind::HasMany, "Post")
                    .foreign_key(Ident::from_static("author_id"))
                    .build(),
            )
            .build()
    }

    #[test]
    fn the_descriptor_answers_what_migrate_asks() {
        let users = users();
        assert_eq!(users.table().name().as_str(), "users");
        assert_eq!(users.primary_key().len(), 1);
        assert!(
            users
                .column("email")
                .is_some_and(ColumnDescriptor::is_unique)
        );
        assert!(users.indexes()[0].is_partial());
        assert!(users.indexes()[0].is_unique());
        assert_eq!(
            users.column("id").and_then(ColumnDescriptor::default),
            Some(&ColumnDefault::sql("uuid_generate_v7()"))
        );
    }

    #[test]
    fn the_descriptor_answers_what_admin_asks() {
        let users = users();
        assert_eq!(users.relations().len(), 1);
        assert!(
            users
                .relation("posts")
                .is_some_and(|r| r.kind().is_collection())
        );
        assert_eq!(
            users.column_for_field("email").map(|c| c.name().as_str()),
            Some("email")
        );
        assert!(users.is_soft_deletable());
        assert!(!users.is_tenant_scoped());
    }

    #[test]
    fn readonly_and_generated_columns_are_not_insertable() {
        let users = users();
        let insertable: Vec<_> = users
            .insertable()
            .iter()
            .map(|column| column.name().as_str())
            .collect();
        assert!(!insertable.contains(&"search"), "{insertable:?}");
        assert!(insertable.contains(&"email"), "{insertable:?}");
    }

    #[test]
    fn a_relation_kind_knows_which_side_owns_the_key() {
        assert!(RelationKind::BelongsTo.owns_the_foreign_key());
        assert!(!RelationKind::HasMany.owns_the_foreign_key());
        assert!(!RelationKind::HasOne.owns_the_foreign_key());
        assert!(!RelationKind::ManyToMany.owns_the_foreign_key());
        // N3: every kind is one statement, whatever the row count.
        for kind in [
            RelationKind::BelongsTo,
            RelationKind::HasMany,
            RelationKind::HasOne,
            RelationKind::ManyToMany,
        ] {
            assert_eq!(kind.statements_per_preload(), 1);
        }
    }

    #[test]
    fn framework_managed_columns_are_identifiable() {
        assert!(ColumnRole::CreatedAt.is_framework_managed());
        assert!(ColumnRole::UpdatedAt.is_framework_managed());
        assert!(ColumnRole::SoftDelete.is_framework_managed());
        assert!(ColumnRole::Version.is_framework_managed());
        assert!(!ColumnRole::Data.is_framework_managed());
        assert!(!ColumnRole::Tenant.is_framework_managed());
    }
}
