//! User-defined types: `CREATE TYPE`, `ALTER TYPE`, `DROP TYPE`.
//!
//! PostgreSQL enum types are the reason this module exists. They are the
//! honest storage for a closed Rust `enum` — a `check` constraint over `text`
//! costs an extra index and does not appear in the catalogue — and they are
//! also the awkward one to migrate, because a variant can be added but not
//! removed.

use crate::ident::{Ident, TypeRef};

/// `CREATE TYPE`.
///
/// ```
/// use moso_sql::ddl::{CreateType, TypeBody};
/// use moso_sql::TypeRef;
///
/// let status = CreateType::new(
///     TypeRef::from_static("order_status"),
///     TypeBody::enumeration(["pending", "paid", "shipped"]),
/// );
/// assert_eq!(status.name().name().as_str(), "order_status");
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct CreateType {
    name: TypeRef,
    body: TypeBody,
}

impl CreateType {
    /// A user-defined type.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateType, TypeBody}, TypeRef};
    /// let t = CreateType::new(TypeRef::from_static("t"), TypeBody::enumeration(["a"]));
    /// assert!(matches!(t.body(), TypeBody::Enum(_)));
    /// ```
    #[must_use]
    pub const fn new(name: TypeRef, body: TypeBody) -> Self {
        Self { name, body }
    }

    /// The type name.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateType, TypeBody}, TypeRef};
    /// let t = CreateType::new(TypeRef::from_static("t"), TypeBody::enumeration(["a"]));
    /// assert_eq!(t.name().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &TypeRef {
        &self.name
    }

    /// What the type is made of.
    ///
    /// ```
    /// # use moso_sql::{ddl::{CreateType, TypeBody}, TypeRef};
    /// let t = CreateType::new(TypeRef::from_static("t"), TypeBody::enumeration(["a"]));
    /// assert!(matches!(t.body(), TypeBody::Enum(_)));
    /// ```
    #[must_use]
    pub const fn body(&self) -> &TypeBody {
        &self.body
    }
}

/// What a user-defined type is made of.
///
/// The labels of an enum are string *values*, not identifiers: they are bound
/// as parameters or quoted as literals, never spliced as syntax.
///
/// ```
/// use moso_sql::ddl::TypeBody;
///
/// let body = TypeBody::enumeration(["draft", "published"]);
/// assert!(matches!(body, TypeBody::Enum(_)));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum TypeBody {
    /// `CREATE TYPE … AS ENUM (…)`.
    Enum(Vec<String>),
}

impl TypeBody {
    /// An enum type with the given labels, in order.
    ///
    /// PostgreSQL sorts enum values by declaration order, so the order here is
    /// part of the schema, not a formatting detail.
    ///
    /// ```
    /// use moso_sql::ddl::TypeBody;
    ///
    /// assert_eq!(TypeBody::enumeration(["a", "b"]).labels(), Some(&["a".to_owned(), "b".to_owned()][..]));
    /// ```
    #[must_use]
    pub fn enumeration(labels: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Enum(labels.into_iter().map(Into::into).collect())
    }

    /// The enum labels, if this is an enum type.
    ///
    /// ```
    /// assert!(moso_sql::ddl::TypeBody::enumeration(["a"]).labels().is_some());
    /// ```
    #[must_use]
    pub fn labels(&self) -> Option<&[String]> {
        match self {
            Self::Enum(labels) => Some(labels),
        }
    }
}

/// `ALTER TYPE`.
///
/// ```
/// use moso_sql::ddl::{AlterType, AlterTypeAction};
/// use moso_sql::TypeRef;
///
/// let add = AlterType::new(
///     TypeRef::from_static("order_status"),
///     AlterTypeAction::add_value("refunded"),
/// );
/// assert!(add.requires_no_transaction());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct AlterType {
    name: TypeRef,
    action: AlterTypeAction,
}

impl AlterType {
    /// Alters a user-defined type.
    ///
    /// ```
    /// # use moso_sql::{ddl::{AlterType, AlterTypeAction}, TypeRef};
    /// let a = AlterType::new(TypeRef::from_static("t"), AlterTypeAction::add_value("x"));
    /// assert_eq!(a.name().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn new(name: TypeRef, action: AlterTypeAction) -> Self {
        Self { name, action }
    }

    /// The type being altered.
    ///
    /// ```
    /// # use moso_sql::{ddl::{AlterType, AlterTypeAction}, TypeRef};
    /// let a = AlterType::new(TypeRef::from_static("t"), AlterTypeAction::add_value("x"));
    /// assert_eq!(a.name().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &TypeRef {
        &self.name
    }

    /// The action.
    ///
    /// ```
    /// # use moso_sql::{ddl::{AlterType, AlterTypeAction}, TypeRef};
    /// let a = AlterType::new(TypeRef::from_static("t"), AlterTypeAction::add_value("x"));
    /// assert!(matches!(a.action(), AlterTypeAction::AddValue { .. }));
    /// ```
    #[must_use]
    pub const fn action(&self) -> &AlterTypeAction {
        &self.action
    }

    /// Whether the statement must run outside a transaction.
    ///
    /// `ALTER TYPE … ADD VALUE` could not run in one before PostgreSQL 12, and
    /// even on 12 and later the new value cannot be *used* in the same
    /// transaction. Treating it as non-transactional is the only behaviour
    /// that works on every supported server.
    ///
    /// ```
    /// # use moso_sql::{ddl::{AlterType, AlterTypeAction}, Ident, TypeRef};
    /// let rename = AlterType::new(
    ///     TypeRef::from_static("t"),
    ///     AlterTypeAction::Rename(Ident::from_static("u")),
    /// );
    /// assert!(!rename.requires_no_transaction());
    /// ```
    #[must_use]
    pub const fn requires_no_transaction(&self) -> bool {
        matches!(self.action, AlterTypeAction::AddValue { .. })
    }
}

/// One action of an [`AlterType`].
///
/// ```
/// use moso_sql::ddl::AlterTypeAction;
///
/// let action = AlterTypeAction::add_value("refunded");
/// assert!(matches!(action, AlterTypeAction::AddValue { .. }));
/// ```
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum AlterTypeAction {
    /// `ADD VALUE`.
    ///
    /// There is no `DROP VALUE`: removing an enum label needs a new type, a
    /// column swap and a backfill. The migration generator emits that plan as
    /// a commented template rather than pretending it is one statement
    /// (`docs/02-data/23-migrations.md`).
    AddValue {
        /// The new label.
        value: String,
        /// Place it before this existing label.
        before: Option<String>,
        /// Place it after this existing label.
        after: Option<String>,
        /// `IF NOT EXISTS`, so re-running the migration is safe.
        if_not_exists: bool,
    },
    /// `RENAME VALUE … TO …`.
    RenameValue {
        /// The current label.
        from: String,
        /// The new label.
        to: String,
    },
    /// `RENAME TO …`.
    Rename(Ident),
    /// `SET SCHEMA …`.
    SetSchema(Ident),
}

impl AlterTypeAction {
    /// Appends a label at the end of the enum, with `IF NOT EXISTS`.
    ///
    /// ```
    /// use moso_sql::ddl::AlterTypeAction;
    ///
    /// let action = AlterTypeAction::add_value("refunded");
    /// assert!(matches!(action, AlterTypeAction::AddValue { if_not_exists: true, .. }));
    /// ```
    #[must_use]
    pub fn add_value(value: impl Into<String>) -> Self {
        Self::AddValue {
            value: value.into(),
            before: None,
            after: None,
            if_not_exists: true,
        }
    }
}

/// `DROP TYPE`.
///
/// ```
/// use moso_sql::ddl::DropType;
/// use moso_sql::TypeRef;
///
/// assert!(DropType::new(TypeRef::from_static("t")).if_exists().is_if_exists());
/// ```
#[derive(Clone, Debug, PartialEq)]
pub struct DropType {
    name: TypeRef,
    if_exists: bool,
    cascade: bool,
}

impl DropType {
    /// Drops a user-defined type.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropType, TypeRef};
    /// assert_eq!(DropType::new(TypeRef::from_static("t")).name().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn new(name: TypeRef) -> Self {
        Self {
            name,
            if_exists: false,
            cascade: false,
        }
    }

    /// `IF EXISTS`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropType, TypeRef};
    /// assert!(DropType::new(TypeRef::from_static("t")).if_exists().is_if_exists());
    /// ```
    #[must_use]
    pub const fn if_exists(mut self) -> Self {
        self.if_exists = true;
        self
    }

    /// `CASCADE`.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropType, TypeRef};
    /// assert!(DropType::new(TypeRef::from_static("t")).cascade().is_cascade());
    /// ```
    #[must_use]
    pub const fn cascade(mut self) -> Self {
        self.cascade = true;
        self
    }

    /// The type name.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropType, TypeRef};
    /// assert_eq!(DropType::new(TypeRef::from_static("t")).name().name().as_str(), "t");
    /// ```
    #[must_use]
    pub const fn name(&self) -> &TypeRef {
        &self.name
    }

    /// Whether `IF EXISTS` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropType, TypeRef};
    /// assert!(!DropType::new(TypeRef::from_static("t")).is_if_exists());
    /// ```
    #[must_use]
    pub const fn is_if_exists(&self) -> bool {
        self.if_exists
    }

    /// Whether `CASCADE` was asked for.
    ///
    /// ```
    /// # use moso_sql::{ddl::DropType, TypeRef};
    /// assert!(!DropType::new(TypeRef::from_static("t")).is_cascade());
    /// ```
    #[must_use]
    pub const fn is_cascade(&self) -> bool {
        self.cascade
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adding_an_enum_value_is_non_transactional() {
        let add = AlterType::new(
            TypeRef::from_static("order_status"),
            AlterTypeAction::add_value("refunded"),
        );
        assert!(add.requires_no_transaction());

        let rename = AlterType::new(
            TypeRef::from_static("order_status"),
            AlterTypeAction::RenameValue {
                from: "paid".to_owned(),
                to: "settled".to_owned(),
            },
        );
        assert!(!rename.requires_no_transaction());
    }

    #[test]
    fn enum_labels_keep_their_declaration_order() {
        let body = TypeBody::enumeration(["pending", "paid", "shipped"]);
        assert_eq!(
            body.labels().expect("an enum"),
            [
                "pending".to_owned(),
                "paid".to_owned(),
                "shipped".to_owned()
            ]
        );
    }
}
