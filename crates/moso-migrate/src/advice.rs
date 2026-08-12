//! Expand/contract guidance — safety-policy point 7.
//!
//! During a rolling deploy the old code and the new code run at the same time.
//! A migration that assumes otherwise breaks the old pods, and it breaks them
//! at the moment of least attention: mid-deploy, on a Friday, while the graphs
//! still look fine.
//!
//! Three shapes cause almost all of it, and the generator detects each one and
//! prints the two migrations you actually need.
//!
//! ```
//! use moso_migrate::advice::Advice;
//! use moso_migrate::diff::{Change, Diff};
//! use moso_migrate::rename::DropAndAdd;
//! use moso_migrate::schema::{Column, Schema, Table};
//! use moso_sql::DataType;
//!
//! let mut before = Schema::empty();
//! let mut users = Table::new("users");
//! users.add_column(Column::new("id", DataType::BigSerial));
//! users.add_column(Column::new("legacy_id", DataType::Integer).nullable());
//! before.add_table(users.clone());
//!
//! let mut after = Schema::empty();
//! let mut trimmed = Table::new("users");
//! trimmed.add_column(Column::new("id", DataType::BigSerial));
//! after.add_table(trimmed);
//!
//! let diff = Diff::compute(&before, &after, &DropAndAdd)?;
//! let advice = Advice::for_diff(&diff);
//! assert_eq!(advice.len(), 1);
//! assert!(advice[0].plan().contains("stop writing"));
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use crate::diff::{Change, Diff};

/// One thing about this migration that will break a rolling deploy.
///
/// ```
/// use moso_migrate::advice::Advice;
///
/// let advice = Advice::dropping_a_column("users", "legacy_id");
/// assert!(advice.summary().contains("legacy_id"));
/// assert!(advice.plan().contains("two migrations"));
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Advice {
    summary: String,
    plan: String,
}

impl Advice {
    /// The one-line problem.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// assert!(Advice::dropping_a_column("t", "c").summary().starts_with("dropping"));
    /// ```
    #[must_use]
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// The expand/contract plan, as text to print.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// assert!(!Advice::dropping_a_column("t", "c").plan().is_empty());
    /// ```
    #[must_use]
    pub fn plan(&self) -> &str {
        &self.plan
    }

    /// Dropping a column the previous version still writes.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// let advice = Advice::dropping_a_column("users", "legacy_id");
    /// assert!(advice.plan().contains("users.legacy_id"));
    /// ```
    #[must_use]
    pub fn dropping_a_column(table: &str, column: &str) -> Self {
        Self {
            summary: format!(
                "dropping `{table}.{column}` breaks any running version that still writes it"
            ),
            plan: format!(
                "expand/contract — this needs two migrations, not one:\n  \
                 1. now:   stop writing `{table}.{column}` in the application, and make the \
                 column nullable if it is not.\n     \
                 Deploy that. Every pod is then running code that does not need it.\n  \
                 2. later: drop the column, in a migration of its own.\n\
                 Dropping it in one step means the old pods start failing the moment this \
                 migration lands, and the deploy is not finished yet."
            ),
        }
    }

    /// Adding a `NOT NULL` column the previous version does not write.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// let advice = Advice::adding_a_required_column("users", "locale");
    /// assert!(advice.plan().contains("DEFAULT"));
    /// ```
    #[must_use]
    pub fn adding_a_required_column(table: &str, column: &str) -> Self {
        Self {
            summary: format!(
                "adding `{table}.{column}` as NOT NULL with no default breaks any running version \
                 that inserts without it"
            ),
            plan: format!(
                "expand/contract — either give it a DEFAULT, or split it in two:\n  \
                 1. now:   add `{table}.{column}` nullable, and start writing it.\n     \
                 Deploy that. Every pod is then filling it in.\n  \
                 2. later: backfill the old rows and add NOT NULL, in a migration of its own.\n\
                 A DEFAULT does the same job in one step and is usually the right answer; \
                 this generator has already written the three-step form for you."
            ),
        }
    }

    /// Renaming a column, which no rolling deploy survives in one step.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// let advice = Advice::renaming("users", "name", "full_name");
    /// assert!(advice.plan().contains("three migrations"));
    /// ```
    #[must_use]
    pub fn renaming(table: &str, from: &str, to: &str) -> Self {
        Self {
            summary: format!(
                "renaming `{table}.{from}` to `{to}` breaks every running version at once"
            ),
            plan: format!(
                "expand/contract — a rename is three migrations if the deploy is rolling:\n  \
                 1. now:   add `{table}.{to}`, and write BOTH columns from the application.\n  \
                 2. next:  backfill `{to}` from `{from}`, then read `{to}` only.\n  \
                 3. later: drop `{table}.{from}`.\n\
                 `ALTER TABLE … RENAME COLUMN` is instant and correct when you can stop the \
                 world; it is a hard outage when you cannot."
            ),
        }
    }

    /// A type change the old code will not understand.
    ///
    /// ```
    /// # use moso_migrate::advice::Advice;
    /// let advice = Advice::narrowing_a_type("users", "n", "bigint", "integer");
    /// assert!(advice.summary().contains("bigint"));
    /// ```
    #[must_use]
    pub fn narrowing_a_type(table: &str, column: &str, from: &str, to: &str) -> Self {
        Self {
            summary: format!(
                "changing `{table}.{column}` from {from} to {to} can fail on data the running \
                 version is still writing"
            ),
            plan: format!(
                "expand/contract:\n  \
                 1. now:   stop writing values that will not fit in {to}, and deploy.\n  \
                 2. next:  verify with `SELECT count(*) FROM {table} WHERE {column} …` that \
                 nothing is out of range.\n  \
                 3. later: change the type.\n\
                 The `ALTER COLUMN … TYPE` takes a full-table rewrite and an ACCESS EXCLUSIVE \
                 lock; on a large table, plan for it."
            ),
        }
    }

    /// Everything about `diff` that will not survive a rolling deploy.
    ///
    /// ```
    /// use moso_migrate::advice::Advice;
    /// use moso_migrate::diff::Diff;
    ///
    /// assert!(Advice::for_diff(&Diff::empty()).is_empty());
    /// ```
    #[must_use]
    pub fn for_diff(diff: &Diff) -> Vec<Self> {
        let mut advice = Vec::new();
        for change in diff.changes() {
            match change {
                Change::DropColumn { table, column } => {
                    advice.push(Self::dropping_a_column(table, column.name()));
                }
                Change::AddColumn {
                    table,
                    column,
                    needs_backfill: true,
                } => advice.push(Self::adding_a_required_column(table, column.name())),
                Change::RenameColumn { table, from, to } => {
                    advice.push(Self::renaming(table, from, to));
                }
                Change::AlterColumnType {
                    table,
                    column,
                    from,
                    to,
                    lossy: true,
                } => advice.push(Self::narrowing_a_type(table, column, from, to)),
                _ => {}
            }
        }
        advice
    }
}

#[cfg(test)]
mod tests {
    use moso_sql::DataType;

    use super::*;
    use crate::rename::{DropAndAdd, Scripted};
    use crate::schema::{Column, Schema, Table};

    fn users() -> Table {
        let mut table = Table::new("users").for_entity("User");
        table.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        table.set_primary_key(["id"]);
        table
    }

    fn schema_with(table: Table) -> Schema {
        let mut schema = Schema::empty();
        schema.add_table(table);
        schema
    }

    #[test]
    fn a_pure_addition_needs_no_advice() {
        let mut after = users();
        after.add_column(Column::new("bio", DataType::Text).nullable());
        let diff =
            Diff::compute(&schema_with(users()), &schema_with(after), &DropAndAdd).expect("diffs");
        assert!(Advice::for_diff(&diff).is_empty());
    }

    #[test]
    fn dropping_a_column_earns_the_two_step_plan() {
        let mut before = users();
        before.add_column(Column::new("legacy_id", DataType::Integer).nullable());
        let diff =
            Diff::compute(&schema_with(before), &schema_with(users()), &DropAndAdd).expect("diffs");
        let advice = Advice::for_diff(&diff);
        assert_eq!(advice.len(), 1);
        assert!(advice[0].plan().contains("1. now:"), "{}", advice[0].plan());
        assert!(
            advice[0].plan().contains("2. later:"),
            "{}",
            advice[0].plan()
        );
    }

    #[test]
    fn a_required_column_earns_the_default_advice() {
        let mut after = users();
        after.add_column(Column::new("locale", DataType::Text));
        let diff =
            Diff::compute(&schema_with(users()), &schema_with(after), &DropAndAdd).expect("diffs");
        let advice = Advice::for_diff(&diff);
        assert_eq!(advice.len(), 1);
        assert!(
            advice[0].summary().contains("NOT NULL"),
            "{}",
            advice[0].summary()
        );
    }

    #[test]
    fn a_defaulted_column_earns_none() {
        let mut after = users();
        after.add_column(Column::new("locale", DataType::Text).with_default("'en'"));
        let diff =
            Diff::compute(&schema_with(users()), &schema_with(after), &DropAndAdd).expect("diffs");
        assert!(Advice::for_diff(&diff).is_empty());
    }

    #[test]
    fn a_rename_earns_the_three_step_plan() {
        let mut before = users();
        before.add_column(Column::new("name", DataType::Text).nullable());
        let mut after = users();
        after.add_column(Column::new("full_name", DataType::Text).nullable());

        let oracle = Scripted::parse(["users.name:full_name"]).expect("parses");
        let diff =
            Diff::compute(&schema_with(before), &schema_with(after), &oracle).expect("diffs");
        let advice = Advice::for_diff(&diff);
        assert_eq!(advice.len(), 1);
        assert!(
            advice[0].plan().contains("write BOTH columns"),
            "{}",
            advice[0].plan()
        );
    }

    #[test]
    fn a_narrowing_type_change_earns_the_verify_step() {
        let mut before = users();
        before.add_column(Column::new("n", DataType::BigInt).nullable());
        let mut after = users();
        after.add_column(Column::new("n", DataType::Integer).nullable());

        let diff =
            Diff::compute(&schema_with(before), &schema_with(after), &DropAndAdd).expect("diffs");
        let advice = Advice::for_diff(&diff);
        assert_eq!(advice.len(), 1);
        assert!(
            advice[0].plan().contains("ACCESS EXCLUSIVE"),
            "{}",
            advice[0].plan()
        );
    }

    #[test]
    fn a_widening_type_change_earns_none() {
        let mut before = users();
        before.add_column(Column::new("n", DataType::Integer).nullable());
        let mut after = users();
        after.add_column(Column::new("n", DataType::BigInt).nullable());
        let diff =
            Diff::compute(&schema_with(before), &schema_with(after), &DropAndAdd).expect("diffs");
        assert!(Advice::for_diff(&diff).is_empty());
    }
}
