//! Diffing two schemas.
//!
//! The differ takes the committed snapshot and the schema the entities describe
//! and produces an ordered list of [`Change`]s. It knows nothing about SQL:
//! that is [`crate::plan`]'s job. Keeping them apart is what lets one differ
//! serve two dialects whose SQL for the same change is completely different.
//!
//! # The ordering rule
//!
//! Changes come out in an order that is valid to apply as written: create
//! before use, drop after everything that referenced it. Within a category the
//! order is by name, so that two runs of the generator on the same input
//! produce the same file — which is the whole of the idempotence acceptance
//! criterion.
//!
//! ```
//! use moso_migrate::diff::Diff;
//! use moso_migrate::rename::DropAndAdd;
//! use moso_migrate::schema::{Column, Schema, Table};
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
//! assert_eq!(diff.len(), 1);
//! assert!(!diff.is_destructive());
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::collections::{BTreeMap, BTreeSet};

use crate::error::Result;
use crate::rename::{Oracle, RenameAnswer, RenameQuestion};
use crate::schema::{Check, Column, EnumType, ForeignKey, Index, Schema, Table};

/// One schema change.
///
/// ```
/// use moso_migrate::diff::Change;
/// use moso_migrate::schema::Table;
///
/// let drop = Change::DropTable(Box::new(Table::new("legacy")));
/// assert!(drop.is_destructive());
/// assert_eq!(drop.description(), "drop the table `legacy`");
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Change {
    /// A named schema has to exist.
    CreateSchema(String),
    /// An extension has to be installed.
    CreateExtension(String),
    /// A new enum type.
    CreateEnum(EnumType),
    /// Labels appended to an existing enum type.
    AddEnumValues {
        /// The type's qualified name.
        name: String,
        /// The labels to append, in order.
        values: Vec<String>,
        /// The label each new one goes after, so the order is preserved.
        after: String,
    },
    /// An enum type whose labels changed in a way `ADD VALUE` cannot express:
    /// one was removed, or the order changed.
    RewriteEnum {
        /// The type as the database has it.
        before: EnumType,
        /// The type as the entities want it.
        after: EnumType,
    },
    /// An enum type that no entity uses any more.
    DropEnum(EnumType),
    /// A whole new table.
    CreateTable(Box<Table>),
    /// A table no entity maps to any more.
    DropTable(Box<Table>),
    /// A table under a new name.
    RenameTable {
        /// The old qualified name.
        from: String,
        /// The new unqualified name.
        to: String,
    },
    /// A new column.
    AddColumn {
        /// The table's qualified name.
        table: String,
        /// The column.
        column: Box<Column>,
        /// Whether the table it is being added to already exists with rows, so
        /// a `NOT NULL` column needs a fill value. Always `true` for an
        /// `ALTER TABLE`; the differ never emits this for a new table.
        needs_backfill: bool,
    },
    /// A column no field maps to any more.
    DropColumn {
        /// The table.
        table: String,
        /// The column, kept whole so the down migration can put it back.
        column: Box<Column>,
    },
    /// A column under a new name.
    RenameColumn {
        /// The table.
        table: String,
        /// The old name.
        from: String,
        /// The new name.
        to: String,
    },
    /// A column's type changed.
    AlterColumnType {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// The type it had.
        from: String,
        /// The type it wants.
        to: String,
        /// Whether the conversion can lose data.
        lossy: bool,
    },
    /// A column became `NOT NULL`.
    SetNotNull {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// Whether existing rows may contain `NULL`, so a backfill is needed
        /// first.
        needs_backfill: bool,
    },
    /// A column became nullable.
    DropNotNull {
        /// The table.
        table: String,
        /// The column.
        column: String,
    },
    /// A column's default changed or was added.
    SetDefault {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// The new default, as SQL text.
        default: String,
        /// The old default, for the down migration.
        previous: Option<String>,
    },
    /// A column's default was removed.
    DropDefault {
        /// The table.
        table: String,
        /// The column.
        column: String,
        /// The old default, for the down migration.
        previous: String,
    },
    /// A table's primary key changed.
    SetPrimaryKey {
        /// The table.
        table: String,
        /// The new key columns.
        columns: Vec<String>,
        /// The old key columns, for the down migration.
        previous: Vec<String>,
    },
    /// A new index.
    CreateIndex {
        /// The table.
        table: String,
        /// The index.
        index: Box<Index>,
    },
    /// An index that is gone.
    DropIndex {
        /// The table.
        table: String,
        /// The index, kept whole so the down migration can rebuild it.
        index: Box<Index>,
    },
    /// An index under a new name.
    RenameIndex {
        /// The table.
        table: String,
        /// The old name.
        from: String,
        /// The new name.
        to: String,
    },
    /// A new foreign key.
    AddForeignKey {
        /// The table.
        table: String,
        /// The constraint.
        foreign_key: Box<ForeignKey>,
    },
    /// A foreign key that is gone.
    DropForeignKey {
        /// The table.
        table: String,
        /// The constraint.
        foreign_key: Box<ForeignKey>,
    },
    /// A new check constraint.
    AddCheck {
        /// The table.
        table: String,
        /// The constraint.
        check: Check,
    },
    /// A check constraint that is gone.
    DropCheck {
        /// The table.
        table: String,
        /// The constraint.
        check: Check,
    },
    /// A table or column comment changed.
    SetComment {
        /// The table.
        table: String,
        /// The column, or `None` for the table itself.
        column: Option<String>,
        /// The new comment, or `None` to remove it.
        comment: Option<String>,
        /// The old comment, for the down migration.
        previous: Option<String>,
    },
}

impl Change {
    /// Whether applying this change can destroy data or remove a guarantee.
    ///
    /// This is the predicate the safety policy turns on: a destructive change
    /// is emitted commented out, and `moso db migrate` refuses until a human
    /// has acknowledged it.
    ///
    /// ```
    /// use moso_migrate::diff::Change;
    /// use moso_migrate::schema::Table;
    ///
    /// assert!(Change::DropTable(Box::new(Table::new("t"))).is_destructive());
    /// assert!(!Change::CreateSchema("app".to_owned()).is_destructive());
    /// ```
    #[must_use]
    pub const fn is_destructive(&self) -> bool {
        match self {
            Self::DropTable(_)
            | Self::DropColumn { .. }
            | Self::DropEnum(_)
            | Self::RewriteEnum { .. } => true,
            Self::AlterColumnType { lossy, .. } => *lossy,
            _ => false,
        }
    }

    /// Whether the change must run outside a transaction.
    ///
    /// `CREATE INDEX CONCURRENTLY` and `ALTER TYPE … ADD VALUE` cannot run
    /// inside one on PostgreSQL. A migration that contains either is marked
    /// non-transactional, which in turn is what makes a partial failure leave
    /// the version dirty rather than silently half-applied.
    ///
    /// ```
    /// use moso_migrate::diff::Change;
    ///
    /// let add = Change::AddEnumValues {
    ///     name: "user_role".to_owned(),
    ///     values: vec!["auditor".to_owned()],
    ///     after: "member".to_owned(),
    /// };
    /// assert!(add.requires_no_transaction());
    /// ```
    #[must_use]
    pub const fn requires_no_transaction(&self) -> bool {
        matches!(self, Self::AddEnumValues { .. } | Self::CreateIndex { .. })
    }

    /// The change in one line, for a migration file's comment and for
    /// `moso db status`.
    ///
    /// ```
    /// use moso_migrate::diff::Change;
    ///
    /// let change = Change::RenameColumn {
    ///     table: "users".to_owned(),
    ///     from: "name".to_owned(),
    ///     to: "full_name".to_owned(),
    /// };
    /// assert_eq!(change.description(), "rename `users.name` to `full_name`");
    /// ```
    #[must_use]
    pub fn description(&self) -> String {
        match self {
            Self::CreateSchema(name) => format!("create the schema `{name}`"),
            Self::CreateExtension(name) => format!("create the extension `{name}`"),
            Self::CreateEnum(enum_type) => {
                format!("create the type `{}`", enum_type.qualified_name())
            }
            Self::AddEnumValues { name, values, .. } => format!(
                "add {} to `{name}`",
                values
                    .iter()
                    .map(|value| format!("`{value}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Self::RewriteEnum { after, .. } => format!(
                "rewrite the type `{}` — a label was removed or reordered",
                after.qualified_name()
            ),
            Self::DropEnum(enum_type) => {
                format!("drop the type `{}`", enum_type.qualified_name())
            }
            Self::CreateTable(table) => format!("create the table `{}`", table.qualified_name()),
            Self::DropTable(table) => format!("drop the table `{}`", table.qualified_name()),
            Self::RenameTable { from, to } => format!("rename the table `{from}` to `{to}`"),
            Self::AddColumn { table, column, .. } => {
                format!("add `{table}.{}`", column.name())
            }
            Self::DropColumn { table, column } => format!("drop `{table}.{}`", column.name()),
            Self::RenameColumn { table, from, to } => {
                format!("rename `{table}.{from}` to `{to}`")
            }
            Self::AlterColumnType {
                table,
                column,
                from,
                to,
                ..
            } => format!("change `{table}.{column}` from {from} to {to}"),
            Self::SetNotNull { table, column, .. } => format!("make `{table}.{column}` NOT NULL"),
            Self::DropNotNull { table, column } => format!("make `{table}.{column}` nullable"),
            Self::SetDefault {
                table,
                column,
                default,
                ..
            } => format!("default `{table}.{column}` to {default}"),
            Self::DropDefault { table, column, .. } => {
                format!("drop the default on `{table}.{column}`")
            }
            Self::SetPrimaryKey { table, columns, .. } => {
                format!(
                    "set the primary key of `{table}` to ({})",
                    columns.join(", ")
                )
            }
            Self::CreateIndex { table, index } => {
                format!("index `{table}` as `{}`", index.name())
            }
            Self::DropIndex { table, index } => {
                format!("drop the index `{}` on `{table}`", index.name())
            }
            Self::RenameIndex { from, to, .. } => {
                format!("rename the index `{from}` to `{to}`")
            }
            Self::AddForeignKey { table, foreign_key } => format!(
                "reference `{}` from `{table}` as `{}`",
                foreign_key.target_table(),
                foreign_key.name()
            ),
            Self::DropForeignKey { table, foreign_key } => {
                format!("drop the foreign key `{}` on `{table}`", foreign_key.name())
            }
            Self::AddCheck { table, check } => {
                format!("check `{}` on `{table}`", check.name())
            }
            Self::DropCheck { table, check } => {
                format!("drop the check `{}` on `{table}`", check.name())
            }
            Self::SetComment {
                table,
                column: Some(column),
                ..
            } => format!("comment on `{table}.{column}`"),
            Self::SetComment { table, .. } => format!("comment on `{table}`"),
        }
    }

    /// The table this change is about, when there is one.
    ///
    /// ```
    /// use moso_migrate::diff::Change;
    ///
    /// assert_eq!(Change::CreateSchema("app".to_owned()).table(), None);
    /// ```
    #[must_use]
    pub fn table(&self) -> Option<&str> {
        match self {
            Self::CreateTable(table) | Self::DropTable(table) => Some(table.name()),
            Self::RenameTable { from, .. } => Some(from),
            Self::AddColumn { table, .. }
            | Self::DropColumn { table, .. }
            | Self::RenameColumn { table, .. }
            | Self::AlterColumnType { table, .. }
            | Self::SetNotNull { table, .. }
            | Self::DropNotNull { table, .. }
            | Self::SetDefault { table, .. }
            | Self::DropDefault { table, .. }
            | Self::SetPrimaryKey { table, .. }
            | Self::CreateIndex { table, .. }
            | Self::DropIndex { table, .. }
            | Self::RenameIndex { table, .. }
            | Self::AddForeignKey { table, .. }
            | Self::DropForeignKey { table, .. }
            | Self::AddCheck { table, .. }
            | Self::DropCheck { table, .. }
            | Self::SetComment { table, .. } => Some(table),
            _ => None,
        }
    }

    /// The rank that decides where the change goes in the file.
    ///
    /// Lower runs first. Creations before uses, drops after everything that
    /// might have referred to them.
    const fn order(&self) -> u8 {
        match self {
            Self::CreateSchema(_) => 0,
            Self::CreateExtension(_) => 1,
            Self::CreateEnum(_) => 2,
            Self::AddEnumValues { .. } => 3,
            Self::RewriteEnum { .. } => 4,
            Self::RenameTable { .. } => 5,
            Self::CreateTable(_) => 6,
            Self::RenameColumn { .. } => 7,
            Self::RenameIndex { .. } => 8,
            Self::AddColumn { .. } => 9,
            Self::AlterColumnType { .. } => 10,
            Self::SetDefault { .. } | Self::DropDefault { .. } => 11,
            Self::SetNotNull { .. } | Self::DropNotNull { .. } => 12,
            Self::DropCheck { .. } | Self::DropForeignKey { .. } => 13,
            Self::DropIndex { .. } => 14,
            Self::SetPrimaryKey { .. } => 15,
            Self::CreateIndex { .. } => 16,
            Self::AddCheck { .. } => 17,
            Self::AddForeignKey { .. } => 18,
            Self::SetComment { .. } => 19,
            Self::DropColumn { .. } => 20,
            Self::DropTable(_) => 21,
            Self::DropEnum(_) => 22,
        }
    }

    /// The tie-break within a rank, so two runs order identically.
    fn sort_key(&self) -> String {
        format!("{}/{}", self.table().unwrap_or(""), self.description())
    }
}

/// An ordered set of changes.
///
/// ```
/// use moso_migrate::diff::Diff;
///
/// assert!(Diff::empty().is_empty());
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diff {
    changes: Vec<Change>,
}

impl Diff {
    /// No changes.
    ///
    /// ```
    /// assert_eq!(moso_migrate::diff::Diff::empty().len(), 0);
    /// ```
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            changes: Vec::new(),
        }
    }

    /// Diffs `before` into `after`, consulting `oracle` about renames.
    ///
    /// # Errors
    ///
    /// Whatever the oracle returns — [`Error::NeedsAnswer`](crate::Error::NeedsAnswer)
    /// for the non-interactive ones — plus
    /// [`Error::Snapshot`](crate::Error::Snapshot) if a snapshot names a type
    /// this build cannot parse.
    ///
    /// ```
    /// use moso_migrate::diff::Diff;
    /// use moso_migrate::rename::DropAndAdd;
    /// use moso_migrate::Schema;
    ///
    /// let schema = Schema::empty();
    /// assert!(Diff::compute(&schema, &schema, &DropAndAdd)?.is_empty());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn compute(before: &Schema, after: &Schema, oracle: &dyn Oracle) -> Result<Self> {
        let mut changes = Vec::new();

        for schema_name in after.schemas() {
            if !before.schemas().any(|existing| existing == schema_name) {
                changes.push(Change::CreateSchema(schema_name.to_owned()));
            }
        }
        for extension in after.extensions() {
            if !before.extensions().any(|existing| existing == extension) {
                changes.push(Change::CreateExtension(extension.to_owned()));
            }
        }

        diff_enums(before, after, &mut changes);
        let table_renames = diff_tables(before, after, oracle, &mut changes)?;

        for (before_name, after_name) in &table_renames {
            let before_table = before.table(before_name).expect("named by the rename map");
            let after_table = after.table(after_name).expect("named by the rename map");
            diff_table(after_name, before_table, after_table, oracle, &mut changes)?;
        }
        for after_table in after.tables() {
            let key = after_table.qualified_name();
            if table_renames.values().any(|renamed| renamed == &key) {
                continue;
            }
            if let Some(before_table) = before.table(&key) {
                diff_table(&key, before_table, after_table, oracle, &mut changes)?;
            }
        }

        // A new table has to be created after the tables its foreign keys
        // point at, and dropped before them. Name order would put `post_tags`
        // before `posts` and the migration would fail on its first run.
        let creation_rank: BTreeMap<&str, usize> = after
            .creation_order()
            .iter()
            .enumerate()
            .map(|(rank, table)| (table.name(), rank))
            .collect();
        let removal_rank: BTreeMap<&str, usize> = before
            .creation_order()
            .iter()
            .rev()
            .enumerate()
            .map(|(rank, table)| (table.name(), rank))
            .collect();
        let dependency_rank = |change: &Change| -> usize {
            match change {
                Change::CreateTable(table) => creation_rank
                    .get(table.name())
                    .copied()
                    .unwrap_or(usize::MAX),
                Change::DropTable(table) => removal_rank
                    .get(table.name())
                    .copied()
                    .unwrap_or(usize::MAX),
                _ => 0,
            }
        };

        changes.sort_by(|a, b| {
            a.order()
                .cmp(&b.order())
                .then_with(|| dependency_rank(a).cmp(&dependency_rank(b)))
                .then_with(|| a.sort_key().cmp(&b.sort_key()))
        });
        Ok(Self { changes })
    }

    /// The changes, in the order they must be applied.
    ///
    /// ```
    /// assert!(moso_migrate::diff::Diff::empty().changes().is_empty());
    /// ```
    #[must_use]
    pub fn changes(&self) -> &[Change] {
        &self.changes
    }

    /// How many changes there are.
    ///
    /// ```
    /// assert_eq!(moso_migrate::diff::Diff::empty().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.changes.len()
    }

    /// Whether there is nothing to do — the answer `make-migration` prints as
    /// "no changes", and the assertion the idempotence test makes.
    ///
    /// ```
    /// assert!(moso_migrate::diff::Diff::empty().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Whether any change can destroy data.
    ///
    /// ```
    /// assert!(!moso_migrate::diff::Diff::empty().is_destructive());
    /// ```
    #[must_use]
    pub fn is_destructive(&self) -> bool {
        self.changes.iter().any(Change::is_destructive)
    }

    /// The destructive changes, for the warning header.
    ///
    /// ```
    /// assert!(moso_migrate::diff::Diff::empty().destructive().is_empty());
    /// ```
    #[must_use]
    pub fn destructive(&self) -> Vec<&Change> {
        self.changes
            .iter()
            .filter(|change| change.is_destructive())
            .collect()
    }

    /// Whether any change forces the whole migration outside a transaction.
    ///
    /// ```
    /// assert!(!moso_migrate::diff::Diff::empty().requires_no_transaction());
    /// ```
    #[must_use]
    pub fn requires_no_transaction(&self) -> bool {
        self.changes.iter().any(Change::requires_no_transaction)
    }

    /// A one-line-per-change summary, for the migration file's header.
    ///
    /// ```
    /// assert_eq!(moso_migrate::diff::Diff::empty().summary(), Vec::<String>::new());
    /// ```
    #[must_use]
    pub fn summary(&self) -> Vec<String> {
        self.changes.iter().map(Change::description).collect()
    }

    /// A name for a migration that contains these changes, used when the
    /// developer does not supply one.
    ///
    /// ```
    /// use moso_migrate::diff::{Change, Diff};
    ///
    /// assert_eq!(Diff::empty().suggested_name(), "no_changes");
    /// ```
    #[must_use]
    pub fn suggested_name(&self) -> String {
        let Some(first) = self.changes.first() else {
            return "no_changes".to_owned();
        };
        let base = match first {
            Change::CreateTable(table) => format!("create_{}", table.name()),
            Change::DropTable(table) => format!("drop_{}", table.name()),
            Change::AddColumn { table, column, .. } => format!("add_{table}_{}", column.name()),
            Change::DropColumn { table, column } => format!("drop_{table}_{}", column.name()),
            Change::RenameTable { to, .. } => format!("rename_to_{to}"),
            Change::RenameColumn { table, to, .. } => format!("rename_{table}_{to}"),
            Change::CreateIndex { index, .. } => format!("index_{}", index.name()),
            Change::CreateEnum(enum_type) => format!("create_type_{}", enum_type.name()),
            other => other.table().map_or_else(
                || "schema_change".to_owned(),
                |table| format!("alter_{table}"),
            ),
        };
        let suffix = if self.changes.len() > 1 {
            format!("_and_{}_more", self.changes.len() - 1)
        } else {
            String::new()
        };
        crate::version::MigrationId::new(crate::Version::now(), &format!("{base}{suffix}"))
            .name()
            .to_owned()
    }
}

fn diff_enums(before: &Schema, after: &Schema, changes: &mut Vec<Change>) {
    for wanted in after.enums() {
        let key = wanted.qualified_name();
        match before.enum_type(&key) {
            None => changes.push(Change::CreateEnum(wanted.clone())),
            Some(existing) if existing == wanted => {}
            Some(existing) if wanted.is_additive_over(existing) => {
                let values = wanted.labels_missing_from(existing);
                let after_label = existing
                    .labels()
                    .last()
                    .cloned()
                    .unwrap_or_else(|| values[0].clone());
                changes.push(Change::AddEnumValues {
                    name: key,
                    values,
                    after: after_label,
                });
            }
            Some(existing) => changes.push(Change::RewriteEnum {
                before: existing.clone(),
                after: wanted.clone(),
            }),
        }
    }
    for existing in before.enums() {
        if after.enum_type(&existing.qualified_name()).is_none() {
            changes.push(Change::DropEnum(existing.clone()));
        }
    }
}

/// Matches dropped tables against added ones, asking the oracle about each
/// candidate, and returns the accepted renames as `before -> after`.
fn diff_tables(
    before: &Schema,
    after: &Schema,
    oracle: &dyn Oracle,
    changes: &mut Vec<Change>,
) -> Result<BTreeMap<String, String>> {
    let removed: Vec<&Table> = before
        .tables()
        .filter(|table| after.table(&table.qualified_name()).is_none())
        .collect();
    let added: Vec<&Table> = after
        .tables()
        .filter(|table| before.table(&table.qualified_name()).is_none())
        .collect();

    let mut renames: BTreeMap<String, String> = BTreeMap::new();
    let mut claimed_before: BTreeSet<String> = BTreeSet::new();
    let mut claimed_after: BTreeSet<String> = BTreeSet::new();

    for gone in &removed {
        for new in &added {
            if claimed_after.contains(&new.qualified_name())
                || claimed_before.contains(&gone.qualified_name())
            {
                continue;
            }
            if !looks_like_the_same_table(gone, new) {
                continue;
            }
            let question = RenameQuestion::table(gone.qualified_name(), new.name());
            if oracle.answer(&question)? == RenameAnswer::Rename {
                claimed_before.insert(gone.qualified_name());
                claimed_after.insert(new.qualified_name());
                changes.push(Change::RenameTable {
                    from: gone.qualified_name(),
                    to: new.name().to_owned(),
                });
                renames.insert(gone.qualified_name(), new.qualified_name());
            }
        }
    }

    for gone in removed {
        if !claimed_before.contains(&gone.qualified_name()) {
            changes.push(Change::DropTable(Box::new(gone.clone())));
        }
    }
    for new in added {
        if !claimed_after.contains(&new.qualified_name()) {
            changes.push(Change::CreateTable(Box::new(new.clone())));
        }
    }
    Ok(renames)
}

/// Whether two tables are similar enough that a rename is plausible.
///
/// The test is the entity name — a renamed entity keeps its Rust type only if
/// the *table* was renamed and not the type — or a substantial overlap in
/// column names. Asking about every dropped/added pair would turn a migration
/// that creates ten tables and drops one into eleven questions.
fn looks_like_the_same_table(before: &Table, after: &Table) -> bool {
    if before.entity().is_some() && before.entity() == after.entity() {
        return true;
    }
    let before_columns: BTreeSet<&str> = before.columns().iter().map(Column::name).collect();
    let after_columns: BTreeSet<&str> = after.columns().iter().map(Column::name).collect();
    if before_columns.is_empty() || after_columns.is_empty() {
        return false;
    }
    let shared = before_columns.intersection(&after_columns).count();
    let largest = before_columns.len().max(after_columns.len());
    shared * 2 >= largest
}

fn diff_table(
    key: &str,
    before: &Table,
    after: &Table,
    oracle: &dyn Oracle,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let renames = diff_columns(key, before, after, oracle, changes)?;

    for wanted in after.columns() {
        let previous_name = renames
            .iter()
            .find(|(_, to)| to.as_str() == wanted.name())
            .map(|(from, _)| from.as_str())
            .unwrap_or_else(|| wanted.name());
        let Some(existing) = before.column(previous_name) else {
            continue;
        };
        diff_column(key, existing, wanted, changes);
    }

    if before.primary_key() != after.primary_key() {
        changes.push(Change::SetPrimaryKey {
            table: key.to_owned(),
            columns: after.primary_key().to_vec(),
            previous: before.primary_key().to_vec(),
        });
    }

    diff_indexes(key, before, after, oracle, changes)?;
    diff_foreign_keys(key, before, after, changes);
    diff_checks(key, before, after, changes);

    if before.comment() != after.comment() {
        changes.push(Change::SetComment {
            table: key.to_owned(),
            column: None,
            comment: after.comment().map(ToOwned::to_owned),
            previous: before.comment().map(ToOwned::to_owned),
        });
    }
    Ok(())
}

fn diff_columns(
    key: &str,
    before: &Table,
    after: &Table,
    oracle: &dyn Oracle,
    changes: &mut Vec<Change>,
) -> Result<Vec<(String, String)>> {
    let removed: Vec<&Column> = before
        .columns()
        .iter()
        .filter(|column| after.column(column.name()).is_none())
        .collect();
    let added: Vec<&Column> = after
        .columns()
        .iter()
        .filter(|column| before.column(column.name()).is_none())
        .collect();

    let mut renames: Vec<(String, String)> = Vec::new();
    let mut claimed_before: BTreeSet<&str> = BTreeSet::new();
    let mut claimed_after: BTreeSet<&str> = BTreeSet::new();

    // Two passes: an exact match on everything-but-the-name first, so that a
    // column that was genuinely renamed is not stolen by a weaker candidate.
    for strict in [true, false] {
        for gone in &removed {
            if claimed_before.contains(gone.name()) {
                continue;
            }
            for new in &added {
                if claimed_after.contains(new.name()) {
                    continue;
                }
                let plausible = if strict {
                    gone.matches_ignoring_name(new)
                } else {
                    gone.field().is_some() && gone.field() == new.field()
                };
                if !plausible {
                    continue;
                }
                let question = RenameQuestion::column(key, gone.name(), new.name());
                if oracle.answer(&question)? == RenameAnswer::Rename {
                    claimed_before.insert(gone.name());
                    claimed_after.insert(new.name());
                    renames.push((gone.name().to_owned(), new.name().to_owned()));
                    changes.push(Change::RenameColumn {
                        table: key.to_owned(),
                        from: gone.name().to_owned(),
                        to: new.name().to_owned(),
                    });
                }
            }
        }
    }

    for gone in removed {
        if !claimed_before.contains(gone.name()) {
            changes.push(Change::DropColumn {
                table: key.to_owned(),
                column: Box::new(gone.clone()),
            });
        }
    }
    for new in added {
        if !claimed_after.contains(new.name()) {
            changes.push(Change::AddColumn {
                table: key.to_owned(),
                column: Box::new(new.clone()),
                needs_backfill: new.needs_a_fill_value(),
            });
        }
    }
    Ok(renames)
}

fn diff_column(table: &str, before: &Column, after: &Column, changes: &mut Vec<Change>) {
    if before.type_name() != after.type_name() {
        let lossy = match (before.data_type(), after.data_type()) {
            (Ok(from), Ok(to)) => crate::schema::is_lossy(&from, &to),
            // An unparseable type on either side means we cannot prove the
            // conversion safe, and unproven means acknowledged.
            _ => true,
        };
        changes.push(Change::AlterColumnType {
            table: table.to_owned(),
            column: after.name().to_owned(),
            from: before.type_name().to_owned(),
            to: after.type_name().to_owned(),
            lossy,
        });
    }
    match (before.is_nullable(), after.is_nullable()) {
        (true, false) => changes.push(Change::SetNotNull {
            table: table.to_owned(),
            column: after.name().to_owned(),
            needs_backfill: after.default().is_none(),
        }),
        (false, true) => changes.push(Change::DropNotNull {
            table: table.to_owned(),
            column: after.name().to_owned(),
        }),
        _ => {}
    }
    match (before.default(), after.default()) {
        (previous, Some(default)) if previous != Some(default) => {
            changes.push(Change::SetDefault {
                table: table.to_owned(),
                column: after.name().to_owned(),
                default: default.to_owned(),
                previous: previous.map(ToOwned::to_owned),
            });
        }
        (Some(previous), None) => changes.push(Change::DropDefault {
            table: table.to_owned(),
            column: after.name().to_owned(),
            previous: previous.to_owned(),
        }),
        _ => {}
    }
    if before.comment() != after.comment() {
        changes.push(Change::SetComment {
            table: table.to_owned(),
            column: Some(after.name().to_owned()),
            comment: after.comment().map(ToOwned::to_owned),
            previous: before.comment().map(ToOwned::to_owned),
        });
    }
}

fn diff_indexes(
    key: &str,
    before: &Table,
    after: &Table,
    oracle: &dyn Oracle,
    changes: &mut Vec<Change>,
) -> Result<()> {
    let removed: Vec<&Index> = before
        .indexes()
        .filter(|index| after.index(index.name()).is_none())
        .collect();
    let added: Vec<&Index> = after
        .indexes()
        .filter(|index| before.index(index.name()).is_none())
        .collect();

    let mut claimed_before: BTreeSet<&str> = BTreeSet::new();
    let mut claimed_after: BTreeSet<&str> = BTreeSet::new();

    for gone in &removed {
        for new in &added {
            if claimed_before.contains(gone.name()) || claimed_after.contains(new.name()) {
                continue;
            }
            if !gone.matches_ignoring_name(new) {
                continue;
            }
            let question = RenameQuestion::index(key, gone.name(), new.name());
            if oracle.answer(&question)? == RenameAnswer::Rename {
                claimed_before.insert(gone.name());
                claimed_after.insert(new.name());
                changes.push(Change::RenameIndex {
                    table: key.to_owned(),
                    from: gone.name().to_owned(),
                    to: new.name().to_owned(),
                });
            }
        }
    }

    for gone in removed {
        if !claimed_before.contains(gone.name()) {
            changes.push(Change::DropIndex {
                table: key.to_owned(),
                index: Box::new(gone.clone()),
            });
        }
    }
    for new in added {
        if !claimed_after.contains(new.name()) {
            changes.push(Change::CreateIndex {
                table: key.to_owned(),
                index: Box::new(new.clone()),
            });
        }
    }
    // A changed index is a drop and a rebuild: there is no `ALTER INDEX` that
    // changes the key columns.
    //
    // The comparison is `matches_ignoring_name`, not `!=`, because the names
    // are equal here by construction and because whether a *constraint* owns
    // the index is a representation detail rather than a schema difference:
    // PostgreSQL implements `UNIQUE (email)` as a unique index and SQLite as a
    // separate one, and reporting that as a change would make every SQLite
    // schema look permanently drifted.
    for wanted in after.indexes() {
        if let Some(existing) = before.index(wanted.name())
            && !existing.matches_ignoring_name(wanted)
        {
            changes.push(Change::DropIndex {
                table: key.to_owned(),
                index: Box::new(existing.clone()),
            });
            changes.push(Change::CreateIndex {
                table: key.to_owned(),
                index: Box::new(wanted.clone()),
            });
        }
    }
    Ok(())
}

fn diff_foreign_keys(key: &str, before: &Table, after: &Table, changes: &mut Vec<Change>) {
    for wanted in after.foreign_keys() {
        match before.foreign_key(wanted.name()) {
            Some(existing) if existing == wanted => {}
            Some(existing) => {
                changes.push(Change::DropForeignKey {
                    table: key.to_owned(),
                    foreign_key: Box::new(existing.clone()),
                });
                changes.push(Change::AddForeignKey {
                    table: key.to_owned(),
                    foreign_key: Box::new(wanted.clone()),
                });
            }
            None => changes.push(Change::AddForeignKey {
                table: key.to_owned(),
                foreign_key: Box::new(wanted.clone()),
            }),
        }
    }
    for existing in before.foreign_keys() {
        if after.foreign_key(existing.name()).is_none() {
            changes.push(Change::DropForeignKey {
                table: key.to_owned(),
                foreign_key: Box::new(existing.clone()),
            });
        }
    }
}

fn diff_checks(key: &str, before: &Table, after: &Table, changes: &mut Vec<Change>) {
    for wanted in after.checks() {
        match before.check(wanted.name()) {
            // Compared through `normalise_expression`, because a live
            // PostgreSQL re-prints a predicate from its parse tree and the
            // literal text will not match what the entity wrote.
            Some(existing)
                if crate::schema::normalise_expression(existing.expression())
                    == crate::schema::normalise_expression(wanted.expression()) => {}
            Some(existing) => {
                changes.push(Change::DropCheck {
                    table: key.to_owned(),
                    check: existing.clone(),
                });
                changes.push(Change::AddCheck {
                    table: key.to_owned(),
                    check: wanted.clone(),
                });
            }
            None => changes.push(Change::AddCheck {
                table: key.to_owned(),
                check: wanted.clone(),
            }),
        }
    }
    for existing in before.checks() {
        if after.check(existing.name()).is_none() {
            changes.push(Change::DropCheck {
                table: key.to_owned(),
                check: existing.clone(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use moso_sql::DataType;

    use super::*;
    use crate::rename::{DropAndAdd, RefuseToGuess, Scripted};
    use crate::schema::{Action, ForeignKey};

    fn users() -> Table {
        let mut table = Table::new("users").for_entity("User");
        table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        table.add_column(Column::new("email", DataType::Text).for_field("email"));
        table.set_primary_key(["id"]);
        table
    }

    fn schema_with(table: Table) -> Schema {
        let mut schema = Schema::empty();
        schema.add_table(table);
        schema
    }

    fn diff(before: &Schema, after: &Schema) -> Diff {
        Diff::compute(before, after, &DropAndAdd).expect("diffs")
    }

    #[test]
    fn an_identical_schema_produces_nothing() {
        let schema = schema_with(users());
        assert!(diff(&schema, &schema).is_empty());
    }

    #[test]
    fn a_new_entity_is_one_create_table() {
        let after = schema_with(users());
        let changes = diff(&Schema::empty(), &after);
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes.changes()[0], Change::CreateTable(_)));
        assert!(!changes.is_destructive());
    }

    #[test]
    fn a_removed_entity_is_a_destructive_drop_table() {
        let before = schema_with(users());
        let changes = diff(&before, &Schema::empty());
        assert!(changes.is_destructive());
        assert_eq!(changes.destructive().len(), 1);
    }

    #[test]
    fn a_new_field_is_an_add_column_that_knows_it_needs_a_fill_value() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_column(Column::new("locale", DataType::Text));
        let after = schema_with(after_table);

        let changes = diff(&before, &after);
        match &changes.changes()[0] {
            Change::AddColumn {
                column,
                needs_backfill,
                ..
            } => {
                assert_eq!(column.name(), "locale");
                assert!(*needs_backfill, "NOT NULL with no default");
            }
            other => panic!("expected AddColumn, got {other:?}"),
        }
    }

    #[test]
    fn a_new_nullable_field_needs_no_fill_value() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_column(Column::new("bio", DataType::Text).nullable());
        let changes = diff(&before, &schema_with(after_table));
        assert!(matches!(
            changes.changes()[0],
            Change::AddColumn {
                needs_backfill: false,
                ..
            }
        ));
    }

    #[test]
    fn a_renamed_column_is_one_rename_when_the_oracle_says_so() {
        let before = schema_with(users());
        let mut after_table = Table::new("users").for_entity("User");
        after_table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        after_table.add_column(Column::new("email_address", DataType::Text).for_field("email"));
        after_table.set_primary_key(["id"]);

        let oracle = Scripted::parse(["users.email:email_address"]).expect("parses");
        let changes = Diff::compute(&before, &schema_with(after_table), &oracle).expect("diffs");
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes.changes()[0], Change::RenameColumn { .. }));
        assert!(!changes.is_destructive());
    }

    #[test]
    fn the_same_change_is_a_drop_and_an_add_when_the_oracle_says_no() {
        let before = schema_with(users());
        let mut after_table = Table::new("users").for_entity("User");
        after_table.add_column(Column::new("id", DataType::BigSerial));
        after_table.add_column(Column::new("email_address", DataType::Text));
        after_table.set_primary_key(["id"]);

        let changes = diff(&before, &schema_with(after_table));
        assert_eq!(changes.len(), 2);
        assert!(changes.is_destructive());
    }

    #[test]
    fn a_rename_candidate_is_refused_rather_than_guessed_in_ci() {
        let before = schema_with(users());
        let mut after_table = Table::new("users").for_entity("User");
        after_table.add_column(Column::new("id", DataType::BigSerial));
        after_table.add_column(Column::new("email_address", DataType::Text));
        after_table.set_primary_key(["id"]);

        let error =
            Diff::compute(&before, &schema_with(after_table), &RefuseToGuess).expect_err("refuses");
        assert!(
            error
                .to_string()
                .contains("--rename users.email:email_address"),
            "{error}"
        );
    }

    #[test]
    fn a_renamed_table_carries_its_columns_over() {
        let before = schema_with(users());
        let mut after_table = Table::new("accounts").for_entity("User");
        after_table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        after_table.add_column(Column::new("email", DataType::Text).for_field("email"));
        after_table.add_column(Column::new("locale", DataType::Text).nullable());
        after_table.set_primary_key(["id"]);

        let oracle = Scripted::parse(["users:accounts"]).expect("parses");
        let changes = Diff::compute(&before, &schema_with(after_table), &oracle).expect("diffs");
        let kinds: Vec<&str> = changes
            .changes()
            .iter()
            .map(|change| match change {
                Change::RenameTable { .. } => "rename",
                Change::AddColumn { .. } => "add",
                _ => "other",
            })
            .collect();
        assert_eq!(kinds, ["rename", "add"], "{:?}", changes.summary());
    }

    #[test]
    fn a_type_change_records_its_lossiness() {
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::BigInt));
        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::Integer));

        let changes = diff(&schema_with(before_table), &schema_with(after_table));
        match &changes.changes()[0] {
            Change::AlterColumnType {
                lossy, from, to, ..
            } => {
                assert!(*lossy);
                assert_eq!(from, "bigint");
                assert_eq!(to, "integer");
            }
            other => panic!("expected AlterColumnType, got {other:?}"),
        }
        assert!(changes.is_destructive());
    }

    #[test]
    fn a_widening_type_change_is_not_destructive() {
        let mut before_table = users();
        before_table.add_column(Column::new("n", DataType::Integer));
        let mut after_table = users();
        after_table.add_column(Column::new("n", DataType::BigInt));
        assert!(!diff(&schema_with(before_table), &schema_with(after_table)).is_destructive());
    }

    #[test]
    fn nullability_moves_in_both_directions() {
        let mut relaxed = users();
        relaxed.add_column(Column::new("bio", DataType::Text).nullable());
        let mut tightened = users();
        tightened.add_column(Column::new("bio", DataType::Text));

        let forward = diff(
            &schema_with(relaxed.clone()),
            &schema_with(tightened.clone()),
        );
        assert!(matches!(
            forward.changes()[0],
            Change::SetNotNull {
                needs_backfill: true,
                ..
            }
        ));

        let backward = diff(&schema_with(tightened), &schema_with(relaxed));
        assert!(matches!(backward.changes()[0], Change::DropNotNull { .. }));
    }

    #[test]
    fn defaults_move_in_both_directions() {
        let mut plain = users();
        plain.add_column(Column::new("locale", DataType::Text));
        let mut defaulted = users();
        defaulted.add_column(Column::new("locale", DataType::Text).with_default("'en'"));

        let set = diff(&schema_with(plain.clone()), &schema_with(defaulted.clone()));
        assert!(matches!(set.changes()[0], Change::SetDefault { .. }));

        let dropped = diff(&schema_with(defaulted), &schema_with(plain));
        assert!(matches!(dropped.changes()[0], Change::DropDefault { .. }));
    }

    #[test]
    fn a_changed_index_is_a_drop_and_a_create() {
        let mut before_table = users();
        before_table.add_index(Index::new("idx_users_email", ["email"]));
        let mut after_table = users();
        after_table.add_index(Index::new("idx_users_email", ["email", "id"]));

        let changes = diff(&schema_with(before_table), &schema_with(after_table));
        assert_eq!(changes.len(), 2);
        assert!(matches!(changes.changes()[0], Change::DropIndex { .. }));
        assert!(matches!(changes.changes()[1], Change::CreateIndex { .. }));
    }

    #[test]
    fn a_renamed_index_is_a_rename_when_confirmed() {
        let mut before_table = users();
        before_table.add_index(Index::new("idx_old", ["email"]));
        let mut after_table = users();
        after_table.add_index(Index::new("idx_new", ["email"]));

        let oracle = Scripted::parse(["users.idx_old:idx_new"]).expect("parses");
        let changes = Diff::compute(
            &schema_with(before_table),
            &schema_with(after_table),
            &oracle,
        )
        .expect("diffs");
        assert_eq!(changes.len(), 1);
        assert!(matches!(changes.changes()[0], Change::RenameIndex { .. }));
    }

    #[test]
    fn foreign_keys_and_checks_are_added_and_dropped() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.add_foreign_key(
            ForeignKey::new("fk", ["id"], "other", ["id"]).on_delete(Action::Cascade),
        );
        after_table.add_check(Check::new("chk", "id > 0"));

        let changes = diff(&before, &schema_with(after_table.clone()));
        assert_eq!(changes.len(), 2);
        assert!(
            changes
                .changes()
                .iter()
                .any(|change| matches!(change, Change::AddForeignKey { .. }))
        );
        assert!(
            changes
                .changes()
                .iter()
                .any(|change| matches!(change, Change::AddCheck { .. }))
        );

        let reverse = diff(&schema_with(after_table), &before);
        assert_eq!(reverse.len(), 2);
    }

    #[test]
    fn enum_variants_are_added_in_place() {
        let mut before = Schema::empty();
        before.add_enum(EnumType::new("user_role", ["admin", "member"]));
        let mut after = Schema::empty();
        after.add_enum(EnumType::new("user_role", ["admin", "member", "auditor"]));

        let changes = diff(&before, &after);
        match &changes.changes()[0] {
            Change::AddEnumValues { values, after, .. } => {
                assert_eq!(values, &["auditor".to_owned()]);
                assert_eq!(after, "member");
            }
            other => panic!("expected AddEnumValues, got {other:?}"),
        }
        assert!(changes.requires_no_transaction());
        assert!(!changes.is_destructive());
    }

    #[test]
    fn a_removed_enum_variant_is_a_destructive_rewrite() {
        let mut before = Schema::empty();
        before.add_enum(EnumType::new("user_role", ["admin", "member"]));
        let mut after = Schema::empty();
        after.add_enum(EnumType::new("user_role", ["admin"]));

        let changes = diff(&before, &after);
        assert!(matches!(changes.changes()[0], Change::RewriteEnum { .. }));
        assert!(changes.is_destructive());
    }

    #[test]
    fn changes_come_out_in_a_stable_order() {
        let mut before = Schema::empty();
        before.add_table(users());
        before.add_table(Table::new("legacy"));

        let mut after = Schema::empty();
        let mut updated = users();
        updated.add_column(Column::new("locale", DataType::Text).nullable());
        updated.add_index(Index::new("idx_users_email", ["email"]));
        after.add_table(updated);
        after.add_table(Table::new("audit_log"));

        let first = diff(&before, &after);
        let second = diff(&before, &after);
        assert_eq!(first.summary(), second.summary());

        // Creations before drops, always.
        let create_at = first
            .changes()
            .iter()
            .position(|change| matches!(change, Change::CreateTable(_)))
            .expect("a create");
        let drop_at = first
            .changes()
            .iter()
            .position(|change| matches!(change, Change::DropTable(_)))
            .expect("a drop");
        assert!(create_at < drop_at);
    }

    #[test]
    fn extensions_and_schemas_come_first() {
        let mut after = Schema::empty();
        after.add_extension("pg_trgm");
        after.add_table(Table::new("events").in_schema("analytics"));

        let changes = diff(&Schema::empty(), &after);
        assert!(matches!(changes.changes()[0], Change::CreateSchema(_)));
        assert!(matches!(changes.changes()[1], Change::CreateExtension(_)));
        assert!(matches!(changes.changes()[2], Change::CreateTable(_)));
    }

    #[test]
    fn suggested_names_are_readable() {
        let after = schema_with(users());
        assert_eq!(
            diff(&Schema::empty(), &after).suggested_name(),
            "create_users"
        );
        assert_eq!(Diff::empty().suggested_name(), "no_changes");
    }

    #[test]
    fn a_primary_key_change_is_recorded_with_its_previous_value() {
        let before = schema_with(users());
        let mut after_table = users();
        after_table.set_primary_key(["id", "email"]);
        let changes = diff(&before, &schema_with(after_table));
        match &changes.changes()[0] {
            Change::SetPrimaryKey {
                columns, previous, ..
            } => {
                assert_eq!(columns, &["id".to_owned(), "email".to_owned()]);
                assert_eq!(previous, &["id".to_owned()]);
            }
            other => panic!("expected SetPrimaryKey, got {other:?}"),
        }
    }
}
