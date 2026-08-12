//! [`Insert<E>`] — one row, many rows in **one** statement, or an upsert.
//!
//! # One statement, however many rows
//!
//! [`Insert::rows`] builds a single multi-row `INSERT`, not a loop. That is the
//! difference between a bulk load that takes a second and one that takes a
//! minute, and it is why [`Insert::execute`] chunks against the dialect's
//! bind-parameter limit rather than refusing: a 100 000-row insert becomes two
//! statements on PostgreSQL, not 100 000.

use core::marker::PhantomData;

use moso_sql::{Assignment, Expr, Ident, OnConflict, Statement};

use crate::column::{Column, ColumnValue};
use crate::db::TenantId;
use crate::entity::{Entity, NewEntity};
use crate::error::{Error, Result};
use crate::executor::Executor;
use crate::sqltype::SqlType;

/// The upsert clause and the write-path machinery, in a file of their own.
///
/// Declared here rather than in `lib.rs` because this build's file ownership
/// does not include `lib.rs`; see the module's own documentation.
#[path = "upsert.rs"]
pub mod upsert;

pub use upsert::{Conflict, ConflictAction};
use upsert::{current_timestamp, returning_entity, tenant_column, write_error};

/// An `INSERT` into `E`'s table.
///
/// ```
/// # use moso_orm::{Entity, Insert, NewEntity};
/// fn one<E: Entity, N: NewEntity>(row: N) -> Insert<E> {
///     Insert::row(row)
/// }
/// ```
pub struct Insert<E> {
    columns: Vec<Ident>,
    rows: Vec<Vec<Expr>>,
    conflict: Option<Conflict>,
    returning_entity: bool,
    entity: PhantomData<fn() -> E>,
}

impl<E: Entity> Insert<E> {
    /// One row, from the `New…` struct the derive generates.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// fn one<E: Entity, N: NewEntity>(row: N) -> Insert<E> {
    ///     Insert::row(row)
    /// }
    /// ```
    #[must_use]
    pub fn row<N: NewEntity>(row: N) -> Self {
        Self {
            columns: N::idents(),
            rows: vec![row.into_row()],
            conflict: None,
            returning_entity: false,
            entity: PhantomData,
        }
    }

    /// Many rows, in **one** statement.
    ///
    /// An empty iterator produces an insert that runs nothing and reports zero
    /// rows, rather than invalid SQL.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// fn many<E: Entity, N: NewEntity>(rows: Vec<N>) -> Insert<E> {
    ///     Insert::rows(rows)
    /// }
    /// ```
    #[must_use]
    pub fn rows<N: NewEntity>(rows: impl IntoIterator<Item = N>) -> Self {
        Self {
            columns: N::idents(),
            rows: rows.into_iter().map(NewEntity::into_row).collect(),
            conflict: None,
            returning_entity: false,
            entity: PhantomData,
        }
    }

    /// On a conflict on `column`, do nothing.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Insert, NewEntity};
    /// fn idempotent<E: Entity, N: NewEntity>(row: N, key: Column<E, String>) -> Insert<E> {
    ///     Insert::row(row).on_conflict(key).do_nothing()
    /// }
    /// ```
    #[must_use]
    pub fn on_conflict<T: SqlType>(mut self, column: Column<E, T>) -> Self {
        self.conflict = Some(Conflict::new([column.ident()]));
        self
    }

    /// On a conflict on several columns, do nothing.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// # use moso_sql::Ident;
    /// fn idempotent<E: Entity, N: NewEntity>(row: N) -> Insert<E> {
    ///     Insert::row(row).on_conflict_columns([Ident::from_static("a"), Ident::from_static("b")])
    /// }
    /// ```
    #[must_use]
    pub fn on_conflict_columns(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        self.conflict = Some(Conflict::new(columns));
        self
    }

    /// Keeps the existing row on a conflict.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Insert, NewEntity};
    /// fn keep<E: Entity, N: NewEntity>(row: N, key: Column<E, String>) -> Insert<E> {
    ///     Insert::row(row).on_conflict(key).do_nothing()
    /// }
    /// ```
    #[must_use]
    pub fn do_nothing(mut self) -> Self {
        if let Some(conflict) = self.conflict.as_mut() {
            conflict.set_action(ConflictAction::Nothing);
        }
        self
    }

    /// Overwrites these columns from the row being inserted.
    ///
    /// The upsert: `ON CONFLICT (email) DO UPDATE SET name = excluded.name`.
    ///
    /// An entity with an `updated_at` column gets it bumped as well — the
    /// attribute's promise is that the column is managed, and an upsert that
    /// rewrote a row while leaving its timestamp behind would break every
    /// "changed since" query.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Insert, NewEntity};
    /// # use moso_sql::Ident;
    /// fn upsert<E: Entity, N: NewEntity>(row: N, key: Column<E, String>) -> Insert<E> {
    ///     Insert::row(row).on_conflict(key).do_update([Ident::from_static("name")])
    /// }
    /// ```
    #[must_use]
    pub fn do_update(mut self, columns: impl IntoIterator<Item = Ident>) -> Self {
        if let Some(conflict) = self.conflict.as_mut() {
            conflict.set_action(ConflictAction::Update(columns.into_iter().collect()));
        }
        self
    }

    /// Sets one column to an explicit value, overriding what the row supplies.
    ///
    /// For the framework-managed columns — a tenant, a timestamp — and for the
    /// application that wants to force one.
    ///
    /// ```
    /// # use moso_orm::{Column, Entity, Insert, NewEntity};
    /// fn forced<E: Entity, N: NewEntity>(row: N, flag: Column<E, bool>) -> Insert<E> {
    ///     Insert::row(row).set(flag, true)
    /// }
    /// ```
    #[must_use]
    pub fn set<T: SqlType>(mut self, column: Column<E, T>, value: impl ColumnValue<T>) -> Self {
        self.assign(column.ident(), Expr::bound(value.into_column_value()));
        self
    }

    /// Fills the tenant column from `tenant`.
    ///
    /// A tenant-scoped entity refuses to be inserted without one: the column is
    /// `NOT NULL` in the generated migration, and a row written under the wrong
    /// tenant is the failure this whole mechanism exists to prevent. Calling
    /// this is the alternative to putting the tenant in the `New…` struct.
    ///
    /// On an entity that is not tenant-scoped it does nothing, so a generic
    /// helper can call it without asking first.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity, TenantId};
    /// fn for_tenant<E: Entity, N: NewEntity>(row: N, tenant: TenantId) -> Insert<E> {
    ///     Insert::row(row).scoped(tenant)
    /// }
    /// ```
    #[must_use]
    pub fn scoped(mut self, tenant: TenantId) -> Self {
        if let Some(column) = tenant_column::<E>() {
            self.assign(column, Expr::bound(tenant.into_value()));
        }
        self
    }

    /// Asks the statement to return the inserted rows, so that
    /// database-generated values — the key, the timestamps — come back.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// fn returning<E: Entity, N: NewEntity>(row: N) -> Insert<E> {
    ///     Insert::row(row).returning_entity()
    /// }
    /// ```
    #[must_use]
    pub const fn returning_entity(mut self) -> Self {
        self.returning_entity = true;
        self
    }

    /// The columns being written.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// fn width<E: Entity, N: NewEntity>(row: N) -> usize {
    ///     Insert::<E>::row(row).columns().len()
    /// }
    /// ```
    #[must_use]
    pub fn columns(&self) -> &[Ident] {
        &self.columns
    }

    /// How many rows the statement writes.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// fn count<E: Entity, N: NewEntity>(rows: Vec<N>) -> usize {
    ///     Insert::<E>::rows(rows).row_count()
    /// }
    /// ```
    #[must_use]
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// The conflict clause, when there is one.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity};
    /// fn upserts<E: Entity, N: NewEntity>(row: N) -> bool {
    ///     Insert::<E>::row(row).conflict().is_some()
    /// }
    /// ```
    #[must_use]
    pub const fn conflict(&self) -> Option<&Conflict> {
        self.conflict.as_ref()
    }

    /// Renders the statement.
    ///
    /// One statement, however many rows — see [`Insert::statements`] for the
    /// chunked form [`Insert::execute`] uses when the row count would exceed
    /// the dialect's parameter limit.
    ///
    /// # Errors
    ///
    /// [`Error::Build`] when there is nothing to insert, when a row does not
    /// line up with the column list, or when the conflict clause cannot be
    /// expressed; [`Error::TenantMissing`] when a tenant-scoped entity has no
    /// tenant column value.
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity, Result};
    /// # use moso_sql::Statement;
    /// fn statement<E: Entity, N: NewEntity>(row: N) -> Result<Statement> {
    ///     Insert::<E>::row(row).to_statement()
    /// }
    /// ```
    pub fn to_statement(&self) -> Result<Statement> {
        let mut built = self.statements(usize::MAX)?;
        built.pop().ok_or_else(|| Self::nothing_to_insert())
    }

    /// The statement, or the chunks of it that fit under `max_bind_params`.
    ///
    /// A multi-row insert binds one parameter per value, and every server has a
    /// ceiling — 65 535 on PostgreSQL's wire protocol. Splitting is the only
    /// honest answer: refusing would make the bulk path useless at exactly the
    /// size it exists for.
    ///
    /// **The chunks are separate statements.** Inside a transaction they are
    /// still all-or-nothing; outside one, a failure part-way leaves the earlier
    /// chunks written. Wrap the call in `db.transaction(..)` when that matters.
    ///
    /// # Errors
    ///
    /// As [`Insert::to_statement`].
    ///
    /// ```
    /// # use moso_orm::{Entity, Insert, NewEntity, Result};
    /// # use moso_sql::Statement;
    /// fn chunked<E: Entity, N: NewEntity>(rows: Vec<N>) -> Result<Vec<Statement>> {
    ///     Insert::<E>::rows(rows).statements(65_535)
    /// }
    /// ```
    pub fn statements(&self, max_bind_params: usize) -> Result<Vec<Statement>> {
        if self.rows.is_empty() {
            return Err(Self::nothing_to_insert());
        }
        self.check_tenant()?;
        self.check_arity()?;

        // A `New…` struct with no columns at all inserts whatever the table
        // defaults to; `INSERT INTO t () VALUES ()` is not valid SQL anywhere.
        if self.columns.is_empty() {
            return self
                .rows
                .iter()
                .map(|_| self.finish(moso_sql::Insert::into_table(E::TABLE).default_values()))
                .collect();
        }

        let per_chunk = (max_bind_params / self.columns.len()).max(1);
        self.rows
            .chunks(per_chunk)
            .map(|chunk| {
                self.finish(
                    moso_sql::Insert::into_table(E::TABLE)
                        .columns(self.columns.iter().cloned())
                        .rows(chunk.iter().cloned()),
                )
            })
            .collect()
    }

    /// Runs the insert for its effect and returns the number of rows written.
    ///
    /// # Errors
    ///
    /// [`Error::UniqueViolation`] and the rest of [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Insert, NewEntity, Result};
    /// async fn run<E: Entity, N: NewEntity>(row: N, ex: impl Executor<'_>) -> Result<u64> {
    ///     Insert::<E>::row(row).execute(ex).await
    /// }
    /// ```
    pub async fn execute(self, executor: impl Executor<'_>) -> Result<u64> {
        if self.rows.is_empty() {
            return Ok(0);
        }
        let handle = executor.handle();
        let statements = self.statements(handle.dialect().max_bind_params())?;
        let mut written = 0;
        for statement in &statements {
            written += handle.execute(statement).await.map_err(write_error::<E>)?;
        }
        Ok(written)
    }

    /// Runs the insert and returns the row it wrote.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] when a `DO NOTHING` conflict wrote nothing, plus the
    /// rest of [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Insert, NewEntity, Result};
    /// async fn run<E: Entity, N: NewEntity>(row: N, ex: impl Executor<'_>) -> Result<E> {
    ///     Insert::<E>::row(row).returning_entity().fetch_one(ex).await
    /// }
    /// ```
    pub async fn fetch_one(self, executor: impl Executor<'_>) -> Result<E> {
        self.fetch_optional(executor)
            .await?
            .ok_or(Error::NotFound { entity: E::NAME })
    }

    /// Runs the insert and returns the row it wrote, if it wrote one.
    ///
    /// `None` is the honest answer to `ON CONFLICT DO NOTHING` when the row was
    /// already there.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Insert, NewEntity, Result};
    /// async fn run<E: Entity, N: NewEntity>(r: N, ex: impl Executor<'_>) -> Result<Option<E>> {
    ///     Insert::<E>::row(r).returning_entity().fetch_optional(ex).await
    /// }
    /// ```
    pub async fn fetch_optional(self, executor: impl Executor<'_>) -> Result<Option<E>> {
        if self.rows.is_empty() {
            return Ok(None);
        }
        let statement = self.returning_entity().to_statement()?;
        let Some(row) = executor
            .handle()
            .fetch_optional(&statement)
            .await
            .map_err(write_error::<E>)?
        else {
            return Ok(None);
        };
        Ok(Some(E::from_row(&row)?))
    }

    /// Runs the insert and returns every row it wrote.
    ///
    /// # Errors
    ///
    /// Anything in [`Error`].
    ///
    /// ```no_run
    /// # use moso_orm::{Entity, Executor, Insert, NewEntity, Result};
    /// async fn run<E: Entity, N: NewEntity>(r: Vec<N>, ex: impl Executor<'_>) -> Result<Vec<E>> {
    ///     Insert::<E>::rows(r).returning_entity().fetch_all(ex).await
    /// }
    /// ```
    pub async fn fetch_all(self, executor: impl Executor<'_>) -> Result<Vec<E>> {
        if self.rows.is_empty() {
            return Ok(Vec::new());
        }
        let handle = executor.handle();
        let expected = self.rows.len();
        let statements = self
            .returning_entity()
            .statements(handle.dialect().max_bind_params())?;

        let mut entities = Vec::with_capacity(expected);
        for statement in &statements {
            let rows = handle
                .fetch_all(statement)
                .await
                .map_err(write_error::<E>)?;
            for row in &rows {
                entities.push(E::from_row(row)?);
            }
        }
        Ok(entities)
    }

    /// Sets `column` to `value`, overriding it in place when the row already
    /// supplies it and widening every row when it does not.
    fn assign(&mut self, column: Ident, value: Expr) {
        match self.columns.iter().position(|name| *name == column) {
            Some(index) => {
                for row in &mut self.rows {
                    if let Some(slot) = row.get_mut(index) {
                        *slot = value.clone();
                    }
                }
            }
            None => {
                self.columns.push(column);
                for row in &mut self.rows {
                    row.push(value.clone());
                }
            }
        }
    }

    /// Adds the conflict clause and the `RETURNING` list to a built insert.
    fn finish(&self, mut insert: moso_sql::Insert) -> Result<Statement> {
        if let Some(conflict) = self.conflict.as_ref() {
            insert = insert.on_conflict(self.conflict_clause(conflict)?);
        }
        if self.returning_entity {
            insert = insert.returning(returning_entity::<E>());
        }
        Ok(insert.into_statement())
    }

    /// Translates the entity-level conflict clause into a SQL one.
    fn conflict_clause(&self, conflict: &Conflict) -> Result<OnConflict> {
        let target = conflict.target();
        let base = if target.is_empty() {
            if conflict.action().writes() {
                return Err(Error::Build(moso_sql::Error::InvalidClause {
                    clause: "ON CONFLICT … DO UPDATE",
                    reason: "no conflict target was named, and an update needs to know which \
                             unique index it is resolving",
                    help: "name the column: `.on_conflict(User::EMAIL).do_update(..)`",
                }));
            }
            OnConflict::any()
        } else {
            OnConflict::columns(target.iter().cloned())
        };

        Ok(match conflict.action() {
            ConflictAction::Nothing => base.do_nothing(),
            ConflictAction::Update(columns) => {
                let mut assignments: Vec<Assignment> = columns
                    .iter()
                    .map(|column| Assignment::new(column.clone(), Expr::excluded(column.clone())))
                    .collect();
                if let Some(updated_at) = E::descriptor().updated_at()
                    && !columns.contains(updated_at)
                {
                    assignments.push(Assignment::new(updated_at.clone(), current_timestamp()));
                }
                base.do_update(assignments)
            }
        })
    }

    /// Refuses to write a tenant-scoped row with no tenant.
    fn check_tenant(&self) -> Result<()> {
        match tenant_column::<E>() {
            Some(column) if !self.columns.contains(&column) => {
                Err(Error::TenantMissing { entity: E::NAME })
            }
            _ => Ok(()),
        }
    }

    /// Refuses a row that does not line up with the column list, before the
    /// server has to.
    fn check_arity(&self) -> Result<()> {
        for (index, row) in self.rows.iter().enumerate() {
            if row.len() != self.columns.len() {
                return Err(Error::Build(moso_sql::Error::RowArity {
                    row: index,
                    expected: self.columns.len(),
                    found: row.len(),
                }));
            }
        }
        Ok(())
    }

    /// The error an insert with no rows produces.
    fn nothing_to_insert() -> Error {
        Error::Build(moso_sql::Error::incomplete(
            "INSERT",
            "any rows to insert",
            "call `Entity::insert(row)` or `Entity::insert_many(rows)` with at least one row; an \
             empty bulk insert is a no-op through `execute`, which reports zero rows",
        ))
    }
}

impl<E: Entity> core::fmt::Debug for Insert<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Insert")
            .field("entity", &E::NAME)
            .field("columns", &self.columns.len())
            .field("rows", &self.rows.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::descriptor::EntityDescriptor;
    use crate::entity::ColumnDef;
    use crate::row::{DecodeError, Row};
    use moso_sql::{ConflictTarget, Returning, TableRef, ValueKind};
    use std::sync::OnceLock;

    /// A tag, reduced to what an insert test needs.
    #[derive(Clone, Debug)]
    struct Tag {
        id: i64,
    }

    impl Entity for Tag {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("tags");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64)
                .primary_key()
                .with_default(),
            ColumnDef::new("name", ValueKind::Text).unique(),
        ];
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
            DESCRIPTOR.get_or_init(|| {
                EntityDescriptor::builder("Tag", Self::TABLE)
                    .timestamps("created_at", "updated_at")
                    .build()
            })
        }
    }

    /// What has to be supplied to create a tag: the key has a default, so it is
    /// not in here — which is the point of the generated `New…` struct.
    struct NewTag {
        name: String,
    }

    impl NewEntity for NewTag {
        const COLUMNS: &'static [&'static str] = &["name"];

        fn into_row(self) -> Vec<Expr> {
            vec![Expr::value(self.name)]
        }
    }

    /// An invoice, which no query may touch without naming its tenant.
    #[derive(Clone, Debug)]
    struct Invoice {
        id: i64,
    }

    impl Entity for Invoice {
        type Pk = i64;

        const TABLE: TableRef = TableRef::from_static("invoices");
        const COLUMNS: &'static [ColumnDef] = &[
            ColumnDef::new("id", ValueKind::I64).primary_key(),
            ColumnDef::new("total", ValueKind::I64),
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
                    .tenant("tenant_id")
                    .build()
            })
        }
    }

    /// The amount to invoice, and nothing else: the tenant is the framework's.
    struct NewInvoice {
        total: i64,
    }

    impl NewEntity for NewInvoice {
        const COLUMNS: &'static [&'static str] = &["total"];

        fn into_row(self) -> Vec<Expr> {
            vec![Expr::value(self.total)]
        }
    }

    fn tags(names: &[&str]) -> Vec<NewTag> {
        names
            .iter()
            .map(|name| NewTag {
                name: (*name).to_owned(),
            })
            .collect()
    }

    fn as_insert(statement: &Statement) -> &moso_sql::Insert {
        match statement {
            Statement::Insert(insert) => insert,
            other => panic!("expected an INSERT, got {other:?}"),
        }
    }

    #[test]
    fn a_new_struct_omits_the_columns_the_database_supplies() {
        assert_eq!(NewTag::COLUMNS, ["name"]);
        let insert = Insert::<Tag>::row(NewTag {
            name: "rust".into(),
        });
        assert_eq!(insert.columns().len(), 1);
        assert_eq!(insert.row_count(), 1);
    }

    #[test]
    fn many_rows_are_one_statement() {
        let insert = Insert::<Tag>::rows(tags(&["a", "b", "c"]));
        assert_eq!(insert.row_count(), 3);
        assert_eq!(insert.columns().len(), 1);

        let statement = insert.to_statement().expect("a valid insert");
        let built = as_insert(&statement);
        assert_eq!(built.row_count(), 3, "three rows, one statement");
        assert_eq!(built.bind_count(), 3);
        assert_eq!(built.table().name().as_str(), "tags");
    }

    #[test]
    fn an_empty_bulk_insert_is_a_no_op_not_broken_sql() {
        let insert = Insert::<Tag>::rows(Vec::<NewTag>::new());
        assert_eq!(insert.row_count(), 0);
        let error = insert.to_statement().expect_err("nothing to insert");
        assert!(error.to_string().contains("help:"), "{error}");
    }

    #[test]
    fn set_overrides_a_supplied_column_and_appends_a_new_one() {
        const NAME: Column<Tag, String> = Column::new("name");
        const ID: Column<Tag, i64> = Column::new("id");

        let overridden = Insert::<Tag>::row(NewTag { name: "a".into() }).set(NAME, "b");
        assert_eq!(overridden.columns().len(), 1, "an override does not widen");
        let statement = overridden.to_statement().expect("a valid insert");
        assert_eq!(as_insert(&statement).bind_count(), 1);

        let widened = Insert::<Tag>::row(NewTag { name: "a".into() }).set(ID, 7_i64);
        assert_eq!(widened.columns().len(), 2, "a new column widens");
        let statement = widened.to_statement().expect("a valid insert");
        assert_eq!(as_insert(&statement).bind_count(), 2);
    }

    #[test]
    fn an_upsert_records_its_target_and_action() {
        const NAME: Column<Tag, String> = Column::new("name");
        let upsert = Insert::<Tag>::row(NewTag { name: "a".into() })
            .on_conflict(NAME)
            .do_update([Ident::from_static("name")]);
        let conflict = upsert.conflict().expect("a conflict clause");
        assert_eq!(conflict.target().len(), 1);
        assert!(conflict.action().writes());
    }

    #[test]
    fn an_upsert_becomes_do_update_set_from_excluded() {
        const NAME: Column<Tag, String> = Column::new("name");
        let statement = Insert::<Tag>::row(NewTag { name: "a".into() })
            .on_conflict(NAME)
            .do_update([Ident::from_static("name")])
            .to_statement()
            .expect("a valid upsert");

        let clause = as_insert(&statement).conflict().expect("ON CONFLICT");
        assert!(matches!(clause.target(), ConflictTarget::Columns(columns) if columns.len() == 1));
        let moso_sql::ConflictAction::DoUpdate(assignments) = clause.action() else {
            panic!("expected a DO UPDATE");
        };
        assert_eq!(assignments[0].column().as_str(), "name");
        assert_eq!(
            assignments[0].value(),
            &Expr::excluded(Ident::from_static("name"))
        );
        assert_eq!(
            assignments[1].column().as_str(),
            "updated_at",
            "a managed timestamp is bumped by the upsert, not left stale"
        );
    }

    #[test]
    fn do_nothing_needs_no_target_and_do_update_does() {
        let ignored = Insert::<Tag>::row(NewTag { name: "a".into() })
            .on_conflict_columns([])
            .to_statement()
            .expect("ON CONFLICT DO NOTHING infers the index");
        let clause = as_insert(&ignored).conflict().expect("ON CONFLICT");
        assert!(matches!(clause.target(), ConflictTarget::Any));

        let ambiguous = Insert::<Tag>::row(NewTag { name: "a".into() })
            .on_conflict_columns([])
            .do_update([Ident::from_static("name")])
            .to_statement()
            .expect_err("an update needs to know which index it resolves");
        assert!(ambiguous.to_string().contains("help:"), "{ambiguous}");
        assert!(ambiguous.is_programmer_error());
    }

    #[test]
    fn returning_entity_lists_the_columns_in_decode_order() {
        let plain = Insert::<Tag>::row(NewTag { name: "a".into() })
            .to_statement()
            .expect("a valid insert");
        assert_eq!(as_insert(&plain).returning_clause(), &Returning::None);

        let returning = Insert::<Tag>::row(NewTag { name: "a".into() })
            .returning_entity()
            .to_statement()
            .expect("a valid insert");
        let Returning::Items(items) = as_insert(&returning).returning_clause() else {
            panic!("expected an explicit RETURNING list, never `*`");
        };
        assert_eq!(items.len(), Tag::COLUMNS.len());
    }

    #[test]
    fn a_bulk_insert_chunks_to_the_dialects_parameter_limit() {
        let insert = Insert::<Tag>::rows(tags(&["a", "b", "c", "d", "e"]));

        let unbounded = insert.statements(usize::MAX).expect("one statement");
        assert_eq!(unbounded.len(), 1);
        assert_eq!(as_insert(&unbounded[0]).row_count(), 5);

        // One parameter per row here, so a limit of two is two rows a chunk.
        let chunked = insert.statements(2).expect("three statements");
        assert_eq!(chunked.len(), 3);
        assert_eq!(as_insert(&chunked[0]).row_count(), 2);
        assert_eq!(as_insert(&chunked[2]).row_count(), 1);
        let total: usize = chunked.iter().map(|s| as_insert(s).row_count()).sum();
        assert_eq!(total, 5, "chunking loses no rows");

        // A limit below one row's width still makes progress rather than
        // dividing by zero or looping forever.
        assert_eq!(insert.statements(0).expect("five statements").len(), 5);
    }

    #[test]
    fn a_tenant_scoped_entity_refuses_to_be_inserted_without_one() {
        let unscoped = Insert::<Invoice>::row(NewInvoice { total: 100 });
        let error = unscoped.to_statement().expect_err("no tenant");
        assert!(matches!(error, Error::TenantMissing { entity: "Invoice" }));
        assert!(error.to_string().contains("help:"), "{error}");
        assert!(error.is_programmer_error());

        let scoped = Insert::<Invoice>::row(NewInvoice { total: 100 }).scoped(TenantId::of(7_i64));
        let statement = scoped.to_statement().expect("a tenant was named");
        let built = as_insert(&statement);
        assert_eq!(built.column_names().len(), 2);
        assert_eq!(built.column_names()[1].as_str(), "tenant_id");
        assert_eq!(built.bind_count(), 2);
    }

    #[test]
    fn a_row_that_does_not_line_up_with_the_columns_is_refused_before_the_server_sees_it() {
        /// A row that claims two columns and supplies one.
        struct Broken;

        impl NewEntity for Broken {
            const COLUMNS: &'static [&'static str] = &["name", "slug"];

            fn into_row(self) -> Vec<Expr> {
                vec![Expr::value("a")]
            }
        }

        let error = Insert::<Tag>::row(Broken)
            .to_statement()
            .expect_err("one value, two columns");
        assert!(error.to_string().contains("help:"), "{error}");
        assert!(matches!(
            error,
            Error::Build(moso_sql::Error::RowArity {
                row: 0,
                expected: 2,
                found: 1
            })
        ));
    }

    #[test]
    fn an_entity_whose_row_supplies_nothing_inserts_the_defaults() {
        /// Everything about this row comes from the table's defaults.
        struct Blank;

        impl NewEntity for Blank {
            const COLUMNS: &'static [&'static str] = &[];

            fn into_row(self) -> Vec<Expr> {
                Vec::new()
            }
        }

        let statement = Insert::<Tag>::row(Blank)
            .to_statement()
            .expect("DEFAULT VALUES is valid SQL; an empty column list is not");
        assert!(as_insert(&statement).uses_default_values());
    }
}
