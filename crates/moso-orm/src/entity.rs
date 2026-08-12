//! [`Entity`] — the trait `#[derive(Entity)]` implements, and the cheap
//! compile-time column list that hangs off it.
//!
//! # Two descriptions, deliberately
//!
//! [`Entity::COLUMNS`] is a `const`: names, kinds and flags, no allocation, no
//! `OnceLock`, usable from a `const fn`. Every query built at runtime reads it.
//!
//! [`Entity::descriptor`] is the rich one: SQL types, defaults, indexes,
//! relations, everything a migration diff or an admin form needs. It allocates,
//! so it is behind a `OnceLock` the derive writes and nothing on the query path
//! touches it.
//!
//! One would have been simpler and would have put a `Vec<String>` on the hot
//! path.

use core::marker::PhantomData;

use moso_sql::{Expr, Ident, TableRef, ValueKind};

use crate::descriptor::{ColumnRole, EntityDescriptor};
use crate::row::{DecodeError, Row};
use crate::sqltype::SqlType;

/// A Rust struct that is one table.
///
/// Written by `#[derive(Entity)]`. Implementing it by hand is supported and
/// rarely worth it — the derive also generates the column constants, the
/// `NewEntity` insert struct and the relation constants, none of which this
/// trait can express.
///
/// # ADR-0008
///
/// An entity is deliberately **not** a [`Schema`](moso_schema::Schema).
/// Returning one from a handler is a compile error, because entities carry
/// password hashes, tenant ids and soft-delete timestamps. Write an output DTO
/// with `#[schema(from = User)]` — three lines, generated, and checked when the
/// entity changes.
///
/// ```
/// use moso_orm::descriptor::EntityDescriptor;
/// use moso_orm::{ColumnDef, DecodeError, Entity, Row};
/// use moso_sql::{TableRef, ValueKind};
/// use std::sync::OnceLock;
///
/// /// A country, as the lookup table stores one.
/// #[derive(Clone, Debug)]
/// pub struct Country {
///     /// ISO 3166-1 alpha-2.
///     pub code: String,
///     /// The English name.
///     pub name: String,
/// }
///
/// impl Entity for Country {
///     type Pk = String;
///
///     const TABLE: TableRef = TableRef::from_static("countries");
///     const COLUMNS: &'static [ColumnDef] = &[
///         ColumnDef::new("code", ValueKind::Text).primary_key(),
///         ColumnDef::new("name", ValueKind::Text),
///     ];
///     const NAME: &'static str = "Country";
///
///     fn pk(&self) -> Self::Pk {
///         self.code.clone()
///     }
///
///     fn from_row(row: &Row) -> Result<Self, DecodeError> {
///         Ok(Self { code: row.get_string(0)?, name: row.get_string(1)? })
///     }
///
///     fn descriptor() -> &'static EntityDescriptor {
///         static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
///         DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Country", Self::TABLE).build())
///     }
/// }
///
/// assert_eq!(Country::TABLE.name().as_str(), "countries");
/// assert_eq!(Country::COLUMNS.len(), 2);
/// assert_eq!(Country::NAME, "Country");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a database entity",
    label = "not an entity",
    note = "an entity is a struct that maps to one table, with a primary key",
    note = "help: write `#[derive(moso::Entity)]` above `{Self}`, and mark its key \
            `#[entity(pk)]`",
    note = "if `{Self}` is a response type, it is a `Schema`, not an entity — the two are \
            deliberately different (ADR-0008)"
)]
pub trait Entity: Sized + Send + Sync + 'static {
    /// The primary key's Rust type. A composite key is a tuple.
    type Pk: SqlType + Clone + Send + Sync + 'static;

    /// The table, with its schema when one was declared.
    const TABLE: TableRef;

    /// Every column, in the order [`Entity::from_row`] reads them.
    const COLUMNS: &'static [ColumnDef];

    /// The Rust type's name, for diagnostics.
    ///
    /// The derive emits the bare identifier — `"User"` — so a message never
    /// prints a module path. There is no default: `core::any::type_name` is not
    /// a `const fn` on stable, and a name that is sometimes a path and
    /// sometimes an identifier makes diagnostics inconsistent.
    const NAME: &'static str;

    /// This row's primary key.
    ///
    /// ```
    /// # use moso_orm::Entity;
    /// fn key_of<E: Entity>(entity: &E) -> E::Pk {
    ///     entity.pk()
    /// }
    /// ```
    fn pk(&self) -> Self::Pk;

    /// Decodes one row, **positionally**.
    ///
    /// Column `i` is `COLUMNS[i]`, because the query that produced the row was
    /// built from the same list. No name is hashed, and no column is looked up.
    ///
    /// # Errors
    ///
    /// [`DecodeError`] naming the entity, the field and both types.
    ///
    /// ```
    /// # use moso_orm::{DecodeError, Entity, Row};
    /// fn decode<E: Entity>(row: &Row) -> Result<E, DecodeError> {
    ///     E::from_row(row)
    /// }
    /// ```
    fn from_row(row: &Row) -> Result<Self, DecodeError>;

    /// The rich description, for `moso-migrate` and `moso-admin`.
    ///
    /// Built once, behind a `OnceLock`. Nothing on the query path calls it.
    ///
    /// ```
    /// # use moso_orm::Entity;
    /// fn table_of<E: Entity>() -> &'static str {
    ///     E::descriptor().table().name().as_str()
    /// }
    /// ```
    fn descriptor() -> &'static EntityDescriptor;

    /// The primary key's column names, derived from [`Entity::COLUMNS`].
    ///
    /// ```
    /// # use moso_orm::Entity;
    /// fn key_columns<E: Entity>() -> Vec<&'static str> {
    ///     E::primary_key_columns()
    /// }
    /// ```
    #[must_use]
    fn primary_key_columns() -> Vec<&'static str> {
        Self::COLUMNS
            .iter()
            .filter(|column| column.is_primary_key())
            .map(ColumnDef::name)
            .collect()
    }
}

/// One column, as a compile-time constant.
///
/// Everything here is `const`-constructible, so `Entity::COLUMNS` costs no
/// allocation and no lazy initialisation. The rich form — SQL type, default,
/// comment — lives in
/// [`ColumnDescriptor`](crate::descriptor::ColumnDescriptor).
///
/// ```
/// use moso_orm::{ColumnDef, ColumnRole};
/// use moso_sql::ValueKind;
///
/// const EMAIL: ColumnDef = ColumnDef::new("email", ValueKind::Text).unique();
/// assert!(EMAIL.is_unique());
/// assert!(EMAIL.is_writable());
/// assert_eq!(EMAIL.column_role(), ColumnRole::Data);
/// ```
///
/// # Why it is `Copy`
///
/// `#[entity(embedded)]` splices an embedded value object's columns into its
/// owner's [`Entity::COLUMNS`], and that splice happens in a `const` block —
/// see [`concat_columns`]. A `const fn` cannot call `Clone::clone`, so the type
/// it copies has to be `Copy`. Every field already is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnDef {
    name: &'static str,
    kind: ValueKind,
    nullable: bool,
    primary_key: bool,
    unique: bool,
    readonly: bool,
    has_default: bool,
    role: ColumnRole,
}

impl ColumnDef {
    /// A plain, required, writable column.
    ///
    /// ```
    /// use moso_orm::ColumnDef;
    /// use moso_sql::ValueKind;
    ///
    /// const NAME: ColumnDef = ColumnDef::new("name", ValueKind::Text);
    /// assert_eq!(NAME.name(), "name");
    /// ```
    #[must_use]
    pub const fn new(name: &'static str, kind: ValueKind) -> Self {
        Self {
            name,
            kind,
            nullable: false,
            primary_key: false,
            unique: false,
            readonly: false,
            has_default: false,
            role: ColumnRole::Data,
        }
    }

    /// Marks the column part of the primary key.
    ///
    /// ```
    /// use moso_orm::ColumnDef;
    /// use moso_sql::ValueKind;
    ///
    /// const ID: ColumnDef = ColumnDef::new("id", ValueKind::Uuid).primary_key();
    /// assert!(ID.is_primary_key());
    /// ```
    #[must_use]
    pub const fn primary_key(mut self) -> Self {
        self.primary_key = true;
        self
    }

    /// Marks the column nullable.
    ///
    /// ```
    /// use moso_orm::ColumnDef;
    /// use moso_sql::ValueKind;
    ///
    /// const BIO: ColumnDef = ColumnDef::new("bio", ValueKind::Text).nullable();
    /// assert!(BIO.is_nullable());
    /// ```
    #[must_use]
    pub const fn nullable(mut self) -> Self {
        self.nullable = true;
        self
    }

    /// Marks the column `UNIQUE`.
    ///
    /// ```
    /// use moso_orm::ColumnDef;
    /// use moso_sql::ValueKind;
    ///
    /// const EMAIL: ColumnDef = ColumnDef::new("email", ValueKind::Text).unique();
    /// assert!(EMAIL.is_unique());
    /// ```
    #[must_use]
    pub const fn unique(mut self) -> Self {
        self.unique = true;
        self
    }

    /// Excludes the column from every `INSERT` and `UPDATE`.
    ///
    /// ```
    /// use moso_orm::ColumnDef;
    /// use moso_sql::ValueKind;
    ///
    /// const TSV: ColumnDef = ColumnDef::new("search", ValueKind::Text).readonly();
    /// assert!(!TSV.is_writable());
    /// ```
    #[must_use]
    pub const fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }

    /// Records that the database supplies a value when none is given, which is
    /// what makes the field `Option` in the generated insert struct.
    ///
    /// ```
    /// use moso_orm::ColumnDef;
    /// use moso_sql::ValueKind;
    ///
    /// const ID: ColumnDef = ColumnDef::new("id", ValueKind::Uuid).with_default();
    /// assert!(ID.has_default());
    /// ```
    #[must_use]
    pub const fn with_default(mut self) -> Self {
        self.has_default = true;
        self
    }

    /// Sets what the column is for.
    ///
    /// ```
    /// use moso_orm::{ColumnDef, ColumnRole};
    /// use moso_sql::ValueKind;
    ///
    /// const AT: ColumnDef =
    ///     ColumnDef::new("updated_at", ValueKind::Timestamp).role(ColumnRole::UpdatedAt);
    /// assert_eq!(AT.column_role(), ColumnRole::UpdatedAt);
    /// ```
    #[must_use]
    pub const fn role(mut self, role: ColumnRole) -> Self {
        self.role = role;
        self
    }

    /// The SQL column name.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert_eq!(ColumnDef::new("id", ValueKind::Uuid).name(), "id");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &'static str {
        self.name
    }

    /// The column name as a validated identifier.
    ///
    /// # Panics
    ///
    /// If the name is not a valid SQL identifier — which the derive checks at
    /// compile time, so this is unreachable from generated code.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert_eq!(ColumnDef::new("id", ValueKind::Uuid).ident().as_str(), "id");
    /// ```
    #[must_use]
    pub const fn ident(&self) -> Ident {
        Ident::from_static(self.name)
    }

    /// The parameter type values of this column bind as.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert_eq!(ColumnDef::new("id", ValueKind::Uuid).kind(), ValueKind::Uuid);
    /// ```
    #[must_use]
    pub const fn kind(&self) -> ValueKind {
        self.kind
    }

    /// Whether the column accepts `NULL`.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert!(!ColumnDef::new("id", ValueKind::Uuid).is_nullable());
    /// ```
    #[must_use]
    pub const fn is_nullable(&self) -> bool {
        self.nullable
    }

    /// Whether the column is part of the primary key.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert!(ColumnDef::new("id", ValueKind::Uuid).primary_key().is_primary_key());
    /// ```
    #[must_use]
    pub const fn is_primary_key(&self) -> bool {
        self.primary_key
    }

    /// Whether the column is `UNIQUE`.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert!(!ColumnDef::new("id", ValueKind::Uuid).is_unique());
    /// ```
    #[must_use]
    pub const fn is_unique(&self) -> bool {
        self.unique
    }

    /// Whether the database supplies a value when none is given.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert!(!ColumnDef::new("id", ValueKind::Uuid).has_default());
    /// ```
    #[must_use]
    pub const fn has_default(&self) -> bool {
        self.has_default
    }

    /// Whether an `INSERT` or an `UPDATE` may write it.
    ///
    /// ```
    /// # use moso_orm::ColumnDef;
    /// # use moso_sql::ValueKind;
    /// assert!(ColumnDef::new("id", ValueKind::Uuid).is_writable());
    /// ```
    #[must_use]
    pub const fn is_writable(&self) -> bool {
        !self.readonly
    }

    /// What the column is for, beyond holding data.
    ///
    /// ```
    /// # use moso_orm::{ColumnDef, ColumnRole};
    /// # use moso_sql::ValueKind;
    /// assert_eq!(ColumnDef::new("id", ValueKind::Uuid).column_role(), ColumnRole::Data);
    /// ```
    #[must_use]
    pub const fn column_role(&self) -> ColumnRole {
        self.role
    }
}

/// The total length of several column lists, as a `const`.
///
/// One half of the `#[entity(embedded)]` splice: an embedded value object
/// contributes its own [`ColumnDef`] list to its owner's
/// [`Entity::COLUMNS`], and the owner's list is a `const`, so the length has to
/// be computable in a `const` context before the array can be built.
///
/// ```
/// use moso_orm::ColumnDef;
/// use moso_orm::entity::total_columns;
/// use moso_sql::ValueKind;
///
/// const A: &[ColumnDef] = &[ColumnDef::new("id", ValueKind::I64)];
/// const B: &[ColumnDef] = &[
///     ColumnDef::new("line1", ValueKind::Text),
///     ColumnDef::new("city", ValueKind::Text),
/// ];
/// const TOTAL: usize = total_columns(&[A, B]);
/// assert_eq!(TOTAL, 3);
/// ```
#[must_use]
pub const fn total_columns(parts: &[&[ColumnDef]]) -> usize {
    let mut total = 0;
    let mut part = 0;
    while part < parts.len() {
        total += parts[part].len();
        part += 1;
    }
    total
}

/// Concatenates several column lists into one array, as a `const`.
///
/// The other half of the `#[entity(embedded)]` splice. `N` comes from
/// [`total_columns`] over the same slice; a mismatch is a compile-time index
/// error rather than a wrong column list.
///
/// # Panics
///
/// If `N` is smaller than the total length of `parts` — which cannot happen in
/// generated code, because both come from the same expression.
///
/// ```
/// use moso_orm::ColumnDef;
/// use moso_orm::entity::{concat_columns, total_columns};
/// use moso_sql::ValueKind;
///
/// const OWN: &[ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// const EMBEDDED: &[ColumnDef] = &[ColumnDef::new("address_city", ValueKind::Text)];
/// const PARTS: &[&[ColumnDef]] = &[OWN, EMBEDDED];
/// const ALL: [ColumnDef; total_columns(PARTS)] = concat_columns(PARTS);
///
/// assert_eq!(ALL.len(), 2);
/// assert_eq!(ALL[1].name(), "address_city");
/// ```
#[must_use]
pub const fn concat_columns<const N: usize>(parts: &[&[ColumnDef]]) -> [ColumnDef; N] {
    let mut out = [ColumnDef::new("__moso_unwritten", ValueKind::Bool); N];
    let mut written = 0;
    let mut part = 0;
    while part < parts.len() {
        let list = parts[part];
        let mut index = 0;
        while index < list.len() {
            out[written] = list[index];
            written += 1;
            index += 1;
        }
        part += 1;
    }
    assert!(
        written == N,
        "moso: the spliced column list is shorter than the array it fills — this is a bug in \
         `#[derive(Entity)]`, not in your code"
    );
    out
}

/// The total length of several name lists, as a `const`.
///
/// The [`NewEntity::COLUMNS`] half of the same splice.
///
/// ```
/// use moso_orm::entity::total_names;
///
/// const A: &[&str] = &["email"];
/// const B: &[&str] = &["address_city", "address_line1"];
/// assert_eq!(total_names(&[A, B]), 3);
/// ```
#[must_use]
pub const fn total_names(parts: &[&[&'static str]]) -> usize {
    let mut total = 0;
    let mut part = 0;
    while part < parts.len() {
        total += parts[part].len();
        part += 1;
    }
    total
}

/// Concatenates several name lists into one array, as a `const`.
///
/// # Panics
///
/// If `N` is smaller than the total length of `parts`.
///
/// ```
/// use moso_orm::entity::{concat_names, total_names};
///
/// const PARTS: &[&[&str]] = &[&["email"], &["address_city"]];
/// const ALL: [&str; total_names(PARTS)] = concat_names(PARTS);
/// assert_eq!(ALL, ["email", "address_city"]);
/// ```
#[must_use]
pub const fn concat_names<const N: usize>(parts: &[&[&'static str]]) -> [&'static str; N] {
    let mut out = ["__moso_unwritten"; N];
    let mut written = 0;
    let mut part = 0;
    while part < parts.len() {
        let list = parts[part];
        let mut index = 0;
        while index < list.len() {
            out[written] = list[index];
            written += 1;
            index += 1;
        }
        part += 1;
    }
    assert!(
        written == N,
        "moso: the spliced name list is shorter than the array it fills — this is a bug in \
         `#[derive(Entity)]`, not in your code"
    );
    out
}

/// The struct `#[derive(Entity)]` generates for inserting one row.
///
/// It has the entity's fields **minus** the ones the database supplies: a
/// primary key with a default, `created_at`, `updated_at`, read-only and
/// generated columns, and relations. That is the answer to "construct a struct
/// with four dummy fields to insert a row", which is the most common complaint
/// about `ActiveModel`-shaped APIs.
///
/// ```
/// use moso_orm::NewEntity;
/// use moso_sql::{Expr, Ident};
///
/// /// What has to be supplied to create a tag.
/// pub struct NewTag {
///     /// The display name.
///     pub name: String,
/// }
///
/// impl NewEntity for NewTag {
///     const COLUMNS: &'static [&'static str] = &["name"];
///
///     fn into_row(self) -> Vec<Expr> {
///         vec![Expr::value(self.name)]
///     }
/// }
///
/// assert_eq!(NewTag::COLUMNS, ["name"]);
/// assert_eq!(NewTag { name: "rust".into() }.into_row().len(), 1);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not an insertable row",
    label = "not insertable",
    note = "`Entity::insert` takes the `New…` struct `#[derive(Entity)]` generates — for `User` \
            that is `NewUser`",
    note = "help: build a `New…` value, or implement `NewEntity for {Self}`"
)]
pub trait NewEntity: Send + Sync + 'static {
    /// The columns this row supplies, in the order [`NewEntity::into_row`]
    /// produces values.
    const COLUMNS: &'static [&'static str];

    /// The values, in `COLUMNS` order.
    ///
    /// ```
    /// # use moso_orm::NewEntity;
    /// # use moso_sql::Expr;
    /// fn row_of<N: NewEntity>(new: N) -> Vec<Expr> {
    ///     new.into_row()
    /// }
    /// ```
    fn into_row(self) -> Vec<Expr>;

    /// The columns as validated identifiers.
    ///
    /// # Panics
    ///
    /// If a column name is not a valid SQL identifier, which the derive checks
    /// at compile time.
    ///
    /// ```
    /// # use moso_orm::NewEntity;
    /// # use moso_sql::Ident;
    /// fn idents<N: NewEntity>() -> Vec<Ident> {
    ///     N::idents()
    /// }
    /// ```
    #[must_use]
    fn idents() -> Vec<Ident> {
        Self::COLUMNS
            .iter()
            .map(|name| Ident::new(*name).expect("a generated column name is a valid identifier"))
            .collect()
    }
}

/// Proof that an entity has no outstanding scope obligation, so its queries may
/// run.
///
/// This is the *only* thing the second parameter of
/// [`Select`](crate::Select) is used for. The joined-entity set is checked at
/// build time instead — see the module documentation of [`crate::select`] for
/// why, and for what the error looks like.
///
/// `()` — the default — implements it for every entity. `NeedsTenant` does not,
/// which is what turns a missing tenant scope into a compile error rather than
/// a cross-tenant data leak.
///
/// ```
/// use moso_orm::Ready;
///
/// struct Invoice;
/// fn runnable<E, J: Ready<E>>() {}
/// runnable::<Invoice, ()>();
/// ```
#[diagnostic::on_unimplemented(
    message = "`{E}` is tenant-scoped and this query has no tenant",
    label = "no tenant",
    note = "a query that forgets its tenant reads another customer's rows, so the compiler \
            insists on one",
    note = "help: name the tenant: `{E}::query().scoped(tenant)`",
    note = "help: or, deliberately across every tenant: `{E}::query().across_tenants()`"
)]
pub trait Ready<E> {}

impl<E> Ready<E> for () {}

/// A query on a tenant-scoped entity that has not named its tenant yet.
///
/// The second parameter of [`Select`](crate::Select) when
/// `#[entity(tenant = "…")]` is set. It implements no [`Ready`], so
/// `fetch_all` does not exist until `.scoped(..)` or `.across_tenants()`
/// discharges it.
///
/// ```
/// use moso_orm::NeedsTenant;
///
/// // The marker is uninhabited-by-convention: it exists only in a type.
/// fn takes_marker(_marker: core::marker::PhantomData<NeedsTenant>) {}
/// takes_marker(core::marker::PhantomData);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct NeedsTenant;

/// A typed reference to an entity's table, for the rare code that is generic
/// over the entity.
///
/// ```
/// use moso_orm::{Entity, EntityRef};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_orm::{ColumnDef, DecodeError, Row};
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct Tag { pub id: i64 }
/// # impl Entity for Tag {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("tags");
/// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "Tag";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("Tag", Self::TABLE).build())
/// #     }
/// # }
/// let tags = EntityRef::<Tag>::new();
/// assert_eq!(tags.table().name().as_str(), "tags");
/// assert_eq!(tags.name(), "Tag");
/// ```
pub struct EntityRef<E>(PhantomData<fn() -> E>);

impl<E: Entity> EntityRef<E> {
    /// A reference to `E`'s table.
    ///
    /// ```
    /// # use moso_orm::EntityRef;
    /// # use moso_orm::Entity;
    /// fn make<E: Entity>() -> EntityRef<E> {
    ///     EntityRef::new()
    /// }
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self(PhantomData)
    }

    /// The table.
    ///
    /// ```
    /// # use moso_orm::{Entity, EntityRef};
    /// fn table<E: Entity>() -> moso_sql::TableRef {
    ///     EntityRef::<E>::new().table()
    /// }
    /// ```
    #[must_use]
    pub fn table(&self) -> TableRef {
        E::TABLE
    }

    /// The entity's Rust type name.
    ///
    /// ```
    /// # use moso_orm::{Entity, EntityRef};
    /// fn name<E: Entity>() -> &'static str {
    ///     EntityRef::<E>::new().name()
    /// }
    /// ```
    #[must_use]
    pub fn name(&self) -> &'static str {
        E::NAME
    }
}

impl<E: Entity> Default for EntityRef<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Clone for EntityRef<E> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<E> Copy for EntityRef<E> {}

impl<E: Entity> core::fmt::Debug for EntityRef<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "EntityRef<{}>", E::NAME)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COLUMNS: &[ColumnDef] = &[
        ColumnDef::new("id", ValueKind::Uuid)
            .primary_key()
            .with_default(),
        ColumnDef::new("email", ValueKind::Text).unique(),
        ColumnDef::new("bio", ValueKind::Text).nullable(),
        ColumnDef::new("search", ValueKind::Text).readonly(),
        ColumnDef::new("created_at", ValueKind::Timestamp).role(ColumnRole::CreatedAt),
    ];

    #[test]
    fn the_column_list_is_a_compile_time_constant() {
        // If this compiles at all, `ColumnDef` is const-constructible, which is
        // the property that keeps `Entity::COLUMNS` off the allocation path.
        assert_eq!(COLUMNS.len(), 5);
        assert!(COLUMNS[0].is_primary_key());
        assert!(COLUMNS[0].has_default());
        assert!(COLUMNS[1].is_unique());
        assert!(COLUMNS[2].is_nullable());
        assert!(!COLUMNS[3].is_writable());
        assert_eq!(COLUMNS[4].column_role(), ColumnRole::CreatedAt);
    }

    #[test]
    fn a_column_name_becomes_an_ident_without_allocating_twice() {
        const ID: ColumnDef = ColumnDef::new("id", ValueKind::Uuid);
        const IDENT: Ident = ID.ident();
        assert_eq!(IDENT.as_str(), "id");
    }

    #[test]
    fn the_unit_scope_is_ready_for_every_entity() {
        fn assert_ready<E, J: Ready<E>>() {}
        struct Invoice;
        struct User;
        assert_ready::<Invoice, ()>();
        assert_ready::<User, ()>();
    }

    #[test]
    fn needs_tenant_is_not_ready() {
        // A negative trait bound cannot be asserted in Rust, so this records the
        // intent: `NeedsTenant` deliberately has no `Ready` impl, and the UI
        // test in `moso-ui-tests` is what proves the error message.
        assert_eq!(core::mem::size_of::<NeedsTenant>(), 0);
    }
}
