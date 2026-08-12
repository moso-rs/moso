//! [`Update<E>`] — and the guard that stops an unfiltered mass update.
//!
//! An `UPDATE` with no `WHERE` has cost real companies real data. `update_all()`
//! therefore **refuses** to run without a filter unless the caller writes
//! `.all_rows()`, which is deliberately conspicuous and easy to grep for.
//!
//! # What the statement carries that the caller did not write
//!
//! | Entity declares | The statement gets |
//! | --- | --- |
//! | `#[entity(updated_at)]` | `SET updated_at = current_timestamp` |
//! | `#[entity(versioned = "version")]` | `SET version = version + 1`, and `AND version = $n` when the row said which version it read |
//! | `#[entity(soft_delete = "deleted_at")]` | `AND deleted_at IS NULL`, unless [`Update::with_deleted`] |
//! | `#[entity(tenant = "tenant_id")]` | `AND tenant_id = $n`, or a refusal |
//!
//! Each of those is a promise the attribute made. An update that silently left
//! `updated_at` behind, or that rewrote another tenant's row, would be the
//! attribute lying.

use core::marker::PhantomData;

use moso_sql::{Assignment, Expr, Statement, Value};

use crate::column::{Column, ColumnValue};
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

/// An `UPDATE` of `E`'s table.
///
/// ```
/// # use moso_orm::{Column, Entity, Update};
/// fn rename<E: Entity>(name: Column<E, String>) -> Update<E> {
///     Update::all().set(name, "New name").all_rows()
/// }
/// ```
pub struct Update<E> {
    assignments: Vec<Assignment>,
    filters: Vec<Filter>,
    all_rows: bool,
    returning: bool,
    /// The version the row had when it was read, for optimistic locking.
    version: Option<Value>,
    /// The tenant whose rows may be rewritten.
    tenant: Option<TenantId>,
    /// Whether every tenant's rows may be rewritten, said on purpose.
    across_tenants: bool,
    /// Whether soft-deleted rows are in range — which is what a restore needs.
    with_deleted: bool,
    at: CallSite,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity> Update<E> {
    /// A bulk update. Refuses to run until it has a filter or `.all_rows()`.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn bulk<E: Entity>() -> Update<E> {
    ///     Update::all()
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn all() -> Self {
        Self {
            assignments: Vec::new(),
            filters: Vec::new(),
            all_rows: false,
            returning: false,
            version: None,
            tenant: None,
            across_tenants: false,
            with_deleted: false,
            at: CallSite::caller(),
            entity: PhantomData,
        }
    }

    /// An update scoped to one row's primary key.
    ///
    /// This is what `entity.update()` calls, and it needs no `.all_rows()`
    /// because it already has a filter.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn one<E: Entity>(key: E::Pk) -> Update<E> {
    ///     Update::by_key(key)
    /// }
    /// ```
    #[must_use]
    #[track_caller]
    pub fn by_key(key: E::Pk) -> Self {
        let update = Self::all();
        let Some(primary) = E::COLUMNS.iter().find(|column| column.is_primary_key()) else {
            return update;
        };
        let column: Column<E, E::Pk> = Column::new(primary.name());
        update.filter(Predicate::of(
            [E::NAME],
            column.expr().eq(Expr::bound(key.into_value())),
        ))
    }

    /// Continues an update from a query's filters.
    ///
    /// Crate-internal: [`Select::update`](crate::Select::update) is the public
    /// door.
    #[must_use]
    #[track_caller]
    pub(crate) fn from_filters(filters: Vec<Filter>) -> Self {
        Self {
            filters,
            ..Self::all()
        }
    }

    /// Sets a column to a value.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn deactivate<E: Entity>(active: Column<E, bool>) -> Update<E> {
    ///     Update::all().set(active, false).all_rows()
    /// }
    /// ```
    #[must_use]
    pub fn set<T: SqlType>(mut self, column: Column<E, T>, value: impl ColumnValue<T>) -> Self {
        self.assignments.push(Assignment::new(
            column.ident(),
            Expr::bound(value.into_column_value()),
        ));
        self
    }

    /// Sets a column only when there is a value — the `PATCH` idiom.
    ///
    /// A partial update whose body left a field out should leave the column
    /// alone, and writing that as an `if let` around a rebinding of the builder
    /// is exactly the boilerplate non-negotiable N4 exists to remove.
    ///
    /// Note that `Option<T>` columns need [`Update::set_null`] to be *cleared*:
    /// `None` here means "the request did not mention this field", which is a
    /// different thing from "the request asked for null".
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn patch<E: Entity>(name: Column<E, String>, new_name: Option<String>) -> Update<E> {
    ///     Update::all().set_opt(name, new_name).all_rows()
    /// }
    /// ```
    #[must_use]
    pub fn set_opt<T: SqlType>(
        self,
        column: Column<E, T>,
        value: Option<impl ColumnValue<T>>,
    ) -> Self {
        match value {
            Some(value) => self.set(column, value),
            None => self,
        }
    }

    /// Sets a column from its own current value, atomically.
    ///
    /// `set_with(User::LOGIN_COUNT, |count| count + Expr::value(1))` becomes
    /// `login_count = login_count + $1`, which is a single statement and cannot
    /// lose a concurrent increment. Reading, adding one and writing back can.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// # use moso_sql::Expr;
    /// fn bump<E: Entity>(count: Column<E, i64>) -> Update<E> {
    ///     Update::all().set_with(count, |current| current + Expr::value(1_i64)).all_rows()
    /// }
    /// ```
    #[must_use]
    pub fn set_with<T: SqlType>(
        mut self,
        column: Column<E, T>,
        value: impl FnOnce(Expr) -> Expr,
    ) -> Self {
        let current = Expr::col(column.ident());
        self.assignments
            .push(Assignment::new(column.ident(), value(current)));
        self
    }

    /// Sets a column to `NULL`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn clear<E: Entity>(bio: Column<E, Option<String>>) -> Update<E> {
    ///     Update::all().set_null(bio).all_rows()
    /// }
    /// ```
    #[must_use]
    pub fn set_null<T: crate::sqltype::Nullable>(mut self, column: Column<E, T>) -> Self {
        self.assignments
            .push(Assignment::new(column.ident(), Expr::null()));
        self
    }

    /// Adds a filter. Repeated calls are `AND`ed.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn stale<E: Entity>(seen: Column<E, i64>, active: Column<E, bool>) -> Update<E> {
    ///     Update::all().set(active, false).filter(seen.lt(0))
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
    /// # use moso_orm::{Column, Entity, Predicate, Update};
    /// fn maybe<E: Entity>(update: Update<E>, p: Option<Predicate>) -> Update<E> {
    ///     update.filter_opt(p)
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

    /// Says, on purpose, that every row is to be rewritten.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn migrate<E: Entity>(flag: Column<E, bool>) -> Update<E> {
    ///     Update::all().set(flag, true).all_rows()
    /// }
    /// ```
    #[must_use]
    pub const fn all_rows(mut self) -> Self {
        self.all_rows = true;
        self
    }

    /// The version the row carried when it was read.
    ///
    /// This is optimistic locking: the statement matches only while the version
    /// column still holds this value, and bumps it as it writes. A write that
    /// matches nothing is [`Error::StaleWrite`] — a 409 — rather than a silent
    /// no-op, which is the failure mode that loses somebody's edit.
    ///
    /// `#[derive(Entity)]` emits this on `entity.update()` when the entity
    /// declares `#[entity(versioned = "…")]`; a bulk update does not have a
    /// version to expect and does not get the predicate.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn guarded<E: Entity>(key: E::Pk, version: i32) -> Update<E> {
    ///     Update::by_key(key).expecting_version(version)
    /// }
    /// ```
    #[must_use]
    pub fn expecting_version(mut self, version: impl SqlType) -> Self {
        self.version = Some(version.into_value());
        self
    }

    /// Restricts the update to one tenant's rows.
    ///
    /// ```
    /// # use moso_orm::{Entity, TenantId, Update};
    /// fn for_tenant<E: Entity>(update: Update<E>, tenant: TenantId) -> Update<E> {
    ///     update.scoped(tenant)
    /// }
    /// ```
    #[must_use]
    pub fn scoped(mut self, tenant: TenantId) -> Self {
        self.tenant = Some(tenant);
        self
    }

    /// Rewrites every tenant's rows, on purpose.
    ///
    /// Deliberately long to type and easy to grep for, exactly as
    /// [`Select::across_tenants`](crate::Select::across_tenants) is.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn migration<E: Entity>(update: Update<E>) -> Update<E> {
    ///     update.across_tenants()
    /// }
    /// ```
    #[must_use]
    pub const fn across_tenants(mut self) -> Self {
        self.across_tenants = true;
        self
    }

    /// Includes soft-deleted rows, which is what restoring one needs.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn restore<E: Entity>(key: E::Pk, deleted_at: Column<E, Option<i64>>) -> Update<E> {
    ///     Update::by_key(key).with_deleted().set_null(deleted_at)
    /// }
    /// ```
    #[must_use]
    pub const fn with_deleted(mut self) -> Self {
        self.with_deleted = true;
        self
    }

    /// Asks the statement to return the rewritten rows.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// fn returning<E: Entity>(flag: Column<E, bool>) -> Update<E> {
    ///     Update::all().set(flag, true).all_rows().returning_entity()
    /// }
    /// ```
    #[must_use]
    pub const fn returning_entity(mut self) -> Self {
        self.returning = true;
        self
    }

    /// The assignments, in the order they were added.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn how_many<E: Entity>(update: &Update<E>) -> usize {
    ///     update.assignments().len()
    /// }
    /// ```
    #[must_use]
    pub fn assignments(&self) -> &[Assignment] {
        &self.assignments
    }

    /// The filters.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn how_many<E: Entity>(update: &Update<E>) -> usize {
    ///     update.filters().len()
    /// }
    /// ```
    #[must_use]
    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    /// Whether the statement is allowed to touch every row.
    ///
    /// ```
    /// # use moso_orm::{Entity, Update};
    /// fn guarded<E: Entity>(update: &Update<E>) -> bool {
    ///     !update.touches_every_row()
    /// }
    /// ```
    #[must_use]
    pub const fn touches_every_row(&self) -> bool {
        self.all_rows
    }

    /// Refuses an unfiltered mass update.
    ///
    /// # Errors
    ///
    /// [`Error::UnfilteredWrite`] when there is no filter and no
    /// `.all_rows()`.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Update};
    /// # fn example<E: Entity>(flag: Column<E, bool>) {
    /// assert!(Update::<E>::all().set(flag, true).check_guard().is_err());
    /// assert!(Update::<E>::all().set(flag, true).all_rows().check_guard().is_ok());
    /// # }
    /// ```
    pub fn check_guard(&self) -> Result<()> {
        if self.all_rows || !self.filters.is_empty() {
            return Ok(());
        }
        Err(Error::UnfilteredWrite {
            operation: "UPDATE",
            table: E::NAME,
        })
    }

    /// Renders the statement.
    ///
    /// # Errors
    ///
    /// [`Error::UnfilteredWrite`] from [`Update::check_guard`],
    /// [`Error::Unjoined`] when a filter mentions another entity — an
    /// `UPDATE` has no joins, so any other entity is out of scope —
    /// [`Error::TenantMissing`] when a tenant-scoped entity named no tenant,
    /// and [`Error::Build`] when there is nothing to set.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Result, Update};
    /// # use moso_sql::Statement;
    /// fn statement<E: Entity>(update: &Update<E>) -> Result<Statement> {
    ///     update.to_statement()
    /// }
    /// ```
    pub fn to_statement(&self) -> Result<Statement> {
        self.check_guard()?;
        self.check_scope()?;

        if self.assignments.is_empty() {
            return Err(Error::Build(moso_sql::Error::incomplete(
                "UPDATE",
                "anything to set",
                "call `.set(Column, value)`, `.set_with(Column, |current| …)` or `.set_null(..)`",
            )));
        }

        let mut update = moso_sql::Update::table(E::TABLE);
        for assignment in &self.assignments {
            update = update.set_assignment(assignment.clone());
        }

        // The managed columns the attributes promised, unless the caller wrote
        // them itself.
        if let Some(updated_at) = E::descriptor().updated_at()
            && !self.assigns(updated_at)
        {
            update = update.set(updated_at.clone(), current_timestamp());
        }
        if let Some(version) = version_column::<E>()
            && !self.assigns(&version)
        {
            update = update.set_with(version, |current| current.plus(Expr::value(1_i32)));
        }

        for filter in &self.filters {
            update = update.filter(filter.predicate().expr().clone());
        }
        if let Some(expected) = self.version.clone()
            && let Some(version) = version_column::<E>()
        {
            update = update.filter(Expr::col(version).eq(Expr::bound(expected)));
        }
        update = update.filter_opt(self.tenant_predicate()?);
        if !self.with_deleted
            && let Some(deleted_at) = soft_delete_column::<E>()
        {
            update = update.filter(Expr::col(deleted_at).is_null());
        }

        if self.returning {
            update = update.returning(returning_entity::<E>());
        }
        Ok(update.into_statement())
    }

    /// Checks that no filter mentions an entity this statement cannot reach.
    ///
    /// # Errors
    ///
    /// [`Error::Unjoined`] naming the entity and the offending line.
    ///
    /// ```
    /// # use moso_orm::{Entity, Result, Update};
    /// fn ok<E: Entity>(update: &Update<E>) -> Result<()> {
    ///     update.check_scope()
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

    /// Runs the update and returns the number of rows it rewrote.
    ///
    /// # Errors
    ///
    /// [`Error::StaleWrite`] when a version was expected and no row still had
    /// it, plus anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Update};
    /// async fn run<E: Entity>(update: Update<E>, ex: impl Executor<'_>) -> Result<u64> {
    ///     update.execute(ex).await
    /// }
    /// ```
    pub async fn execute(self, executor: impl Executor<'_>) -> Result<u64> {
        if self.assignments.is_empty() {
            return Ok(0);
        }
        let expected_version = self.version.is_some();
        let statement = self.to_statement()?;
        let rewritten = executor
            .handle()
            .execute(&statement)
            .await
            .map_err(write_error::<E>)?;
        if expected_version && rewritten == 0 {
            return Err(Error::StaleWrite { entity: E::NAME });
        }
        Ok(rewritten)
    }

    /// Runs the update and returns the single row it rewrote.
    ///
    /// # Errors
    ///
    /// [`Error::StaleWrite`] when an optimistic-locking version did not match,
    /// [`Error::NotFound`] when nothing matched.
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Update};
    /// async fn run<E: Entity>(update: Update<E>, ex: impl Executor<'_>) -> Result<E> {
    ///     update.fetch_one(ex).await
    /// }
    /// ```
    pub async fn fetch_one(self, executor: impl Executor<'_>) -> Result<E> {
        let expected_version = self.version.is_some();
        let statement = self.returning_entity().to_statement()?;
        let Some(row) = executor
            .handle()
            .fetch_optional(&statement)
            .await
            .map_err(write_error::<E>)?
        else {
            return Err(if expected_version {
                Error::StaleWrite { entity: E::NAME }
            } else {
                Error::NotFound { entity: E::NAME }
            });
        };
        Ok(E::from_row(&row)?)
    }

    /// Runs the update and returns every row it rewrote.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Result, Update};
    /// async fn run<E: Entity>(update: Update<E>, ex: impl Executor<'_>) -> Result<Vec<E>> {
    ///     update.fetch_all(ex).await
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

    /// Whether the caller already wrote this column itself.
    fn assigns(&self, column: &moso_sql::Ident) -> bool {
        self.assignments
            .iter()
            .any(|assignment| assignment.column() == column)
    }

    /// The tenant predicate, or a refusal when the entity needs one and no
    /// tenant was named.
    fn tenant_predicate(&self) -> Result<Option<Expr>> {
        let Some(column) = tenant_column::<E>() else {
            return Ok(None);
        };
        match (&self.tenant, self.across_tenants) {
            (Some(tenant), _) => Ok(Some(
                Expr::col(column).eq(Expr::bound(tenant.value().clone())),
            )),
            (None, true) => Ok(None),
            (None, false) => Err(Error::TenantMissing { entity: E::NAME }),
        }
    }
}

impl<E: Entity> core::fmt::Debug for Update<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Update")
            .field("entity", &E::NAME)
            .field("assignments", &self.assignments.len())
            .field("filters", &self.filters.len())
            .field("all_rows", &self.all_rows)
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

    /// A user, reduced to what an update test needs.
    #[derive(Clone, Debug)]
    struct User {
        id: i64,
    }

    impl Entity for User {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("users");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("is_active", ValueKind::Bool),
            ColumnDef::new("login_count", ValueKind::I64),
        ];
        const NAME: &'static str = "User";

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
                EntityDescriptor::builder("User", Self::TABLE)
                    .timestamps("created_at", "updated_at")
                    .soft_delete("deleted_at")
                    .build()
            })
        }
    }

    /// An order, with the version column that makes a stale write loud.
    #[derive(Clone, Debug)]
    struct Order {
        id: i64,
    }

    impl Entity for Order {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("orders");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("status", ValueKind::Text),
            ColumnDef::new("version", ValueKind::I32),
            ColumnDef::new("tenant_id", ValueKind::I64),
        ];
        const NAME: &'static str = "Order";

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
                EntityDescriptor::builder("Order", Self::TABLE)
                    .versioned("version")
                    .tenant("tenant_id")
                    .build()
            })
        }
    }

    const ID: Column<User, i64> = Column::new("id");
    const IS_ACTIVE: Column<User, bool> = Column::new("is_active");
    const LOGIN_COUNT: Column<User, i64> = Column::new("login_count");
    const STATUS: Column<Order, String> = Column::new("status");

    fn as_update(statement: &Statement) -> &moso_sql::Update {
        match statement {
            Statement::Update(update) => update,
            other => panic!("expected an UPDATE, got {other:?}"),
        }
    }

    fn assigns(statement: &Statement, column: &str) -> bool {
        as_update(statement)
            .assignments()
            .iter()
            .any(|assignment| assignment.column().as_str() == column)
    }

    #[test]
    fn an_unfiltered_mass_update_is_refused() {
        let dangerous = Update::<User>::all().set(IS_ACTIVE, false);
        let error = dangerous
            .check_guard()
            .expect_err("no filter, no `all_rows`");
        let text = error.to_string();
        assert!(text.contains("every row"), "{text}");
        assert!(text.contains("help:"), "{text}");
        assert!(error.is_programmer_error());

        // …and the refusal survives all the way to the statement.
        let error = Update::<User>::all()
            .set(IS_ACTIVE, false)
            .to_statement()
            .expect_err("the guard runs before anything is rendered");
        assert!(matches!(error, Error::UnfilteredWrite { .. }));
    }

    #[test]
    fn a_filter_discharges_the_guard() {
        let safe = Update::<User>::all().set(IS_ACTIVE, false).filter(ID.gt(0));
        assert!(safe.check_guard().is_ok());
        assert!(safe.to_statement().is_ok());
    }

    #[test]
    fn all_rows_discharges_the_guard_conspicuously() {
        let deliberate = Update::<User>::all().set(IS_ACTIVE, false).all_rows();
        assert!(deliberate.check_guard().is_ok());
        assert!(deliberate.touches_every_row());
        let statement = deliberate.to_statement().expect("a deliberate mass update");
        // One filter, and it is the soft-delete one — not the caller's.
        assert_eq!(as_update(&statement).filters().len(), 1);
    }

    #[test]
    fn an_update_by_key_needs_no_guard() {
        let one = Update::<User>::by_key(7);
        assert_eq!(one.filters().len(), 1);
        assert!(one.check_guard().is_ok());
    }

    #[test]
    fn set_with_reads_the_column_rather_than_a_value() {
        let bumped =
            Update::<User>::by_key(1).set_with(LOGIN_COUNT, |current| current + Expr::value(1_i64));
        assert_eq!(bumped.assignments().len(), 1);
        // The assignment's value mentions the column, which is what makes the
        // increment atomic.
        assert_ne!(bumped.assignments()[0].value(), &Expr::value(1_i64));

        let statement = bumped.to_statement().expect("a valid update");
        let assignment = &as_update(&statement).assignments()[0];
        assert_eq!(assignment.column().as_str(), "login_count");
        assert!(
            matches!(assignment.value(), Expr::Binary { .. }),
            "`login_count = login_count + $1`, not `login_count = $1`"
        );
    }

    #[test]
    fn set_opt_skips_the_field_the_request_left_out() {
        let untouched = Update::<User>::by_key(1).set_opt(IS_ACTIVE, None::<bool>);
        assert!(untouched.assignments().is_empty());
        let touched = Update::<User>::by_key(1).set_opt(IS_ACTIVE, Some(false));
        assert_eq!(touched.assignments().len(), 1);
    }

    #[test]
    fn an_update_with_nothing_to_set_is_refused_rather_than_rendered() {
        let error = Update::<User>::by_key(1)
            .to_statement()
            .expect_err("`UPDATE users SET` is not a statement");
        assert!(error.to_string().contains("help:"), "{error}");
    }

    #[test]
    fn a_filter_on_another_entity_is_out_of_scope() {
        let stranger = Predicate::of(["Post"], Expr::value(true));
        let update = Update::<User>::all().set(IS_ACTIVE, false).filter(stranger);
        let error = update.check_scope().expect_err("`Post` is not reachable");
        assert!(error.to_string().contains("`Post` is not joined"));
    }

    #[test]
    fn a_managed_timestamp_is_bumped_and_a_written_one_is_left_alone() {
        let statement = Update::<User>::by_key(1)
            .set(IS_ACTIVE, false)
            .to_statement()
            .expect("a valid update");
        assert!(
            assigns(&statement, "updated_at"),
            "`#[entity(updated_at)]` promises the column is managed"
        );

        const UPDATED_AT: Column<User, i64> = Column::new("updated_at");
        let statement = Update::<User>::by_key(1)
            .set(UPDATED_AT, 0_i64)
            .to_statement()
            .expect("a valid update");
        assert_eq!(
            as_update(&statement).assignments().len(),
            1,
            "a caller that writes the column itself is not overridden"
        );
    }

    #[test]
    fn a_soft_deleted_row_is_out_of_range_unless_the_update_says_otherwise() {
        let ordinary = Update::<User>::by_key(1)
            .set(IS_ACTIVE, false)
            .to_statement()
            .expect("a valid update");
        assert_eq!(
            as_update(&ordinary).filters().len(),
            2,
            "the key, and `deleted_at IS NULL`"
        );

        let restoring = Update::<User>::by_key(1)
            .with_deleted()
            .set(IS_ACTIVE, true)
            .to_statement()
            .expect("a valid update");
        assert_eq!(
            as_update(&restoring).filters().len(),
            1,
            "restoring a row has to be able to see it"
        );
    }

    #[test]
    fn a_versioned_entity_bumps_its_version_and_matches_the_one_it_read() {
        let blind = Update::<Order>::by_key(1)
            .set(STATUS, "paid")
            .across_tenants()
            .to_statement()
            .expect("a valid update");
        assert!(
            assigns(&blind, "version"),
            "every write moves the version, or nobody else can detect staleness"
        );
        assert_eq!(as_update(&blind).filters().len(), 1, "the key only");

        let guarded = Update::<Order>::by_key(1)
            .set(STATUS, "paid")
            .expecting_version(3_i32)
            .across_tenants()
            .to_statement()
            .expect("a valid update");
        assert_eq!(
            as_update(&guarded).filters().len(),
            2,
            "the key, and `version = 3`"
        );
    }

    #[test]
    fn a_tenant_scoped_update_refuses_to_run_unscoped() {
        let unscoped = Update::<Order>::by_key(1).set(STATUS, "paid");
        let error = unscoped.to_statement().expect_err("no tenant");
        assert!(matches!(error, Error::TenantMissing { entity: "Order" }));
        assert!(error.to_string().contains("help:"), "{error}");

        let scoped = Update::<Order>::by_key(1)
            .set(STATUS, "paid")
            .scoped(TenantId::of(7_i64))
            .to_statement()
            .expect("a tenant was named");
        assert_eq!(
            as_update(&scoped).filters().len(),
            2,
            "the key, and `tenant_id = 7`"
        );

        let everyone = Update::<Order>::by_key(1)
            .set(STATUS, "paid")
            .across_tenants()
            .to_statement()
            .expect("said on purpose");
        assert_eq!(as_update(&everyone).filters().len(), 1);
    }

    #[test]
    fn returning_lists_the_columns_the_decoder_expects() {
        let statement = Update::<User>::by_key(1)
            .set(IS_ACTIVE, false)
            .returning_entity()
            .to_statement()
            .expect("a valid update");
        let Returning::Items(items) = as_update(&statement).returning_clause() else {
            panic!("expected an explicit RETURNING list, never `*`");
        };
        assert_eq!(items.len(), User::COLUMNS.len());
    }

    #[test]
    fn the_update_targets_the_entitys_table() {
        let statement = Update::<User>::by_key(1)
            .set(IS_ACTIVE, false)
            .to_statement()
            .expect("a valid update");
        assert_eq!(as_update(&statement).target().name().as_str(), "users");
        assert!(as_update(&statement).has_filter());
    }
}
