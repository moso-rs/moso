//! Turning a [`Diff`] into SQL for one backend.
//!
//! The planner is where the zero-downtime idioms live. The operation table in
//! `docs/02-data/23-migrations.md` does not say "add an index"; it says
//! `CREATE INDEX CONCURRENTLY`, and it says a new foreign key is
//! `ADD CONSTRAINT … NOT VALID` followed by `VALIDATE CONSTRAINT`. Those are
//! not decorations. The first takes a lock for the duration of the build; the
//! second scans every row while holding `ACCESS EXCLUSIVE`. Getting them right
//! is the difference between a deploy and an outage.
//!
//! It is also where the two backends stop looking alike. SQLite cannot alter a
//! column's type, change its nullability, change its default, or add a
//! constraint, so every such change becomes the 12-step table rebuild — and
//! since several changes to one table collapse into one rebuild, the planner
//! groups them.
//!
//! ```
//! use moso_migrate::diff::Diff;
//! use moso_migrate::plan::Plan;
//! use moso_migrate::rename::DropAndAdd;
//! use moso_migrate::schema::{Column, Schema, Table};
//! use moso_orm::Backend;
//! use moso_sql::DataType;
//!
//! let before = Schema::empty();
//! let mut after = Schema::empty();
//! let mut users = Table::new("users");
//! users.add_column(Column::new("id", DataType::BigSerial));
//! users.set_primary_key(["id"]);
//! after.add_table(users);
//!
//! let diff = Diff::compute(&before, &after, &DropAndAdd)?;
//! let plan = Plan::build(&diff, &before, &after, Backend::Postgres)?;
//! assert_eq!(plan.operations().len(), 1);
//! assert!(plan.is_reversible());
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};

use moso_orm::Backend;
use moso_sql::ddl::{
    AlterTable, AlterTableAction, AlterType, AlterTypeAction, ColumnSpec, CreateExtension,
    CreateIndex, CreateSchema, CreateTable, CreateType, Ddl, DropIndex, DropTable, DropType,
    ForeignKey as SqlForeignKey, Generated as SqlGenerated, IndexMethod, IndexTarget,
    PartitionStrategy, Partitioning, RenameIndex, RenameTable, TableConstraint, TypeBody,
};
use moso_sql::{DataType, Expr, Ident, Nulls, Order, RawExpr, TableRef, TypeRef};

use crate::diff::{Change, Diff};
use crate::emit;
use crate::error::{Error, Result};
use crate::schema::{
    Check, Column, ForeignKey, Index, IndexPart, NullsOrder, Partition, Schema, Sort, Table,
    unqualify,
};

/// The suffix the SQLite table rebuild gives its scratch table.
///
/// Exposed because a `moso db check` run against a database where a rebuild
/// failed part-way will see it, and the message needs to name it.
///
/// ```
/// assert_eq!(moso_migrate::plan::REBUILD_SUFFIX, "__moso_new");
/// ```
pub const REBUILD_SUFFIX: &str = "__moso_new";

/// One step of a migration: the SQL to apply it, the SQL to undo it, and what
/// the runner needs to know about it.
///
/// ```
/// use moso_migrate::plan::Operation;
///
/// let op = Operation::new("create the table `users`", ["CREATE TABLE \"users\" ()"])
///     .reversed_by(["DROP TABLE \"users\""]);
/// assert!(op.is_reversible());
/// assert!(!op.is_destructive());
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    description: String,
    up: Vec<String>,
    down: Vec<String>,
    reversible: bool,
    destructive: bool,
    non_transactional: bool,
    notes: Vec<String>,
}

impl Operation {
    /// An operation with its forward SQL, irreversible until
    /// [`Operation::reversed_by`] is called.
    ///
    /// ```
    /// use moso_migrate::plan::Operation;
    ///
    /// let op = Operation::new("do a thing", ["SELECT 1"]);
    /// assert!(!op.is_reversible());
    /// ```
    #[must_use]
    pub fn new(
        description: impl Into<String>,
        up: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            description: description.into(),
            up: up.into_iter().map(Into::into).collect(),
            down: Vec::new(),
            reversible: false,
            destructive: false,
            non_transactional: false,
            notes: Vec::new(),
        }
    }

    /// Gives the operation a down migration.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// let op = Operation::new("x", ["SELECT 1"]).reversed_by(["SELECT 2"]);
    /// assert_eq!(op.down(), ["SELECT 2"]);
    /// ```
    #[must_use]
    pub fn reversed_by(mut self, down: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.down = down.into_iter().map(Into::into).collect();
        self.reversible = true;
        self
    }

    /// Marks the operation destructive, so it is emitted commented out.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(Operation::new("x", ["DROP TABLE t"]).destructive().is_destructive());
    /// ```
    #[must_use]
    pub const fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Marks the operation as one that cannot run inside a transaction.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(Operation::new("x", ["CREATE INDEX CONCURRENTLY i ON t (c)"])
    ///     .outside_a_transaction()
    ///     .requires_no_transaction());
    /// ```
    #[must_use]
    pub const fn outside_a_transaction(mut self) -> Self {
        self.non_transactional = true;
        self
    }

    /// Attaches a comment that is written above the SQL.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// let op = Operation::new("x", ["SELECT 1"]).note("this takes a while");
    /// assert_eq!(op.notes(), ["this takes a while"]);
    /// ```
    #[must_use]
    pub fn note(mut self, note: impl Into<String>) -> Self {
        self.notes.push(note.into());
        self
    }

    /// What the operation does, in one line.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert_eq!(Operation::new("x", ["SELECT 1"]).description(), "x");
    /// ```
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The forward statements.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert_eq!(Operation::new("x", ["SELECT 1"]).up(), ["SELECT 1"]);
    /// ```
    #[must_use]
    pub fn up(&self) -> &[String] {
        &self.up
    }

    /// The reverse statements, empty when the operation is irreversible.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(Operation::new("x", ["SELECT 1"]).down().is_empty());
    /// ```
    #[must_use]
    pub fn down(&self) -> &[String] {
        &self.down
    }

    /// Whether it can be undone.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(!Operation::new("x", ["SELECT 1"]).is_reversible());
    /// ```
    #[must_use]
    pub const fn is_reversible(&self) -> bool {
        self.reversible
    }

    /// Whether it can destroy data.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(!Operation::new("x", ["SELECT 1"]).is_destructive());
    /// ```
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        self.destructive
    }

    /// Whether it must run outside a transaction.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(!Operation::new("x", ["SELECT 1"]).requires_no_transaction());
    /// ```
    #[must_use]
    pub const fn requires_no_transaction(&self) -> bool {
        self.non_transactional
    }

    /// The attached comments.
    ///
    /// ```
    /// # use moso_migrate::plan::Operation;
    /// assert!(Operation::new("x", ["SELECT 1"]).notes().is_empty());
    /// ```
    #[must_use]
    pub fn notes(&self) -> &[String] {
        &self.notes
    }
}

/// A whole migration's worth of operations, for one backend.
///
/// ```
/// use moso_migrate::plan::Plan;
/// use moso_orm::Backend;
///
/// let plan = Plan::empty(Backend::Postgres);
/// assert!(plan.is_empty());
/// assert!(plan.is_reversible(), "an empty plan reverses trivially");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    backend: Backend,
    operations: Vec<Operation>,
}

impl Plan {
    /// A plan with nothing in it.
    ///
    /// ```
    /// use moso_migrate::plan::Plan;
    /// use moso_orm::Backend;
    ///
    /// assert!(Plan::empty(Backend::Sqlite).is_empty());
    /// ```
    #[must_use]
    pub const fn empty(backend: Backend) -> Self {
        Self {
            backend,
            operations: Vec::new(),
        }
    }

    /// Plans `diff`, which takes `before` to `after`, for `backend`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] when a change cannot be expressed on the backend
    /// at all — a partitioned table on SQLite, a `gin` index on SQLite — with
    /// the alternative named. [`Error::Ident`] when a name in the snapshot is
    /// not a legal identifier, which means the snapshot was hand-edited.
    ///
    /// ```
    /// use moso_migrate::diff::Diff;
    /// use moso_migrate::plan::Plan;
    /// use moso_migrate::Schema;
    /// use moso_orm::Backend;
    ///
    /// let empty = Schema::empty();
    /// let plan = Plan::build(&Diff::empty(), &empty, &empty, Backend::Postgres)?;
    /// assert!(plan.is_empty());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn build(diff: &Diff, before: &Schema, after: &Schema, backend: Backend) -> Result<Self> {
        let mut plan = Self::empty(backend);
        let rebuilds = if backend == Backend::Sqlite {
            tables_needing_a_rebuild(diff)
        } else {
            BTreeSet::new()
        };
        let mut rebuilt: BTreeSet<String> = BTreeSet::new();

        for change in diff.changes() {
            // A rebuild recreates the table from its *whole* new definition, so
            // it absorbs every other change to that table. Emitting the rebuild
            // and then, say, an `ADD COLUMN` for a column the rebuild already
            // created would fail at apply time.
            if let Some(table) = change.table()
                && rebuilds.contains(table)
                && absorbed_by_a_rebuild(change)
            {
                if rebuilt.insert(table.to_owned()) {
                    plan.operations
                        .push(rebuild_table(table, before, after, diff)?);
                }
                continue;
            }
            plan.operations
                .push(plan_change(change, before, after, backend)?);
        }
        Ok(plan)
    }

    /// The backend this plan is for.
    ///
    /// ```
    /// # use moso_migrate::plan::Plan;
    /// # use moso_orm::Backend;
    /// assert_eq!(Plan::empty(Backend::Sqlite).backend(), Backend::Sqlite);
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }

    /// The operations, in order.
    ///
    /// ```
    /// # use moso_migrate::plan::Plan;
    /// # use moso_orm::Backend;
    /// assert!(Plan::empty(Backend::Postgres).operations().is_empty());
    /// ```
    #[must_use]
    pub fn operations(&self) -> &[Operation] {
        &self.operations
    }

    /// Whether there is nothing to do.
    ///
    /// ```
    /// # use moso_migrate::plan::Plan;
    /// # use moso_orm::Backend;
    /// assert!(Plan::empty(Backend::Postgres).is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Whether every operation can be undone.
    ///
    /// ```
    /// # use moso_migrate::plan::Plan;
    /// # use moso_orm::Backend;
    /// assert!(Plan::empty(Backend::Postgres).is_reversible());
    /// ```
    #[must_use]
    pub fn is_reversible(&self) -> bool {
        self.operations.iter().all(Operation::is_reversible)
    }

    /// Whether any operation is destructive.
    ///
    /// ```
    /// # use moso_migrate::plan::Plan;
    /// # use moso_orm::Backend;
    /// assert!(!Plan::empty(Backend::Postgres).is_destructive());
    /// ```
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        self.operations.iter().any(Operation::is_destructive)
    }

    /// Whether the migration as a whole must run outside a transaction.
    ///
    /// One operation is enough: a file is transactional or it is not.
    ///
    /// ```
    /// # use moso_migrate::plan::Plan;
    /// # use moso_orm::Backend;
    /// assert!(!Plan::empty(Backend::Postgres).requires_no_transaction());
    /// ```
    #[must_use]
    pub fn requires_no_transaction(&self) -> bool {
        self.operations
            .iter()
            .any(Operation::requires_no_transaction)
    }

    /// Adds an operation, for a caller assembling a plan by hand — a squash, or
    /// a data migration written alongside a generated one.
    ///
    /// ```
    /// use moso_migrate::plan::{Operation, Plan};
    /// use moso_orm::Backend;
    ///
    /// let mut plan = Plan::empty(Backend::Postgres);
    /// plan.push(Operation::new("x", ["SELECT 1"]));
    /// assert_eq!(plan.operations().len(), 1);
    /// ```
    pub fn push(&mut self, operation: Operation) {
        self.operations.push(operation);
    }
}

/// Which changes SQLite cannot make with `ALTER TABLE` and therefore has to
/// express as a whole-table rebuild.
///
/// SQLite gained `DROP COLUMN` in 3.35 and `RENAME COLUMN` in 3.25, but it has
/// never had `ALTER COLUMN` in any form: no type change, no nullability change,
/// no default change, and no way to add or remove a constraint.
fn rebuild_covers(change: &Change) -> bool {
    matches!(
        change,
        Change::AlterColumnType { .. }
            | Change::SetNotNull { .. }
            | Change::DropNotNull { .. }
            | Change::SetDefault { .. }
            | Change::DropDefault { .. }
            | Change::SetPrimaryKey { .. }
            | Change::AddCheck { .. }
            | Change::DropCheck { .. }
            | Change::AddForeignKey { .. }
            | Change::DropForeignKey { .. }
    )
}

/// Which changes a rebuild of the same table subsumes.
///
/// Wider than [`rebuild_covers`]: once a table is being recreated from its new
/// definition, every other change to it has already happened. Emitting an
/// `ADD COLUMN` after a rebuild that created the column fails at apply time.
fn absorbed_by_a_rebuild(change: &Change) -> bool {
    !matches!(
        change,
        Change::CreateTable(_) | Change::DropTable(_) | Change::RenameTable { .. }
    )
}

fn tables_needing_a_rebuild(diff: &Diff) -> BTreeSet<String> {
    diff.changes()
        .iter()
        .filter(|change| rebuild_covers(change))
        .filter_map(|change| change.table().map(ToOwned::to_owned))
        .collect()
}

fn plan_change(
    change: &Change,
    before: &Schema,
    after: &Schema,
    backend: Backend,
) -> Result<Operation> {
    let description = change.description();
    Ok(match change {
        Change::CreateSchema(name) => Operation::new(
            description,
            [render(
                &Ddl::CreateSchema(CreateSchema::new(ident(name)?).if_not_exists()),
                backend,
            )?],
        )
        .reversed_by(Vec::<String>::new()),

        Change::CreateExtension(name) => Operation::new(
            description,
            [render(
                &Ddl::CreateExtension(CreateExtension::new(ident(name)?).if_not_exists()),
                backend,
            )?],
        )
        .reversed_by(Vec::<String>::new()),

        Change::CreateEnum(enum_type) => {
            let name = type_ref(&enum_type.qualified_name())?;
            Operation::new(
                description,
                [render(
                    &Ddl::CreateType(CreateType::new(
                        name.clone(),
                        TypeBody::enumeration(enum_type.labels().iter().cloned()),
                    )),
                    backend,
                )?],
            )
            .reversed_by([render(&Ddl::DropType(DropType::new(name)), backend)?])
        }

        Change::AddEnumValues {
            name,
            values,
            after: previous,
        } => {
            let type_name = type_ref(name)?;
            let mut statements = Vec::with_capacity(values.len());
            let mut anchor = previous.clone();
            for value in values {
                statements.push(render(
                    &Ddl::AlterType(AlterType::new(
                        type_name.clone(),
                        AlterTypeAction::AddValue {
                            value: value.clone(),
                            before: None,
                            after: Some(anchor.clone()),
                            if_not_exists: true,
                        },
                    )),
                    backend,
                )?);
                anchor.clone_from(value);
            }
            Operation::new(description, statements)
                .outside_a_transaction()
                .note(
                    "`ALTER TYPE … ADD VALUE` cannot run inside a transaction, which is why this \
                     migration is marked `transactional false`",
                )
                // PostgreSQL has no `ALTER TYPE … DROP VALUE`. Saying so is
                // better than emitting SQL that fails on rollback.
                .note("PostgreSQL cannot remove an enum label, so this step is irreversible")
        }

        Change::RewriteEnum {
            before: was,
            after: wants,
        } => Operation::new(description, [manual_enum_template(was, wants)])
            .destructive()
            .note(
                "removing or reordering an enum label needs a plan Moso cannot write for you: \
                 the rows holding the removed label have to go somewhere",
            )
            .note(
                "this block is a template, not SQL: `--allow-destructive` refuses it rather than \
                 recording the migration as applied without it",
            ),

        Change::DropEnum(enum_type) => {
            let name = type_ref(&enum_type.qualified_name())?;
            Operation::new(
                description,
                [render(
                    &Ddl::DropType(DropType::new(name.clone()).if_exists()),
                    backend,
                )?],
            )
            .destructive()
            .reversed_by([render(
                &Ddl::CreateType(CreateType::new(
                    name,
                    TypeBody::enumeration(enum_type.labels().iter().cloned()),
                )),
                backend,
            )?])
        }

        Change::CreateTable(table) => plan_create_table(table, backend)?,

        Change::DropTable(table) => {
            let reference = table_ref(&table.qualified_name())?;
            let mut down = vec![render(
                &Ddl::CreateTable(create_table_ddl(table, backend)?),
                backend,
            )?];
            for index in table.indexes() {
                if inlined_as_a_constraint(index, backend) {
                    continue;
                }
                down.push(render(
                    &Ddl::CreateIndex(create_index_ddl(
                        &table.qualified_name(),
                        index,
                        backend,
                        false,
                    )?),
                    backend,
                )?);
            }
            Operation::new(
                description,
                [render(
                    &Ddl::DropTable(DropTable::new([reference])),
                    backend,
                )?],
            )
            .destructive()
            .reversed_by(down)
            .note("the rows are not recoverable; the down migration recreates the table empty")
        }

        Change::RenameTable { from, to } => {
            let (schema, _) = unqualify(from);
            let restored = crate::schema::qualify(schema, to);
            Operation::new(
                description,
                [render(
                    &Ddl::RenameTable(RenameTable::new(table_ref(from)?, ident(to)?)),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::RenameTable(RenameTable::new(
                    table_ref(&restored)?,
                    ident(unqualify(from).1)?,
                )),
                backend,
            )?])
        }

        Change::AddColumn {
            table,
            column,
            needs_backfill,
        } => plan_add_column(table, column, *needs_backfill, backend)?,

        Change::DropColumn { table, column } => {
            let reference = table_ref(table)?;
            Operation::new(
                description,
                [render(
                    &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                        AlterTableAction::DropColumn {
                            name: ident(column.name())?,
                            if_exists: false,
                            cascade: false,
                        },
                    )),
                    backend,
                )?],
            )
            .destructive()
            .reversed_by([render(
                &Ddl::AlterTable(
                    AlterTable::new(reference)
                        .add_column(column_spec(&column.clone().nullable(), backend)?),
                ),
                backend,
            )?])
            .note(format!(
                "the data in `{table}.{}` is not recoverable; the down migration adds the column \
                 back empty and nullable",
                column.name()
            ))
        }

        Change::RenameColumn { table, from, to } => {
            let reference = table_ref(table)?;
            Operation::new(
                description,
                [render(
                    &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                        AlterTableAction::RenameColumn {
                            from: ident(from)?,
                            to: ident(to)?,
                        },
                    )),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::AlterTable(AlterTable::new(reference).action(
                    AlterTableAction::RenameColumn {
                        from: ident(to)?,
                        to: ident(from)?,
                    },
                )),
                backend,
            )?])
        }

        Change::AlterColumnType {
            table,
            column,
            from,
            to,
            lossy,
        } => {
            let from_type = crate::schema::parse(from)?;
            let to_type = crate::schema::parse(to)?;
            let forward = alter_type_statement(table, column, &to_type, &from_type, backend)?;
            let backward = alter_type_statement(table, column, &from_type, &to_type, backend)?;
            let mut operation = Operation::new(description, [forward]).reversed_by([backward]);
            if *lossy {
                operation = operation.destructive().note(format!(
                    "converting `{table}.{column}` from {from} to {to} can lose data; \
                     check the `USING` expression before uncommenting"
                ));
            }
            operation
        }

        Change::SetNotNull {
            table,
            column,
            needs_backfill,
        } => {
            let reference = table_ref(table)?;
            let mut statements = Vec::new();
            let mut operation_notes = Vec::new();
            if *needs_backfill {
                let existing = after
                    .table(table)
                    .and_then(|table| table.column(column))
                    .or_else(|| before.table(table).and_then(|table| table.column(column)));
                let fill = existing.map_or_else(|| "''".to_owned(), placeholder_for);
                statements.push(format!(
                    "UPDATE {} SET {} = {fill} WHERE {} IS NULL",
                    emit::quote_name(table),
                    emit::quote_name(column),
                    emit::quote_name(column)
                ));
                operation_notes.push(format!(
                    "REVIEW: `{table}.{column}` may contain NULLs. The backfill value {fill} is a \
                     placeholder — replace it before applying."
                ));
            }
            statements.push(render(
                &Ddl::AlterTable(
                    AlterTable::new(reference.clone())
                        .action(AlterTableAction::SetNotNull(ident(column)?)),
                ),
                backend,
            )?);
            let mut operation = Operation::new(description, statements).reversed_by([render(
                &Ddl::AlterTable(
                    AlterTable::new(reference)
                        .action(AlterTableAction::DropNotNull(ident(column)?)),
                ),
                backend,
            )?]);
            for note in operation_notes {
                operation = operation.note(note);
            }
            operation
        }

        Change::DropNotNull { table, column } => {
            let reference = table_ref(table)?;
            Operation::new(
                description,
                [render(
                    &Ddl::AlterTable(
                        AlterTable::new(reference.clone())
                            .action(AlterTableAction::DropNotNull(ident(column)?)),
                    ),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::AlterTable(
                    AlterTable::new(reference).action(AlterTableAction::SetNotNull(ident(column)?)),
                ),
                backend,
            )?])
        }

        Change::SetDefault {
            table,
            column,
            default,
            previous,
        } => {
            let reference = table_ref(table)?;
            let forward = render(
                &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                    AlterTableAction::SetDefault {
                        name: ident(column)?,
                        value: raw(default),
                    },
                )),
                backend,
            )?;
            let backward = match previous {
                Some(previous) => render(
                    &Ddl::AlterTable(AlterTable::new(reference).action(
                        AlterTableAction::SetDefault {
                            name: ident(column)?,
                            value: raw(previous),
                        },
                    )),
                    backend,
                )?,
                None => render(
                    &Ddl::AlterTable(
                        AlterTable::new(reference)
                            .action(AlterTableAction::DropDefault(ident(column)?)),
                    ),
                    backend,
                )?,
            };
            Operation::new(description, [forward]).reversed_by([backward])
        }

        Change::DropDefault {
            table,
            column,
            previous,
        } => {
            let reference = table_ref(table)?;
            Operation::new(
                description,
                [render(
                    &Ddl::AlterTable(
                        AlterTable::new(reference.clone())
                            .action(AlterTableAction::DropDefault(ident(column)?)),
                    ),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::AlterTable(
                    AlterTable::new(reference).action(AlterTableAction::SetDefault {
                        name: ident(column)?,
                        value: raw(previous),
                    }),
                ),
                backend,
            )?])
        }

        Change::SetPrimaryKey {
            table,
            columns,
            previous,
        } => {
            let reference = table_ref(table)?;
            let name = format!("{}_pkey", unqualify(table).1);
            let mut statements = Vec::new();
            if !previous.is_empty() {
                statements.push(render(
                    &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                        AlterTableAction::DropConstraint {
                            name: ident(&name)?,
                            if_exists: true,
                            cascade: false,
                        },
                    )),
                    backend,
                )?);
            }
            statements.push(render(
                &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                    AlterTableAction::AddConstraint(TableConstraint::primary_key(
                        Some(ident(&name)?),
                        idents(columns)?,
                    )),
                )),
                backend,
            )?);
            let mut down = vec![render(
                &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                    AlterTableAction::DropConstraint {
                        name: ident(&name)?,
                        if_exists: true,
                        cascade: false,
                    },
                )),
                backend,
            )?];
            if !previous.is_empty() {
                down.push(render(
                    &Ddl::AlterTable(AlterTable::new(reference).action(
                        AlterTableAction::AddConstraint(TableConstraint::primary_key(
                            Some(ident(&name)?),
                            idents(previous)?,
                        )),
                    )),
                    backend,
                )?);
            }
            Operation::new(description, statements).reversed_by(down)
        }

        Change::CreateIndex { table, index } => plan_create_index(table, index, backend)?,

        Change::DropIndex { table, index } => {
            let mut operation = Operation::new(
                description,
                [render(
                    &Ddl::DropIndex(drop_index_ddl(index, backend, true)),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::CreateIndex(create_index_ddl(table, index, backend, true)?),
                backend,
            )?]);
            if backend == Backend::Postgres {
                operation = operation
                    .outside_a_transaction()
                    .note("`DROP INDEX CONCURRENTLY` cannot run inside a transaction");
            }
            operation
        }

        Change::RenameIndex { from, to, .. } => Operation::new(
            description,
            [render(
                &Ddl::RenameIndex(RenameIndex::new(ident(from)?, ident(to)?)),
                backend,
            )?],
        )
        .reversed_by([render(
            &Ddl::RenameIndex(RenameIndex::new(ident(to)?, ident(from)?)),
            backend,
        )?]),

        Change::AddForeignKey { table, foreign_key } => {
            plan_add_foreign_key(table, foreign_key, backend)?
        }

        Change::DropForeignKey { table, foreign_key } => {
            let reference = table_ref(table)?;
            Operation::new(
                description,
                [render(
                    &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                        AlterTableAction::DropConstraint {
                            name: ident(foreign_key.name())?,
                            if_exists: true,
                            cascade: false,
                        },
                    )),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::AlterTable(AlterTable::new(reference).action(
                    AlterTableAction::AddConstraint(TableConstraint::ForeignKey(foreign_key_ddl(
                        foreign_key,
                        false,
                    )?)),
                )),
                backend,
            )?])
        }

        Change::AddCheck { table, check } => {
            let reference = table_ref(table)?;
            let mut statements = vec![render(
                &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                    AlterTableAction::AddConstraint(check_ddl(
                        check,
                        backend == Backend::Postgres,
                    )?),
                )),
                backend,
            )?];
            if backend == Backend::Postgres {
                statements.push(render(
                    &Ddl::AlterTable(
                        AlterTable::new(reference.clone())
                            .action(AlterTableAction::ValidateConstraint(ident(check.name())?)),
                    ),
                    backend,
                )?);
            }
            Operation::new(description, statements)
                .reversed_by([render(
                    &Ddl::AlterTable(AlterTable::new(reference).action(
                        AlterTableAction::DropConstraint {
                            name: ident(check.name())?,
                            if_exists: true,
                            cascade: false,
                        },
                    )),
                    backend,
                )?])
                .note(
                    "added `NOT VALID` and validated separately, so the strong lock is held for \
                     the catalogue change only",
                )
        }

        Change::DropCheck { table, check } => {
            let reference = table_ref(table)?;
            Operation::new(
                description,
                [render(
                    &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                        AlterTableAction::DropConstraint {
                            name: ident(check.name())?,
                            if_exists: true,
                            cascade: false,
                        },
                    )),
                    backend,
                )?],
            )
            .reversed_by([render(
                &Ddl::AlterTable(
                    AlterTable::new(reference)
                        .action(AlterTableAction::AddConstraint(check_ddl(check, false)?)),
                ),
                backend,
            )?])
        }

        Change::SetComment {
            table,
            column,
            comment,
            previous,
        } => {
            if backend != Backend::Postgres {
                // SQLite stores no comments. Emitting nothing is honest: there
                // is no schema difference to express.
                Operation::new(description, Vec::<String>::new()).reversed_by(Vec::<String>::new())
            } else {
                Operation::new(
                    description,
                    [comment_statement(
                        table,
                        column.as_deref(),
                        comment.as_deref(),
                    )?],
                )
                .reversed_by([comment_statement(
                    table,
                    column.as_deref(),
                    previous.as_deref(),
                )?])
            }
        }
    })
}

fn plan_create_table(table: &Table, backend: Backend) -> Result<Operation> {
    let mut statements = vec![render(
        &Ddl::CreateTable(create_table_ddl(table, backend)?),
        backend,
    )?];
    for index in table.indexes() {
        // A unique index a constraint owns is already in the `CREATE TABLE`.
        if inlined_as_a_constraint(index, backend) {
            continue;
        }
        // The table was created by the statement above, so nobody can be using
        // it: `CONCURRENTLY` would buy nothing and would force the whole
        // migration out of its transaction.
        statements.push(render(
            &Ddl::CreateIndex(create_index_ddl(
                &table.qualified_name(),
                index,
                backend,
                false,
            )?),
            backend,
        )?);
    }
    if backend == Backend::Postgres {
        if let Some(comment) = table.comment() {
            statements.push(comment_statement(
                &table.qualified_name(),
                None,
                Some(comment),
            )?);
        }
        for column in table.columns() {
            if let Some(comment) = column.comment() {
                statements.push(comment_statement(
                    &table.qualified_name(),
                    Some(column.name()),
                    Some(comment),
                )?);
            }
        }
    }
    Ok(Operation::new(
        format!("create the table `{}`", table.qualified_name()),
        statements,
    )
    .reversed_by([render(
        &Ddl::DropTable(DropTable::new([table_ref(&table.qualified_name())?])),
        backend,
    )?]))
}

fn plan_add_column(
    table: &str,
    column: &Column,
    needs_backfill: bool,
    backend: Backend,
) -> Result<Operation> {
    let reference = table_ref(table)?;
    let description = format!("add `{table}.{}`", column.name());
    let down = render(
        &Ddl::AlterTable(
            AlterTable::new(reference.clone()).action(AlterTableAction::DropColumn {
                name: ident(column.name())?,
                if_exists: true,
                cascade: false,
            }),
        ),
        backend,
    )?;

    if !needs_backfill {
        return Ok(Operation::new(
            description,
            [render(
                &Ddl::AlterTable(
                    AlterTable::new(reference).add_column(column_spec(column, backend)?),
                ),
                backend,
            )?],
        )
        .reversed_by([down]));
    }

    // The expand-safe three-step. Adding a `NOT NULL` column with no default to
    // a table with rows fails outright on PostgreSQL, and on SQLite too; doing
    // it in three steps is the only correct answer, and it is also the one that
    // does not break a rolling deploy.
    let fill = placeholder_for(column);
    let nullable = column.clone().nullable();
    let statements = vec![
        render(
            &Ddl::AlterTable(
                AlterTable::new(reference.clone()).add_column(column_spec(&nullable, backend)?),
            ),
            backend,
        )?,
        format!(
            "UPDATE {} SET {} = {fill} WHERE {} IS NULL",
            emit::quote_name(table),
            emit::quote_name(column.name()),
            emit::quote_name(column.name())
        ),
        render(
            &Ddl::AlterTable(
                AlterTable::new(reference)
                    .action(AlterTableAction::SetNotNull(ident(column.name())?)),
            ),
            backend,
        )?,
    ];

    Ok(Operation::new(description, statements)
        .reversed_by([down])
        .note(format!(
            "REVIEW: `{table}.{}` is NOT NULL with no default, so it is added nullable, \
             backfilled and then tightened. The backfill value {fill} is a placeholder — replace \
             it before applying.",
            column.name()
        )))
}

fn plan_create_index(table: &str, index: &Index, backend: Backend) -> Result<Operation> {
    let description = format!("index `{table}` as `{}`", index.name());
    let create = create_index_ddl(table, index, backend, true)?;
    let drop = render(
        &Ddl::DropIndex(drop_index_ddl(index, backend, true)),
        backend,
    )?;

    if backend != Backend::Postgres {
        return Ok(
            Operation::new(description, [render(&Ddl::CreateIndex(create), backend)?])
                .reversed_by([drop]),
        );
    }

    let mut statements = vec![render(&Ddl::CreateIndex(create), backend)?];
    let mut operation_note = "built `CONCURRENTLY`, so writes are not blocked while it builds; \
                              that is why this migration runs outside a transaction"
        .to_owned();

    // The zero-downtime unique-constraint idiom: build the index without a
    // lock, then promote it to a constraint, which is a catalogue-only change.
    if index.is_unique() && index.backs_a_constraint() {
        statements.push(render(
            &Ddl::AlterTable(table_ref(table).and_then(|reference| {
                Ok(
                    AlterTable::new(reference).action(AlterTableAction::AddUniqueUsingIndex {
                        name: Some(ident(index.name())?),
                        index: ident(index.name())?,
                    }),
                )
            })?),
            backend,
        )?);
        operation_note.push_str(
            "; the constraint is then created `USING INDEX`, which takes the strong lock for a \
             moment rather than for the whole build",
        );
    }

    Ok(Operation::new(description, statements)
        .reversed_by([drop])
        .outside_a_transaction()
        .note(operation_note))
}

fn plan_add_foreign_key(
    table: &str,
    foreign_key: &ForeignKey,
    backend: Backend,
) -> Result<Operation> {
    let reference = table_ref(table)?;
    let description = format!(
        "reference `{}` from `{table}` as `{}`",
        foreign_key.target_table(),
        foreign_key.name()
    );
    let down = render(
        &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
            AlterTableAction::DropConstraint {
                name: ident(foreign_key.name())?,
                if_exists: true,
                cascade: false,
            },
        )),
        backend,
    )?;

    if backend != Backend::Postgres {
        return Ok(Operation::new(
            description,
            [render(
                &Ddl::AlterTable(AlterTable::new(reference).action(
                    AlterTableAction::AddConstraint(TableConstraint::ForeignKey(foreign_key_ddl(
                        foreign_key,
                        false,
                    )?)),
                )),
                backend,
            )?],
        )
        .reversed_by([down]));
    }

    // Two steps: `NOT VALID` takes `SHARE ROW EXCLUSIVE` for a moment,
    // `VALIDATE CONSTRAINT` scans under a weaker lock that does not block
    // writes.
    let statements = vec![
        render(
            &Ddl::AlterTable(AlterTable::new(reference.clone()).action(
                AlterTableAction::AddConstraint(TableConstraint::ForeignKey(foreign_key_ddl(
                    foreign_key,
                    true,
                )?)),
            )),
            backend,
        )?,
        render(
            &Ddl::AlterTable(AlterTable::new(reference).action(
                AlterTableAction::ValidateConstraint(ident(foreign_key.name())?),
            )),
            backend,
        )?,
    ];
    Ok(Operation::new(description, statements)
        .reversed_by([down])
        .note(
            "added `NOT VALID` and validated separately, so the existing rows are scanned without \
             holding `ACCESS EXCLUSIVE`",
        ))
}

/// The SQLite 12-step table rebuild.
///
/// Steps 1 and 12 — `PRAGMA foreign_keys = off/on` — are not here: the runner
/// opens its SQLite connection with foreign keys already off, because a
/// `PRAGMA foreign_keys` inside a transaction is silently ignored, and a
/// migration that silently keeps foreign keys on during a rebuild fails in a
/// way that is very hard to read. Step 11's `PRAGMA foreign_key_check` *is*
/// here, and the runner treats a non-empty result as a failure.
///
/// # The rebuild does not launder the destructive gate
///
/// A rebuild copies the table into its *new* definition, so a column the new
/// definition does not have is simply not copied — which destroys exactly as
/// much data as `ALTER TABLE … DROP COLUMN` does, and a lossy type change loses
/// exactly as much as `ALTER COLUMN … TYPE` does. The operation is therefore
/// marked [`Operation::destructive`] whenever any change it absorbs is
/// destructive, so it is emitted commented out and needs the same
/// acknowledgement a standalone drop needs. The alternative — the behaviour
/// this replaced — was that adding a `CHECK` to a table in the same migration
/// as a column drop silently applied the drop.
fn rebuild_table(table: &str, before: &Schema, after: &Schema, diff: &Diff) -> Result<Operation> {
    let backend = Backend::Sqlite;
    let old = before.table(table).ok_or_else(|| Error::Unsupported {
        backend: backend.as_str(),
        operation: format!("rebuild `{table}`"),
        help: "the table is not in the snapshot, so there is nothing to copy from".to_owned(),
    })?;
    let new = after.table(table).ok_or_else(|| Error::Unsupported {
        backend: backend.as_str(),
        operation: format!("rebuild `{table}`"),
        help: "the table is not in the entity graph, so it is being dropped rather than rebuilt"
            .to_owned(),
    })?;

    let forward = column_renames(table, diff);
    let backward: BTreeMap<&str, &str> = forward
        .iter()
        .map(|(new_name, old_name)| (*old_name, *new_name))
        .collect();

    let mut operation = Operation::new(
        format!("rebuild `{table}` (SQLite cannot alter it in place)"),
        rebuild_statements(old, new, &forward)?,
    )
    .reversed_by(rebuild_statements(new, old, &backward)?)
    .note(
        "SQLite has no `ALTER COLUMN`, so the table is recreated, copied, dropped and renamed — \
         the 12-step recipe from the SQLite manual. Steps 1 and 12 (`PRAGMA foreign_keys`) are \
         the runner's job: a pragma inside a transaction is ignored.",
    );

    let destroyed = destructive_changes_absorbed_by(table, diff);
    if !destroyed.is_empty() {
        operation = operation.destructive().note(format!(
            "the rebuild copies `{table}` into its new definition, so it destroys data: {}. The \
             data is not recoverable, and the down migration recreates the lost columns empty.",
            destroyed.join("; ")
        ));
    }
    Ok(operation)
}

/// The descriptions of the destructive changes one table's rebuild swallows.
///
/// Read from the diff rather than from the two table definitions because the
/// diff is the thing that already classified lossiness: a type change is
/// destructive only when [`Change::is_destructive`] says the conversion cannot
/// be proved safe, and re-deciding that here would be a second answer to a
/// question that already has one.
fn destructive_changes_absorbed_by(table: &str, diff: &Diff) -> Vec<String> {
    diff.changes()
        .iter()
        .filter(|change| change.table() == Some(table))
        .filter(|change| absorbed_by_a_rebuild(change) && change.is_destructive())
        .map(Change::description)
        .collect()
}

/// The column renames the diff records for one table, keyed by the *new* name,
/// because that is what the target column list iterates over.
fn column_renames<'a>(table: &str, diff: &'a Diff) -> BTreeMap<&'a str, &'a str> {
    diff.changes()
        .iter()
        .filter_map(|change| match change {
            Change::RenameColumn {
                table: owner,
                from,
                to,
            } if owner == table => Some((to.as_str(), from.as_str())),
            _ => None,
        })
        .collect()
}

fn rebuild_statements(
    from: &Table,
    to: &Table,
    renames: &BTreeMap<&str, &str>,
) -> Result<Vec<String>> {
    let backend = Backend::Sqlite;
    let scratch_name = format!("{}{REBUILD_SUFFIX}", to.name());
    let scratch = clone_table_as(to, &scratch_name);

    let mut target_columns: Vec<String> = Vec::new();
    let mut source_expressions: Vec<String> = Vec::new();
    for column in to.columns() {
        let source = renames
            .get(column.name())
            .copied()
            .filter(|name| from.column(name).is_some())
            .or_else(|| from.column(column.name()).map(Column::name));
        match source {
            Some(source) => {
                target_columns.push(emit::quote_name(column.name()));
                source_expressions.push(emit::quote_name(source));
            }
            None if column.is_auto_populated() => {}
            None => {
                let fill = column
                    .default()
                    .map_or_else(|| placeholder_for(column), ToOwned::to_owned);
                target_columns.push(emit::quote_name(column.name()));
                source_expressions.push(fill);
            }
        }
    }

    let mut statements = vec![render(
        &Ddl::CreateTable(create_table_ddl(&scratch, backend)?),
        backend,
    )?];
    // A table whose every column is new has nothing to copy; the rebuild is
    // then a create, a drop and a rename, which is still correct.
    if !target_columns.is_empty() {
        statements.push(format!(
            "INSERT INTO {} ({}) SELECT {} FROM {}",
            emit::quote_name(&scratch_name),
            target_columns.join(", "),
            source_expressions.join(", "),
            emit::quote_name(from.name())
        ));
    }
    statements.push(render(
        &Ddl::DropTable(DropTable::new([table_ref(&from.qualified_name())?])),
        backend,
    )?);
    statements.push(render(
        &Ddl::RenameTable(RenameTable::new(
            table_ref(&crate::schema::qualify(to.schema_name(), &scratch_name))?,
            ident(to.name())?,
        )),
        backend,
    )?);
    for index in to.indexes() {
        if inlined_as_a_constraint(index, backend) {
            continue;
        }
        statements.push(render(
            &Ddl::CreateIndex(create_index_ddl(
                &to.qualified_name(),
                index,
                backend,
                false,
            )?),
            backend,
        )?);
    }
    statements.push("PRAGMA foreign_key_check".to_owned());
    Ok(statements)
}

fn clone_table_as(table: &Table, name: &str) -> Table {
    let mut scratch = Table::new(name);
    if let Some(schema) = table.schema_name() {
        scratch = scratch.in_schema(schema);
    }
    for column in table.columns() {
        scratch.add_column(column.clone());
    }
    scratch.set_primary_key(table.primary_key().to_vec());
    for check in table.checks() {
        scratch.add_check(check.clone());
    }
    for foreign_key in table.foreign_keys() {
        scratch.add_foreign_key(foreign_key.clone());
    }
    // Only the indexes the `CREATE TABLE` inlines belong on the scratch table;
    // the rest are created afterwards, under their own names.
    for index in table.indexes() {
        if inlined_as_a_constraint(index, Backend::Sqlite) {
            scratch.add_index(index.clone());
        }
    }
    scratch
}

// ── conversions from the snapshot model to the DDL model ────────────────────

fn create_table_ddl(table: &Table, backend: Backend) -> Result<CreateTable> {
    // A single-column primary key on a serial column is written inline. On
    // SQLite that is not a style choice: `INTEGER PRIMARY KEY AUTOINCREMENT` is
    // the only spelling of a rowid alias, and a separate `PRIMARY KEY (id)`
    // constraint would give the table a second, useless index.
    let inline_key = match table.primary_key() {
        [only] => table
            .column(only)
            .filter(|column| {
                column
                    .data_type()
                    .is_ok_and(|data_type| data_type.is_auto_increment())
            })
            .map(|column| column.name()),
        _ => None,
    };

    let mut create = CreateTable::new(table_ref(&table.qualified_name())?);
    for column in table.columns() {
        let mut spec = column_spec(column, backend)?;
        if inline_key == Some(column.name()) {
            spec = spec.primary_key();
        }
        create = create.column(spec);
    }
    if inline_key.is_none() && !table.primary_key().is_empty() {
        create = create.constraint(TableConstraint::primary_key(
            Some(ident(&format!("{}_pkey", table.name()))?),
            idents(table.primary_key())?,
        ));
    }

    // PostgreSQL names the index a `UNIQUE` constraint creates after the
    // constraint, so the two are interchangeable. SQLite does not — it calls it
    // `sqlite_autoindex_users_1` — so a unique constraint there is emitted as a
    // separately named `CREATE UNIQUE INDEX`, which is the same guarantee under
    // a name the snapshot can round-trip.
    if backend == Backend::Postgres {
        for index in table.indexes() {
            if !(index.backs_a_constraint() && index.is_unique() && index.predicate().is_none()) {
                continue;
            }
            let columns: Vec<&str> = index
                .columns()
                .iter()
                .filter_map(IndexPart::column_name)
                .collect();
            if columns.len() == index.columns().len() {
                create = create.constraint(TableConstraint::unique(
                    Some(ident(index.name())?),
                    idents(&columns)?,
                ));
            }
        }
    }
    for check in table.checks() {
        create = create.constraint(check_ddl(check, false)?);
    }
    for foreign_key in table.foreign_keys() {
        create = create.constraint(TableConstraint::ForeignKey(foreign_key_ddl(
            foreign_key,
            false,
        )?));
    }
    if let Some(partition) = table.partitioning() {
        create = create.partition_by(partition_ddl(partition)?);
    }
    Ok(create)
}

fn column_spec(column: &Column, backend: Backend) -> Result<ColumnSpec> {
    let mut spec =
        ColumnSpec::new(ident(column.name())?, column.data_type()?).nullable(column.is_nullable());
    if let Some(default) = column.default() {
        spec = spec.default(raw(default));
    }
    if let Some(generated) = column.generation() {
        spec = spec.generated(if generated.is_stored() {
            SqlGenerated::stored(raw(generated.expression()))
        } else {
            SqlGenerated::virtual_(raw(generated.expression()))
        });
    }
    if let Some(kind) = column.identity_kind() {
        spec = spec.identity(match kind {
            crate::schema::IdentityKind::Always => moso_sql::ddl::Identity::Always,
            crate::schema::IdentityKind::ByDefault => moso_sql::ddl::Identity::ByDefault,
        });
    }
    if let Some(collation) = column.collation() {
        spec = spec.collate(ident(collation)?);
    }
    // A single-column serial primary key is written inline on both backends;
    // `create_table_ddl` skips the table constraint in that case.
    let _ = backend;
    Ok(spec)
}

fn create_index_ddl(
    table: &str,
    index: &Index,
    backend: Backend,
    concurrent: bool,
) -> Result<CreateIndex> {
    let targets: Vec<IndexTarget> = index
        .columns()
        .iter()
        .map(|part| {
            let mut target = match part.column_name() {
                Some(name) => IndexTarget::column(ident(name)?),
                None => IndexTarget::expr(raw(part.expr())),
            };
            if let Some(sort) = part.sort() {
                target = target.order(match sort {
                    Sort::Asc => Order::Asc,
                    Sort::Desc => Order::Desc,
                });
            }
            if let Some(nulls) = part.nulls_order() {
                target = target.nulls(match nulls {
                    NullsOrder::First => Nulls::First,
                    NullsOrder::Last => Nulls::Last,
                });
            }
            if let Some(ops) = part.ops() {
                target = target.operator_class(ident(ops)?);
            }
            if let Some(collation) = part.collation() {
                target = target.collate(ident(collation)?);
            }
            Ok(target)
        })
        .collect::<Result<_>>()?;

    let mut create = CreateIndex::new(ident(index.name())?, table_ref(table)?, targets);
    if index.is_unique() {
        create = create.unique();
    }
    if concurrent && backend == Backend::Postgres {
        create = create.concurrently();
    }
    if let Some(method) = index.method() {
        if backend == Backend::Postgres {
            create = create.using(index_method(method));
        } else if method != "btree" {
            return Err(Error::Unsupported {
                backend: backend.as_str(),
                operation: format!("build a `{method}` index"),
                help: "SQLite has one index type; drop `method = \"...\"` from the index \
                       attribute, or keep this entity on PostgreSQL"
                    .to_owned(),
            });
        }
    }
    if !index.included().is_empty() && backend == Backend::Postgres {
        create = create.include(idents(index.included())?);
    }
    if index.has_nulls_not_distinct() && backend == Backend::Postgres {
        create = create.nulls_not_distinct();
    }
    if let Some(predicate) = index.predicate() {
        create = create.where_(raw(predicate));
    }
    Ok(create)
}

fn drop_index_ddl(index: &Index, backend: Backend, concurrent: bool) -> DropIndex {
    // An index whose name came out of the snapshot has already been validated;
    // a hand-edited one that is not a legal identifier would have been refused
    // by `Schema::from_json`, so the fallback is unreachable in practice and
    // exists only to keep this function total.
    let name = Ident::new(index.name()).unwrap_or_else(|_| Ident::from_static("moso_bad_ident"));
    let drop = DropIndex::new(name).if_exists();
    if concurrent && backend == Backend::Postgres {
        drop.concurrently()
    } else {
        drop
    }
}

/// Whether a `CREATE TABLE` carries this index as an inline `UNIQUE`
/// constraint, in which case a separate `CREATE INDEX` would duplicate it.
fn inlined_as_a_constraint(index: &Index, backend: Backend) -> bool {
    backend == Backend::Postgres
        && index.backs_a_constraint()
        && index.is_unique()
        && index.predicate().is_none()
        && index.columns().iter().all(|part| part.is_column())
}

fn index_method(method: &str) -> IndexMethod {
    match method {
        "hash" => IndexMethod::Hash,
        "gin" => IndexMethod::Gin,
        "gist" => IndexMethod::Gist,
        "spgist" => IndexMethod::SpGist,
        "brin" => IndexMethod::Brin,
        "btree" => IndexMethod::BTree,
        other => Ident::new(other).map_or(IndexMethod::BTree, IndexMethod::Custom),
    }
}

fn foreign_key_ddl(foreign_key: &ForeignKey, not_valid: bool) -> Result<SqlForeignKey> {
    let mut ddl = SqlForeignKey::new(
        Some(ident(foreign_key.name())?),
        idents(foreign_key.columns())?,
        table_ref(foreign_key.target_table())?,
        idents(foreign_key.target_columns())?,
    );
    if let Some(action) = foreign_key.delete_action() {
        ddl = ddl.on_delete(action.to_sql_action());
    }
    if let Some(action) = foreign_key.update_action() {
        ddl = ddl.on_update(action.to_sql_action());
    }
    if foreign_key.is_deferrable() {
        ddl = ddl.deferrable(foreign_key.is_initially_deferred());
    }
    if not_valid {
        ddl = ddl.not_valid();
    }
    Ok(ddl)
}

fn check_ddl(check: &Check, not_valid: bool) -> Result<TableConstraint> {
    Ok(TableConstraint::Check {
        name: Some(ident(check.name())?),
        expr: raw(check.expression()),
        not_valid,
    })
}

fn partition_ddl(partition: &Partition) -> Result<Partitioning> {
    let strategy = match partition.strategy() {
        "list" => PartitionStrategy::List,
        "hash" => PartitionStrategy::Hash,
        _ => PartitionStrategy::Range,
    };
    Ok(Partitioning::new(strategy, idents(partition.columns())?))
}

fn comment_statement(table: &str, column: Option<&str>, comment: Option<&str>) -> Result<String> {
    let target = match column {
        Some(column) => format!(
            "COLUMN {}.{}",
            emit::quote_name(table),
            emit::quote_name(column)
        ),
        None => format!("TABLE {}", emit::quote_name(table)),
    };
    let body = comment.map_or_else(|| "NULL".to_owned(), emit::quote_literal);
    Ok(format!("COMMENT ON {target} IS {body}"))
}

fn alter_type_statement(
    table: &str,
    column: &str,
    to: &DataType,
    from: &DataType,
    backend: Backend,
) -> Result<String> {
    render(
        &Ddl::AlterTable(table_ref(table).and_then(|reference| {
            Ok(
                AlterTable::new(reference).action(AlterTableAction::AlterColumnType {
                    name: ident(column)?,
                    data_type: to.clone(),
                    using: crate::schema::using_expression(column, from, to)
                        .map(|using| raw(&using)),
                    lossy: crate::schema::is_lossy(from, to),
                }),
            )
        })?),
        backend,
    )
}

/// A commented template for an enum change `ALTER TYPE` cannot express.
///
/// Every line starts with `--`, which is what makes the block's statement list
/// empty when the file is parsed back, which is what makes
/// [`PendingDestructive::is_manual`](crate::file::PendingDestructive::is_manual)
/// true for it — and a manual block is refused even with `allow_destructive`,
/// because recording the migration as applied without running anything would be
/// a silently wrong answer.
///
/// The template is written to be finished, not merely uncommented, and it says
/// so: uncommenting a block whose every line is a comment would satisfy the gate
/// and change nothing.
fn manual_enum_template(
    before: &crate::schema::EnumType,
    after: &crate::schema::EnumType,
) -> String {
    let name = emit::quote_name(&before.qualified_name());
    let replacement = emit::quote_name(&format!("{}_new", before.qualified_name()));
    let removed: Vec<String> = before
        .labels()
        .iter()
        .filter(|label| !after.labels().contains(label))
        .map(|label| emit::quote_literal(label))
        .collect();
    let labels: Vec<String> = after
        .labels()
        .iter()
        .map(|label| emit::quote_literal(label))
        .collect();

    // A pure reorder removes nothing, so there is no row to give a new value
    // to; saying "there is nothing to backfill" is more useful than an `UPDATE`
    // with an empty `IN ()`, which is not even valid SQL.
    let backfill = if removed.is_empty() {
        "-- No label was removed, only reordered, so no existing row needs a new value.".to_owned()
    } else {
        format!(
            "-- UPDATE <table> SET <column> = <replacement label>\n\
             --   WHERE <column> IN ({});",
            removed.join(", ")
        )
    };

    format!(
        "-- Removing or reordering an enum label needs a new type and a swap, and Moso\n\
         -- cannot write it: only you know which value the rows holding a removed label\n\
         -- should get instead.\n\
         --\n\
         -- Write the statements below, filled in, WITHOUT the leading `--`, between the\n\
         -- `-- +migrate destructive` and `-- +migrate end` markers. Deleting the `--` from\n\
         -- these comment lines is not enough: a comment runs nothing, and this migration\n\
         -- would be recorded as applied having changed nothing.\n\
         --\n\
         -- CREATE TYPE {replacement} AS ENUM ({});\n\
         {backfill}\n\
         -- ALTER TABLE <table> ALTER COLUMN <column> DROP DEFAULT;\n\
         -- ALTER TABLE <table> ALTER COLUMN <column> TYPE {replacement}\n\
         --   USING <column>::text::{replacement};\n\
         -- DROP TYPE {name};\n\
         -- ALTER TYPE {replacement} RENAME TO {};\n\
         --\n\
         -- Repeat the three `ALTER TABLE` lines for every table with a {name} column,\n\
         -- and restore any default you dropped afterwards.",
        labels.join(", "),
        emit::quote_name(before.name()),
    )
}

/// A placeholder fill value, by type family, that a reviewer is meant to
/// replace. Never guessed silently: every use of this attaches a `REVIEW` note.
fn placeholder_for(column: &Column) -> String {
    let Ok(data_type) = column.data_type() else {
        return "''".to_owned();
    };
    match data_type {
        DataType::Boolean => "false".to_owned(),
        DataType::SmallInt
        | DataType::Integer
        | DataType::BigInt
        | DataType::SmallSerial
        | DataType::Serial
        | DataType::BigSerial => "0".to_owned(),
        DataType::Real | DataType::DoublePrecision | DataType::Numeric { .. } => "0".to_owned(),
        DataType::Timestamp { .. } | DataType::Date | DataType::Time { .. } => {
            "CURRENT_TIMESTAMP".to_owned()
        }
        DataType::Json | DataType::JsonB => "'{}'".to_owned(),
        DataType::Array(_) => "'{}'".to_owned(),
        _ => "''".to_owned(),
    }
}

fn render(ddl: &Ddl, backend: Backend) -> Result<String> {
    emit::render(ddl, backend)
}

/// Wraps SQL text as an expression that renders back verbatim.
///
/// `?` is doubled on the way in because [`RawExpr`] treats a bare `?` as a
/// placeholder; the renderer halves it again, so a default of `'what?'`
/// survives the round trip.
fn raw(sql: &str) -> Expr {
    Expr::Raw(RawExpr::new(sql.replace('?', "??")))
}

fn ident(name: &str) -> Result<Ident> {
    Ok(Ident::new(name)?)
}

fn idents<S: AsRef<str>>(names: &[S]) -> Result<Vec<Ident>> {
    names.iter().map(|name| ident(name.as_ref())).collect()
}

fn table_ref(qualified: &str) -> Result<TableRef> {
    Ok(match unqualify(qualified) {
        (Some(schema), name) => TableRef::qualified(ident(schema)?, ident(name)?),
        (None, name) => TableRef::new(ident(name)?),
    })
}

fn type_ref(qualified: &str) -> Result<TypeRef> {
    Ok(match unqualify(qualified) {
        (Some(schema), name) => TypeRef::qualified(ident(schema)?, ident(name)?),
        (None, name) => TypeRef::new(ident(name)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rename::DropAndAdd;
    use crate::schema::{Action, EnumType};

    fn users() -> Table {
        let mut table = Table::new("users").for_entity("User");
        table.add_column(Column::new("id", DataType::BigSerial));
        table.add_column(Column::new("email", DataType::Text));
        table.set_primary_key(["id"]);
        table
    }

    fn schema_with(table: Table) -> Schema {
        let mut schema = Schema::empty();
        schema.add_table(table);
        schema
    }

    fn plan(before: &Schema, after: &Schema, backend: Backend) -> Plan {
        let diff = Diff::compute(before, after, &DropAndAdd).expect("diffs");
        Plan::build(&diff, before, after, backend).expect("plans")
    }

    fn all_sql(plan: &Plan) -> String {
        plan.operations()
            .iter()
            .flat_map(|operation| operation.up().iter().cloned())
            .collect::<Vec<_>>()
            .join(";\n")
    }

    #[test]
    fn a_new_table_creates_and_drops() {
        let after = schema_with(users());
        let forward = plan(&Schema::empty(), &after, Backend::Postgres);
        assert_eq!(forward.operations().len(), 1);
        assert!(forward.is_reversible());
        assert!(all_sql(&forward).starts_with("CREATE TABLE \"users\""));
        assert_eq!(forward.operations()[0].down(), ["DROP TABLE \"users\""]);
    }

    #[test]
    fn a_new_index_is_concurrent_on_postgres_and_plain_on_sqlite() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_index(Index::new("idx_users_email", ["email"]));
        let after = schema_with(after_table);

        let postgres = plan(&before, &after, Backend::Postgres);
        assert!(all_sql(&postgres).contains("CREATE INDEX CONCURRENTLY"));
        assert!(postgres.requires_no_transaction());

        let sqlite = plan(&before, &after, Backend::Sqlite);
        assert!(all_sql(&sqlite).contains("CREATE INDEX \"idx_users_email\""));
        assert!(!all_sql(&sqlite).contains("CONCURRENTLY"));
        assert!(!sqlite.requires_no_transaction());
    }

    #[test]
    fn a_new_unique_constraint_uses_the_two_step_idiom() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_index(
            Index::new("users_email_key", ["email"])
                .unique()
                .backing_a_constraint(),
        );
        let postgres = plan(&before, &schema_with(after_table), Backend::Postgres);
        let sql = all_sql(&postgres);
        assert!(sql.contains("CREATE UNIQUE INDEX CONCURRENTLY"), "{sql}");
        assert!(
            sql.contains("ADD CONSTRAINT \"users_email_key\" UNIQUE USING INDEX"),
            "{sql}"
        );
    }

    #[test]
    fn a_new_foreign_key_is_not_valid_then_validated() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_foreign_key(
            ForeignKey::new("users_org_fkey", ["id"], "orgs", ["id"]).on_delete(Action::Cascade),
        );
        let postgres = plan(
            &before,
            &schema_with(after_table.clone()),
            Backend::Postgres,
        );
        let sql = all_sql(&postgres);
        assert!(sql.contains("NOT VALID"), "{sql}");
        assert!(
            sql.contains("VALIDATE CONSTRAINT \"users_org_fkey\""),
            "{sql}"
        );

        // SQLite has neither, and gets the one-step form inside a rebuild.
        let sqlite = plan(&before, &schema_with(after_table), Backend::Sqlite);
        assert!(!all_sql(&sqlite).contains("NOT VALID"));
    }

    #[test]
    fn a_not_null_column_with_no_default_is_added_in_three_steps() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_column(Column::new("locale", DataType::Text));
        let forward = plan(&before, &schema_with(after_table), Backend::Postgres);
        let operation = &forward.operations()[0];
        assert_eq!(operation.up().len(), 3, "{:?}", operation.up());
        assert!(operation.up()[0].contains("ADD COLUMN \"locale\" text"));
        assert!(!operation.up()[0].contains("NOT NULL"));
        assert!(operation.up()[1].starts_with("UPDATE \"users\" SET \"locale\""));
        assert!(operation.up()[2].contains("SET NOT NULL"));
        assert!(operation.notes()[0].contains("REVIEW"));
    }

    #[test]
    fn a_nullable_column_is_added_in_one_step() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_column(Column::new("bio", DataType::Text).nullable());
        let forward = plan(&before, &schema_with(after_table), Backend::Postgres);
        assert_eq!(forward.operations()[0].up().len(), 1);
    }

    #[test]
    fn dropping_a_column_is_destructive_and_says_the_data_is_gone() {
        let mut before_table = users();
        before_table.add_column(Column::new("legacy_id", DataType::Integer).nullable());
        let forward = plan(
            &schema_with(before_table),
            &schema_with(users()),
            Backend::Postgres,
        );
        let operation = &forward.operations()[0];
        assert!(operation.is_destructive());
        assert!(operation.up()[0].contains("DROP COLUMN \"legacy_id\""));
        assert!(operation.notes()[0].contains("not recoverable"));
        assert!(operation.is_reversible());
    }

    #[test]
    fn sqlite_rebuilds_a_table_for_a_type_change() {
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::Integer).nullable());
        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::BigInt).nullable());

        let sqlite = plan(
            &schema_with(before_table),
            &schema_with(after_table),
            Backend::Sqlite,
        );
        assert_eq!(sqlite.operations().len(), 1);
        let sql = sqlite.operations()[0].up();
        assert!(
            sql[0].contains("CREATE TABLE \"users__moso_new\""),
            "{sql:?}"
        );
        assert!(
            sql[1].starts_with("INSERT INTO \"users__moso_new\""),
            "{sql:?}"
        );
        assert!(sql[2] == "DROP TABLE \"users\"", "{sql:?}");
        assert!(sql[3].contains("RENAME TO \"users\""), "{sql:?}");
        assert_eq!(
            sql.last().map(String::as_str),
            Some("PRAGMA foreign_key_check")
        );
        assert!(sqlite.is_reversible());
    }

    #[test]
    fn several_sqlite_changes_to_one_table_collapse_into_one_rebuild() {
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::Integer).nullable());
        before_table.add_column(Column::new("legacy", DataType::Text).nullable());

        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::BigInt));

        let diff = Diff::compute(
            &schema_with(before_table.clone()),
            &schema_with(after_table.clone()),
            &DropAndAdd,
        )
        .expect("diffs");
        assert!(diff.len() >= 3, "{:?}", diff.summary());

        let sqlite = Plan::build(
            &diff,
            &schema_with(before_table),
            &schema_with(after_table),
            Backend::Sqlite,
        )
        .expect("plans");
        assert_eq!(sqlite.operations().len(), 1, "one rebuild, not three");
    }

    #[test]
    fn a_rebuild_that_drops_a_column_is_destructive() {
        // The bug this test exists for: the column drop is absorbed into the
        // rebuild the type change forces, and used to lose its gate on the way.
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::Integer).nullable());
        before_table.add_column(Column::new("legacy_id", DataType::Integer).nullable());

        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::BigInt).nullable());

        let sqlite = plan(
            &schema_with(before_table),
            &schema_with(after_table),
            Backend::Sqlite,
        );
        assert_eq!(sqlite.operations().len(), 1, "one rebuild");
        assert!(sqlite.is_destructive(), "the drop is still a drop");
        let notes = sqlite.operations()[0].notes().join("\n");
        assert!(notes.contains("drop `users.legacy_id`"), "{notes}");
        assert!(notes.contains("not recoverable"), "{notes}");
    }

    #[test]
    fn a_rebuild_that_destroys_nothing_stays_ungated() {
        // Widening `integer` to `bigint` is proven safe, so the rebuild it
        // forces is not a destructive change and must not ask for a signature.
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::Integer).nullable());
        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::BigInt).nullable());

        let sqlite = plan(
            &schema_with(before_table),
            &schema_with(after_table),
            Backend::Sqlite,
        );
        assert!(!sqlite.is_destructive());
    }

    #[test]
    fn a_rebuild_that_narrows_a_type_is_destructive() {
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::BigInt).nullable());
        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::SmallInt).nullable());

        let sqlite = plan(
            &schema_with(before_table),
            &schema_with(after_table),
            Backend::Sqlite,
        );
        assert!(
            sqlite.is_destructive(),
            "a narrowing conversion can lose data"
        );
    }

    #[test]
    fn postgres_does_not_rebuild() {
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::Integer).nullable());
        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::BigInt).nullable());
        let postgres = plan(
            &schema_with(before_table),
            &schema_with(after_table),
            Backend::Postgres,
        );
        assert!(all_sql(&postgres).contains("ALTER COLUMN \"n\" TYPE bigint"));
        assert!(!all_sql(&postgres).contains(REBUILD_SUFFIX));
    }

    #[test]
    fn enum_values_are_appended_one_statement_each() {
        let mut before = Schema::empty();
        before.add_enum(EnumType::new("user_role", ["admin", "member"]));
        let mut after = Schema::empty();
        after.add_enum(EnumType::new(
            "user_role",
            ["admin", "member", "auditor", "owner"],
        ));

        let forward = plan(&before, &after, Backend::Postgres);
        let operation = &forward.operations()[0];
        assert_eq!(operation.up().len(), 2);
        assert!(operation.up()[0].contains("ADD VALUE IF NOT EXISTS 'auditor' AFTER 'member'"));
        assert!(operation.up()[1].contains("ADD VALUE IF NOT EXISTS 'owner' AFTER 'auditor'"));
        assert!(forward.requires_no_transaction());
        assert!(!operation.is_reversible());
    }

    #[test]
    fn a_removed_enum_label_produces_a_commented_template() {
        let mut before = Schema::empty();
        before.add_enum(EnumType::new("user_role", ["admin", "member"]));
        let mut after = Schema::empty();
        after.add_enum(EnumType::new("user_role", ["admin"]));

        let forward = plan(&before, &after, Backend::Postgres);
        let operation = &forward.operations()[0];
        assert!(operation.is_destructive());
        let template = &operation.up()[0];
        assert!(template.contains("CREATE TYPE"), "{template}");
        assert!(
            template.lines().all(|line| line.starts_with("--")),
            "{template}"
        );
        // Every identifier it names is quoted, so the reader can paste it.
        assert!(template.contains("\"user_role_new\""), "{template}");
        assert!(!template.contains("\"user_role\"_new"), "{template}");
        assert!(
            template.contains("WITHOUT the leading `--`"),
            "the template says how to finish it: {template}"
        );
    }

    #[test]
    fn a_reordered_enum_template_does_not_ask_for_a_backfill() {
        let mut before = Schema::empty();
        before.add_enum(EnumType::new("user_role", ["admin", "member"]));
        let mut after = Schema::empty();
        after.add_enum(EnumType::new("user_role", ["member", "admin"]));

        let forward = plan(&before, &after, Backend::Postgres);
        let template = &forward.operations()[0].up()[0];
        assert!(template.contains("only reordered"), "{template}");
        assert!(!template.contains("UPDATE"), "{template}");
    }

    #[test]
    fn a_gin_index_on_sqlite_names_the_alternative() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_index(Index::new("idx_users_doc", ["email"]).using("gin"));
        let diff =
            Diff::compute(&before, &schema_with(after_table.clone()), &DropAndAdd).expect("diffs");
        let error = Plan::build(&diff, &before, &schema_with(after_table), Backend::Sqlite)
            .expect_err("no gin on sqlite");
        assert!(error.to_string().contains("one index type"), "{error}");
    }

    #[test]
    fn a_table_rename_reverses_exactly() {
        let before = schema_with(users());
        let mut renamed = Table::new("accounts").for_entity("User");
        renamed.add_column(Column::new("id", DataType::BigSerial));
        renamed.add_column(Column::new("email", DataType::Text));
        renamed.set_primary_key(["id"]);

        let oracle = crate::rename::Scripted::parse(["users:accounts"]).expect("parses");
        let after = schema_with(renamed);
        let diff = Diff::compute(&before, &after, &oracle).expect("diffs");
        let forward = Plan::build(&diff, &before, &after, Backend::Postgres).expect("plans");
        assert_eq!(
            forward.operations()[0].up(),
            ["ALTER TABLE \"users\" RENAME TO \"accounts\""]
        );
        assert_eq!(
            forward.operations()[0].down(),
            ["ALTER TABLE \"accounts\" RENAME TO \"users\""]
        );
    }

    #[test]
    fn defaults_containing_a_question_mark_survive() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_column(Column::new("greeting", DataType::Text).with_default("'what?'"));
        let forward = plan(&before, &schema_with(after_table), Backend::Postgres);
        assert!(
            all_sql(&forward).contains("DEFAULT 'what?'"),
            "{}",
            all_sql(&forward)
        );
    }

    #[test]
    fn every_generated_statement_is_a_single_statement() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_column(Column::new("locale", DataType::Text));
        after_table.add_index(Index::new("idx_users_locale", ["locale"]));
        let forward = plan(&before, &schema_with(after_table), Backend::Postgres);
        for operation in forward.operations() {
            for statement in operation.up() {
                assert!(
                    !statement.trim_end().ends_with(';'),
                    "statements carry no terminator: {statement}"
                );
            }
        }
    }
}
