//! Scopes — the predicates Moso adds to every query, and the reusable ones you
//! write yourself.
//!
//! # Two different things called a scope
//!
//! 1. **The predicates the framework adds.** A soft-deletable entity gets
//!    `deleted_at IS NULL` on every read; a tenant-scoped one gets
//!    `tenant_id = $n`. [`soft_delete_predicate`] and [`tenant_predicate`]
//!    build them from the entity's [`EntityDescriptor`](crate::EntityDescriptor),
//!    and they live here rather than inside [`Select`] because `UPDATE` and
//!    `DELETE` need exactly the same two predicates and must not drift from it.
//!
//! 2. **The reusable query fragments you name.** Because the builder is
//!    shape-stable (ADR-0007), a scope is just `Select<E> -> Select<E>`. Most
//!    of the time an inherent method is the right shape:
//!
//!    ```
//!    # use moso_orm::{Column, Entity, Select};
//!    fn active<E: Entity>(query: Select<E>, deleted: Column<E, Option<i64>>) -> Select<E> {
//!        query.filter(deleted.is_null())
//!    }
//!    ```
//!
//!    [`Scope`] is for the cases a function cannot cover: a scope chosen at
//!    runtime, stored in a struct, put in a map, or passed across an API
//!    boundary. It is a named, cloneable, composable value.
//!
//! ```
//! use moso_orm::scope::Scope;
//! # use moso_orm::{Column, ColumnDef, DecodeError, Entity, Row, Select};
//! # use moso_orm::descriptor::EntityDescriptor;
//! # use moso_sql::{TableRef, ValueKind};
//! # use std::sync::OnceLock;
//! # #[derive(Clone, Debug)] pub struct Post { pub id: i64 }
//! # impl Entity for Post {
//! #     type Pk = i64;
//! #     const TABLE: TableRef = TableRef::from_static("posts");
//! #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
//! #     const NAME: &'static str = "Post";
//! #     fn pk(&self) -> i64 { self.id }
//! #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
//! #     fn descriptor() -> &'static EntityDescriptor {
//! #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
//! #         D.get_or_init(|| EntityDescriptor::builder("Post", Self::TABLE).build())
//! #     }
//! # }
//! # const ID: Column<Post, i64> = Column::new("id");
//! let recent = Scope::new("recent", |query: Select<Post>| query.order_by(ID.desc()).limit(20));
//! let popular = Scope::new("popular", |query: Select<Post>| query.filter(ID.gt(100)));
//!
//! let both = recent.then(popular);
//! let query = both.apply(Select::<Post>::new());
//!
//! assert_eq!(both.name(), "recent+popular");
//! assert_eq!(query.filters().len(), 1);
//! assert_eq!(query.limit_value(), Some(20));
//! ```

use std::sync::Arc;

use moso_sql::{ColumnRef, Expr};

use crate::db::TenantId;
use crate::entity::Entity;
use crate::predicate::Predicate;
use crate::select::{Deleted, Select};

/// The `deleted_at IS NULL` (or `IS NOT NULL`) predicate for `E`, when `E` is
/// soft-deletable and `mode` asks for one.
///
/// Returns `None` when the entity has no soft-delete column — in which case
/// [`Deleted::Live`] is already the whole truth — and when `mode` is
/// [`Deleted::Any`].
///
/// ```
/// use moso_orm::scope::soft_delete_predicate;
/// # use moso_orm::{Deleted, Entity, Predicate};
/// fn live_rows_only<E: Entity>() -> Option<Predicate> {
///     soft_delete_predicate::<E>(Deleted::Live)
/// }
/// ```
#[must_use]
pub fn soft_delete_predicate<E: Entity>(mode: Deleted) -> Option<Predicate> {
    let column = E::descriptor().soft_delete()?;
    let reference = ColumnRef::qualified(E::TABLE.name().clone(), column.clone());
    let expr = match mode {
        Deleted::Live => Expr::column(reference).is_null(),
        Deleted::Only => Expr::column(reference).is_not_null(),
        Deleted::Any => return None,
    };
    Some(Predicate::of([E::NAME], expr))
}

/// The `tenant_id = $n` predicate for `E`, when `E` is tenant-scoped.
///
/// Returns `None` for an entity without a tenant column, so passing a tenant to
/// a query that has no use for one is silently harmless rather than an error:
/// `db.for_tenant(t)` sets it once for a whole request, and most of the
/// entities that request touches are not tenant-scoped.
///
/// ```
/// use moso_orm::scope::tenant_predicate;
/// # use moso_orm::{Entity, Predicate, TenantId};
/// fn only_this_tenant<E: Entity>(tenant: &TenantId) -> Option<Predicate> {
///     tenant_predicate::<E>(tenant)
/// }
/// ```
#[must_use]
pub fn tenant_predicate<E: Entity>(tenant: &TenantId) -> Option<Predicate> {
    let column = E::descriptor().tenant()?;
    let reference = ColumnRef::qualified(E::TABLE.name().clone(), column.clone());
    let expr = Expr::column(reference).eq(Expr::bound(tenant.value().clone()));
    Some(Predicate::of([E::NAME], expr))
}

/// Whether every query for `E` must name a tenant.
///
/// The compile-time half of this is [`NeedsTenant`](crate::NeedsTenant); this
/// is the build-time half, for the paths that assemble a statement
/// dynamically and have no type to lean on.
///
/// ```
/// use moso_orm::scope::requires_tenant;
/// # use moso_orm::Entity;
/// fn must_be_scoped<E: Entity>() -> bool {
///     requires_tenant::<E>()
/// }
/// ```
#[must_use]
pub fn requires_tenant<E: Entity>() -> bool {
    E::descriptor().is_tenant_scoped()
}

/// A named, reusable transformation of a query.
///
/// Shape stability is what makes this a plain value rather than a trait with
/// four type parameters: a scope is `Select<E, J> -> Select<E, J>`, so it
/// composes with [`Scope::then`] and applies with [`Scope::apply`].
///
/// The name is not decoration. It is what `Debug` prints and what
/// [`Scope::then`] concatenates, so a query assembled from six runtime-chosen
/// scopes can say which six.
///
/// ```
/// use moso_orm::scope::Scope;
/// # use moso_orm::{Column, ColumnDef, DecodeError, Entity, Row, Select};
/// # use moso_orm::descriptor::EntityDescriptor;
/// # use moso_sql::{TableRef, ValueKind};
/// # use std::sync::OnceLock;
/// # #[derive(Clone, Debug)] pub struct User { pub id: i64 }
/// # impl Entity for User {
/// #     type Pk = i64;
/// #     const TABLE: TableRef = TableRef::from_static("users");
/// #     const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
/// #     const NAME: &'static str = "User";
/// #     fn pk(&self) -> i64 { self.id }
/// #     fn from_row(row: &Row) -> Result<Self, DecodeError> { Ok(Self { id: row.get_i64(0)? }) }
/// #     fn descriptor() -> &'static EntityDescriptor {
/// #         static D: OnceLock<EntityDescriptor> = OnceLock::new();
/// #         D.get_or_init(|| EntityDescriptor::builder("User", Self::TABLE).build())
/// #     }
/// # }
/// # const ID: Column<User, i64> = Column::new("id");
/// let newest = Scope::new("newest", |q: Select<User>| q.order_by(ID.desc()));
/// let query = newest.apply(Select::<User>::new());
///
/// assert_eq!(query.order_terms().len(), 1);
/// assert_eq!(format!("{newest:?}"), r#"Scope("newest")"#);
/// ```
pub struct Scope<E: 'static, J: 'static = ()> {
    name: String,
    transform: Transform<E, J>,
}

/// The shared transformation a [`Scope`] wraps.
///
/// `Arc` rather than `Box` so a scope is cheap to clone into the four handlers
/// that use it; `Send + Sync` because a scope built once at boot is read from
/// every request task.
type Transform<E, J> = Arc<dyn Fn(Select<E, J>) -> Select<E, J> + Send + Sync>;

impl<E: Entity, J: 'static> Scope<E, J> {
    /// A scope called `name` that applies `transform`.
    ///
    /// ```
    /// use moso_orm::scope::Scope;
    /// # use moso_orm::{Entity, Select};
    /// fn capped<E: Entity>() -> Scope<E> {
    ///     Scope::new("capped", |query: Select<E>| query.limit(100))
    /// }
    /// ```
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        transform: impl Fn(Select<E, J>) -> Select<E, J> + Send + Sync + 'static,
    ) -> Self {
        Self {
            name: name.into(),
            transform: Arc::new(transform),
        }
    }

    /// The scope that changes nothing.
    ///
    /// The identity element of [`Scope::then`], and the sensible default for a
    /// configuration field of type `Scope<E>`.
    ///
    /// ```
    /// use moso_orm::scope::Scope;
    /// # use moso_orm::{Entity, Select};
    /// fn nothing<E: Entity>(query: Select<E>) -> Select<E> {
    ///     Scope::identity().apply(query)
    /// }
    /// ```
    #[must_use]
    pub fn identity() -> Self {
        Self::new("identity", |query| query)
    }

    /// Applies the scope.
    ///
    /// ```
    /// use moso_orm::scope::Scope;
    /// # use moso_orm::{Entity, Select};
    /// fn apply<E: Entity>(scope: &Scope<E>, query: Select<E>) -> Select<E> {
    ///     scope.apply(query)
    /// }
    /// ```
    #[must_use]
    pub fn apply(&self, query: Select<E, J>) -> Select<E, J> {
        (self.transform)(query)
    }

    /// `self`, then `next`.
    ///
    /// The name of the result is `"self+next"`, so a composed scope still says
    /// what it is made of.
    ///
    /// ```
    /// use moso_orm::scope::Scope;
    /// # use moso_orm::{Entity, Select};
    /// fn both<E: Entity>(a: Scope<E>, b: Scope<E>) -> Scope<E> {
    ///     a.then(b)
    /// }
    /// ```
    #[must_use]
    pub fn then(self, next: Self) -> Self {
        let name = format!("{}+{}", self.name, next.name);
        Self::new(name, move |query| next.apply(self.apply(query)))
    }
}

impl<E: 'static, J: 'static> Scope<E, J> {
    /// The scope's name.
    ///
    /// ```
    /// use moso_orm::scope::Scope;
    /// # use moso_orm::Entity;
    /// fn describe<E: Entity>(scope: &Scope<E>) -> String {
    ///     format!("applying `{}`", scope.name())
    /// }
    /// ```
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }
}

impl<E: 'static, J: 'static> Clone for Scope<E, J> {
    fn clone(&self) -> Self {
        Self {
            name: self.name.clone(),
            transform: Arc::clone(&self.transform),
        }
    }
}

impl<E: Entity, J: 'static> Default for Scope<E, J> {
    fn default() -> Self {
        Self::identity()
    }
}

impl<E: 'static, J: 'static> core::fmt::Debug for Scope<E, J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Scope({:?})", self.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::column::Column;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::row::{DecodeError, Row};
    use moso_sql::{TableRef, ValueKind};
    use std::sync::OnceLock;

    /// A plain entity: no soft delete, no tenant.
    #[derive(Clone, Debug)]
    struct Tag {
        id: i64,
    }

    impl Entity for Tag {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("tags");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
        const NAME: &'static str = "Tag";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Tag", Self::TABLE).build())
        }
    }

    /// An entity that is both soft-deletable and tenant-scoped.
    #[derive(Clone, Debug)]
    struct Invoice {
        id: i64,
    }

    impl Entity for Invoice {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("invoices");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
        const NAME: &'static str = "Invoice";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("Invoice", Self::TABLE)
                    .soft_delete("deleted_at")
                    .tenant("tenant_id")
                    .build()
            })
        }
    }

    const TAG_ID: Column<Tag, i64> = Column::new("id");

    #[test]
    fn an_entity_without_a_soft_delete_column_gets_no_predicate() {
        assert!(soft_delete_predicate::<Tag>(Deleted::Live).is_none());
        assert!(soft_delete_predicate::<Tag>(Deleted::Only).is_none());
        assert!(soft_delete_predicate::<Tag>(Deleted::Any).is_none());
    }

    #[test]
    fn the_soft_delete_predicate_flips_with_the_mode() {
        let live = soft_delete_predicate::<Invoice>(Deleted::Live).expect("soft-deletable");
        let only = soft_delete_predicate::<Invoice>(Deleted::Only).expect("soft-deletable");
        assert_eq!(live.entities(), ["Invoice"]);
        assert_ne!(live.expr(), only.expr());
        assert!(soft_delete_predicate::<Invoice>(Deleted::Any).is_none());
    }

    #[test]
    fn the_tenant_predicate_exists_only_for_a_tenant_scoped_entity() {
        let tenant = TenantId::of(7_i64);
        assert!(tenant_predicate::<Tag>(&tenant).is_none());

        let scoped = tenant_predicate::<Invoice>(&tenant).expect("tenant-scoped");
        assert_eq!(scoped.entities(), ["Invoice"]);
        assert!(requires_tenant::<Invoice>());
        assert!(!requires_tenant::<Tag>());
    }

    #[test]
    fn scopes_compose_in_order_and_keep_their_names() {
        let first = Scope::new("first", |query: Select<Tag>| query.filter(TAG_ID.gt(0)));
        let second = Scope::new("second", |query: Select<Tag>| query.limit(5));
        let both = first.then(second);

        assert_eq!(both.name(), "first+second");
        let query = both.apply(Select::<Tag>::new());
        assert_eq!(query.filters().len(), 1);
        assert_eq!(query.limit_value(), Some(5));
    }

    #[test]
    fn the_identity_scope_changes_nothing() {
        let query = Scope::<Tag>::identity().apply(Select::<Tag>::new().limit(3));
        assert_eq!(query.limit_value(), Some(3));
        assert!(query.filters().is_empty());
        assert_eq!(Scope::<Tag>::default().name(), "identity");
    }

    #[test]
    fn a_scope_is_cloneable_and_debuggable() {
        let scope = Scope::new("recent", |query: Select<Tag>| query.limit(1));
        let copy = scope.clone();
        assert_eq!(copy.name(), "recent");
        assert_eq!(format!("{scope:?}"), r#"Scope("recent")"#);
        assert_eq!(copy.apply(Select::<Tag>::new()).limit_value(), Some(1));
    }
}
