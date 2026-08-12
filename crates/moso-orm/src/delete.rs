//! [`Delete<E>`] — soft by default when the entity says so, and guarded against
//! an unfiltered mass delete for the same reason [`Update`](crate::Update) is.
//!
//! # Soft, hard, and the difference that matters
//!
//! An entity with `#[entity(soft_delete = "deleted_at")]` is deleted by writing
//! the timestamp, so the row survives, disappears from every query, and can be
//! restored. `.hard()` removes it, and deliberately does **not** add the
//! `deleted_at IS NULL` filter — purging rows that were already soft-deleted is
//! the main reason to reach for it.

use core::marker::PhantomData;

use moso_sql::{Expr, Statement, Value};

use crate::column::Column;
use crate::db::TenantId;
use crate::entity::Entity;
use crate::error::{CallSite, Error, Result};
use crate::executor::Executor;
use crate::insert::upsert::{
    current_timestamp, returning_entity, soft_delete_column, tenant_column, version_column,
    write_error,
};
use crate::predicate::Predicate;
use crate::select::Filter;
use crate::sqltype::SqlType;

/// A `DELETE` from `E`'s table.
///
/// When the entity declares a soft-delete column, this writes a timestamp
/// instead of removing the row — unless `.hard()` says otherwise.
///
/// ```
/// # use moso_orm::{Column, Delete, Entity};
/// fn purge<E: Entity>(id: Column<E, i64>) -> Delete<E> {
///     Delete::all().filter(id.lt(0)).hard()
/// }
/// ```
pub struct Delete<E> {
    filters: Vec<Filter>,
    all_rows: bool,
    hard: bool,
    returning: bool,
    /// The version the row had when it was read, for optimistic locking.
    version: Option<Value>,
    /// The tenant whose rows may be deleted.
    tenant: Option<TenantId>,
    /// Whether every tenant's rows may be deleted, said on purpose.
    across_tenants: bool,
    at: CallSite,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity> Delete<E> {
    /// A bulk delete. Refuses to run until it has a filter or `.all_rows()`.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn bulk<E: Entity>() -> Delete<E> {
    ///     Delete::all()
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn all() -> Self {
        Self {
            filters: Vec::new(),
            all_rows: false,
            hard: false,
            returning: false,
            version: None,
            tenant: None,
            across_tenants: false,
            at: CallSite::caller(),
            entity: PhantomData,
        }
    }

    /// A delete scoped to one row's primary key.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn one<E: Entity>(key: E::Pk) -> Delete<E> {
    ///     Delete::by_key(key)
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn by_key(key: E::Pk) -> Self {
        let delete = Self::all();
        let Some(primary) = E::COLUMNS.iter().find(|column| column.is_primary_key()) else {
            return delete;
        };
        let column: Column<E, E::Pk> = Column::new(primary.name());
        delete.filter(Predicate::of(
            [E::NAME],
            column.expr().eq(Expr::bound(key.into_value())),
        ))
    }

    /// Continues a delete from a query's filters.
    #[must_use]
    #[track_caller]
    pub(crate) fn from_filters(filters: Vec<Filter>) -> Self {
        Self {
            filters,
            ..Self::all()
        }
    }

    /// Adds a filter. Repeated calls are `AND`ed.
    ///
    /// ```
    /// # use moso_orm::{Column, Delete, Entity};
    /// fn old<E: Entity>(seen: Column<E, i64>) -> Delete<E> {
    ///     Delete::all().filter(seen.lt(0))
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn filter(mut self, predicate: impl Into<Predicate>) -> Self {
        self.filters.push(Filter::new(predicate.into()));
        self
    }

    /// Adds a filter only when there is one.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity, Predicate};
    /// fn maybe<E: Entity>(delete: Delete<E>, p: Option<Predicate>) -> Delete<E> {
    ///     delete.filter_opt(p)
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn filter_opt(self, predicate: Option<impl Into<Predicate>>) -> Self {
        match predicate {
            Some(predicate) => self.filter(predicate),
            None => self,
        }
    }

    /// Removes the rows even when the entity is soft-deletable.
    ///
    /// Soft-deleted rows are in range: purging what was deleted last month is
    /// what this is for.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn purge<E: Entity>() -> Delete<E> {
    ///     Delete::all().all_rows().hard()
    /// }
    /// ```
    #[must_use]
    pub const fn hard(mut self) -> Self {
        self.hard = true;
        self
    }

    /// Says, on purpose, that every row is to be deleted.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn empty<E: Entity>() -> Delete<E> {
    ///     Delete::all().all_rows()
    /// }
    /// ```
    #[must_use]
    pub const fn all_rows(mut self) -> Self {
        self.all_rows = true;
        self
    }

    /// The version the row carried when it was read.
    ///
    /// Deleting a row somebody else has changed since you read it is the same
    /// mistake as overwriting one, and it fails the same way:
    /// [`Error::StaleWrite`], a 409, rather than a silent zero.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn guarded<E: Entity>(key: E::Pk, version: i32) -> Delete<E> {
    ///     Delete::by_key(key).expecting_version(version)
    /// }
    /// ```
    #[must_use]
    pub fn expecting_version(mut self, version: impl SqlType) -> Self {
        self.version = Some(version.into_value());
        self
    }

    /// Restricts the delete to one tenant's rows.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity, TenantId};
    /// fn for_tenant<E: Entity>(delete: Delete<E>, tenant: TenantId) -> Delete<E> {
    ///     delete.scoped(tenant)
    /// }
    /// ```
    #[must_use]
    pub fn scoped(mut self, tenant: TenantId) -> Self {
        self.tenant = Some(tenant);
        self
    }

    /// Deletes every tenant's rows, on purpose.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn migration<E: Entity>(delete: Delete<E>) -> Delete<E> {
    ///     delete.across_tenants()
    /// }
    /// ```
    #[must_use]
    pub const fn across_tenants(mut self) -> Self {
        self.across_tenants = true;
        self
    }

    /// Asks the statement to return the deleted rows.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn returning<E: Entity>(key: E::Pk) -> Delete<E> {
    ///     Delete::by_key(key).returning_entity()
    /// }
    /// ```
    #[must_use]
    pub const fn returning_entity(mut self) -> Self {
        self.returning = true;
        self
    }

    /// The filters.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn how_many<E: Entity>(delete: &Delete<E>) -> usize {
    ///     delete.filters().len()
    /// }
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    /// Whether the rows are actually removed.
    ///
    /// A soft-deletable entity answers `false` unless `.hard()` was called.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn removes<E: Entity>(delete: &Delete<E>) -> bool {
    ///     delete.is_hard()
    /// }
    /// ```
    #[must_use]
    pub fn is_hard(&self) -> bool {
        self.hard || !E::descriptor().is_soft_deletable()
    }

    /// Whether the statement is allowed to touch every row.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// fn guarded<E: Entity>(delete: &Delete<E>) -> bool {
    ///     !delete.touches_every_row()
    /// }
    /// ```
    #[must_use]
    pub const fn touches_every_row(&self) -> bool {
        self.all_rows
    }

    /// Refuses an unfiltered mass delete.
    ///
    /// # Errors
    ///
    /// [`Error::UnfilteredWrite`] when there is no filter and no
    /// `.all_rows()`.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity};
    /// # fn example<E: Entity>() {
    /// assert!(Delete::<E>::all().check_guard().is_err());
    /// assert!(Delete::<E>::all().all_rows().check_guard().is_ok());
    /// # }
    /// ```
    pub fn check_guard(&self) -> Result<()> {
        if self.all_rows || !self.filters.is_empty() {
            return Ok(());
        }
        Err(Error::UnfilteredWrite {
            operation: "DELETE",
            table: E::NAME,
        })
    }

    /// Renders the statement — a `DELETE`, or the `UPDATE` a soft delete is.
    ///
    /// # Errors
    ///
    /// [`Error::UnfilteredWrite`], [`Error::Unjoined`] when a filter mentions
    /// another entity, and [`Error::TenantMissing`] when a tenant-scoped entity
    /// named no tenant.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity, Result};
    /// # use moso_sql::Statement;
    /// fn statement<E: Entity>(delete: &Delete<E>) -> Result<Statement> {
    ///     delete.to_statement()
    /// }
    /// ```
    pub fn to_statement(&self) -> Result<Statement> {
        self.check_guard()?;
        self.check_scope()?;

        let soft_delete = soft_delete_column::<E>();
        match soft_delete {
            Some(column) if !self.hard => {
                // A soft delete is an UPDATE that hides the row, so it carries
                // the same managed columns an ordinary update would.
                let mut update =
                    moso_sql::Update::table(E::TABLE).set(column.clone(), current_timestamp());
                if let Some(updated_at) = E::descriptor().updated_at() {
                    update = update.set(updated_at.clone(), current_timestamp());
                }
                if let Some(version) = version_column::<E>() {
                    update = update.set_with(version, |current| current.plus(Expr::value(1_i32)));
                }
                for predicate in self.predicates()? {
                    update = update.filter(predicate);
                }
                // Deleting a row twice writes nothing the second time, so the
                // affected count stays the truth.
                update = update.filter(Expr::col(column).is_null());
                if self.returning {
                    update = update.returning(returning_entity::<E>());
                }
                Ok(update.into_statement())
            }
            _ => {
                let mut delete = moso_sql::Delete::from_table(E::TABLE);
                for predicate in self.predicates()? {
                    delete = delete.filter(predicate);
                }
                if self.returning {
                    delete = delete.returning(returning_entity::<E>());
                }
                Ok(delete.into_statement())
            }
        }
    }

    /// Checks that no filter mentions an entity this statement cannot reach.
    ///
    /// # Errors
    ///
    /// [`Error::Unjoined`] naming the entity and the offending line.
    ///
    /// ```
    /// # use moso_orm::{Delete, Entity, Result};
    /// fn ok<E: Entity>(delete: &Delete<E>) -> Result<()> {
    ///     delete.check_scope()
    /// }
    /// ```
    pub fn check_scope(&self) -> Result<()> {
        for filter in &self.filters {
            if let Some(missing) = filter.predicate().missing_from(&[E::NAME]) {
                return Err(Error::Unjoined(Box::new(
                    crate::error::Unjoined::new(E::NAME, E::NAME, missing, missing.to_owned())
                        .at(filter.call_site()),
                )));
            }
        }
        Ok(())
    }

    /// Runs the delete and returns the number of rows it removed.
    ///
    /// # Errors
    ///
    /// [`Error::ForeignKeyViolation`] when another table still references a
    /// row, [`Error::StaleWrite`] when a version was expected and no row still
    /// had it, plus the rest of [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Delete, Entity, Executor, Result};
    /// async fn run<E: Entity>(delete: Delete<E>, ex: impl Executor<'_>) -> Result<u64> {
    ///     delete.execute(ex).await
    /// }
    /// ```
    pub async fn execute(self, executor: impl Executor<'_>) -> Result<u64> {
        let expected_version = self.version.is_some();
        let statement = self.to_statement()?;
        let removed = executor
            .handle()
            .execute(&statement)
            .await
            .map_err(write_error::<E>)?;
        if expected_version && removed == 0 {
            return Err(Error::StaleWrite { entity: E::NAME });
        }
        Ok(removed)
    }

    /// Runs the delete and returns every row it removed.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Delete, Entity, Executor, Result};
    /// async fn run<E: Entity>(delete: Delete<E>, ex: impl Executor<'_>) -> Result<Vec<E>> {
    ///     delete.fetch_all(ex).await
    /// }
    /// ```
    pub async fn fetch_all(self, executor: impl Executor<'_>) -> Result<Vec<E>> {
        let statement = self.returning_entity().to_statement()?;
        let rows = executor
            .handle()
            .fetch_all(&statement)
            .await
            .map_err(write_error::<E>)?;
        let mut entities = Vec::with_capacity(rows.len());
        for row in &rows {
            entities.push(E::from_row(row)?);
        }
        Ok(entities)
    }

    /// Every `WHERE` term: the caller's filters, the expected version, and the
    /// tenant.
    fn predicates(&self) -> Result<Vec<Expr>> {
        let mut predicates: Vec<Expr> = self
            .filters
            .iter()
            .map(|filter| filter.predicate().expr().clone())
            .collect();
        if let Some(expected) = self.version.clone()
            && let Some(version) = version_column::<E>()
        {
            predicates.push(Expr::col(version).eq(Expr::bound(expected)));
        }
        if let Some(column) = tenant_column::<E>() {
            match (&self.tenant, self.across_tenants) {
                (Some(tenant), _) => {
                    predicates.push(Expr::col(column).eq(Expr::bound(tenant.value().clone())));
                }
                (None, true) => {}
                (None, false) => return Err(Error::TenantMissing { entity: E::NAME }),
            }
        }
        Ok(predicates)
    }
}

impl<E: Entity> core::fmt::Debug for Delete<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Delete")
            .field("entity", &E::NAME)
            .field("filters", &self.filters.len())
            .field("all_rows", &self.all_rows)
            .field("hard", &self.hard)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::row::{DecodeError, Row};
    use moso_sql::{Returning, TableRef, ValueKind};
    use std::sync::OnceLock;

    /// A post, with a soft-delete column.
    #[derive(Clone, Debug)]
    struct Post {
        id: i64,
    }

    impl Entity for Post {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("posts");
        const COLUMNS: &'static [ColumnDef] = &[ColumnDef::new("id", ValueKind::I64).primary_key()];
        const NAME: &'static str = "Post";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("Post", Self::TABLE)
                    .soft_delete("deleted_at")
                    .build()
            })
        }
    }

    /// A tag, without one.
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

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| EntityDescriptor::builder("Tag", Self::TABLE).build())
        }
    }

    /// An invoice: tenant-scoped, versioned, and hard-deleted.
    #[derive(Clone, Debug)]
    struct Invoice {
        id: i64,
    }

    impl Entity for Invoice {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("invoices");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("version", ValueKind::I32),
            ColumnDef::new("tenant_id", ValueKind::I64),
        ];
        const NAME: &'static str = "Invoice";

        fn pk(&self) -> i64 {
            self.id
        }

        fn from_row(row: &Row) -> core::result::Result<Self, DecodeError> {
            Ok(Self {
                id: row.get_i64(0)?,
            })
        }

        fn descriptor() -> &'static EntityDescriptor {
            static DESCRIPTOR: OnceLock<EntityDescriptor> = OnceLock::new();
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("Invoice", Self::TABLE)
                    .versioned("version")
                    .tenant("tenant_id")
                    .build()
            })
        }
    }

    fn as_delete(statement: &Statement) -> &moso_sql::Delete {
        match statement {
            Statement::Delete(delete) => delete,
            other => panic!("expected a DELETE, got {other:?}"),
        }
    }

    fn as_update(statement: &Statement) -> &moso_sql::Update {
        match statement {
            Statement::Update(update) => update,
            other => panic!("expected the UPDATE a soft delete is, got {other:?}"),
        }
    }

    #[test]
    fn an_unfiltered_mass_delete_is_refused() {
        let error = Delete::<Post>::all()
            .check_guard()
            .expect_err("no filter, no `all_rows`");
        assert!(error.to_string().contains("every row"));
        assert!(error.is_programmer_error());

        let error = Delete::<Post>::all()
            .to_statement()
            .expect_err("the guard runs before anything is rendered");
        assert!(matches!(error, Error::UnfilteredWrite { .. }));

        assert!(Delete::<Post>::all().all_rows().to_statement().is_ok());
    }

    #[test]
    fn a_soft_deletable_entity_deletes_softly_by_default() {
        assert!(!Delete::<Post>::by_key(1).is_hard());
        assert!(Delete::<Post>::by_key(1).hard().is_hard());

        let soft = Delete::<Post>::by_key(1)
            .to_statement()
            .expect("a valid soft delete");
        let update = as_update(&soft);
        assert_eq!(update.target().name().as_str(), "posts");
        assert_eq!(update.assignments().len(), 1);
        assert_eq!(update.assignments()[0].column().as_str(), "deleted_at");
        assert_eq!(
            update.filters().len(),
            2,
            "the key, and `deleted_at IS NULL` so a second delete writes nothing"
        );
    }

    #[test]
    fn a_hard_delete_removes_the_row_and_can_see_the_deleted_ones() {
        let statement = Delete::<Post>::by_key(1)
            .hard()
            .to_statement()
            .expect("a valid delete");
        let delete = as_delete(&statement);
        assert_eq!(delete.target().name().as_str(), "posts");
        assert_eq!(
            delete.filters().len(),
            1,
            "purging is exactly the case that must reach an already-deleted row"
        );
    }

    #[test]
    fn an_entity_without_a_soft_delete_column_always_deletes_hard() {
        assert!(Delete::<Tag>::by_key(1).is_hard());
        let statement = Delete::<Tag>::by_key(1)
            .to_statement()
            .expect("a valid delete");
        assert!(matches!(statement, Statement::Delete(_)));
    }

    #[test]
    fn a_key_scoped_delete_needs_no_guard() {
        assert!(Delete::<Post>::by_key(1).check_guard().is_ok());
        assert_eq!(Delete::<Post>::by_key(1).filters().len(), 1);
    }

    #[test]
    fn a_filter_on_another_entity_is_out_of_scope() {
        let stranger = Predicate::of(["User"], Expr::value(true));
        let error = Delete::<Post>::all()
            .filter(stranger)
            .check_scope()
            .expect_err("`User` is not reachable from a DELETE");
        assert!(error.to_string().contains("`User` is not joined"));
    }

    #[test]
    fn a_tenant_scoped_delete_refuses_to_run_unscoped() {
        let error = Delete::<Invoice>::by_key(1)
            .to_statement()
            .expect_err("no tenant");
        assert!(matches!(error, Error::TenantMissing { entity: "Invoice" }));

        let scoped = Delete::<Invoice>::by_key(1)
            .scoped(TenantId::of(7_i64))
            .to_statement()
            .expect("a tenant was named");
        assert_eq!(as_delete(&scoped).filters().len(), 2);

        let everyone = Delete::<Invoice>::by_key(1)
            .across_tenants()
            .to_statement()
            .expect("said on purpose");
        assert_eq!(as_delete(&everyone).filters().len(), 1);
    }

    #[test]
    fn a_versioned_delete_matches_the_version_it_read() {
        let statement = Delete::<Invoice>::by_key(1)
            .expecting_version(4_i32)
            .across_tenants()
            .to_statement()
            .expect("a valid delete");
        assert_eq!(
            as_delete(&statement).filters().len(),
            2,
            "the key, and `version = 4`"
        );
    }

    #[test]
    fn returning_lists_the_columns_the_decoder_expects() {
        let hard = Delete::<Tag>::by_key(1)
            .returning_entity()
            .to_statement()
            .expect("a valid delete");
        let Returning::Items(items) = as_delete(&hard).returning_clause() else {
            panic!("expected an explicit RETURNING list, never `*`");
        };
        assert_eq!(items.len(), Tag::COLUMNS.len());

        let soft = Delete::<Post>::by_key(1)
            .returning_entity()
            .to_statement()
            .expect("a valid soft delete");
        assert!(as_update(&soft).returning_clause().is_some());
    }
}
