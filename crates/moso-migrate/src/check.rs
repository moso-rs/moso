//! `moso db check` — the deploy pre-flight.
//!
//! Compares the live database against the committed snapshot and reports drift
//! **in both directions**: what the entities describe and the database does not
//! have, and what the database has and the entities do not describe. One
//! direction alone is half a check — the first catches an unapplied migration,
//! the second catches somebody's `psql` session.
//!
//! Run it in CI against a fresh database, which proves the migrations reproduce
//! the schema, and in the deploy pipeline against staging, which proves nobody
//! hand-edited it.
//!
//! ```no_run
//! use moso_migrate::check::check;
//!
//! # async fn example(
//! #     connection: &mut moso_migrate::conn::Connection,
//! #     expected: &moso_migrate::Schema,
//! # ) -> moso_migrate::Result<()> {
//! let drift = check(connection, expected, Vec::new()).await?;
//! if !drift.is_empty() {
//!     eprintln!("{drift}");
//! }
//! # Ok(())
//! # }
//! ```

use std::fmt;

use crate::conn::Connection;
use crate::diff::{Change, Diff};
use crate::error::Result;
use crate::rename::DropAndAdd;
use crate::schema::Schema;
use crate::version::Version;

/// What a live database has that the snapshot does not, and the reverse.
///
/// ```
/// use moso_migrate::check::Drift;
///
/// let drift = Drift::default();
/// assert!(drift.is_empty());
/// assert_eq!(drift.to_string(), "the database matches the expected schema");
/// ```
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Drift {
    missing: Vec<String>,
    extra: Vec<String>,
    mismatched: Vec<String>,
    pending: Vec<Version>,
}

impl Drift {
    /// Things the entities describe that the database does not have.
    ///
    /// ```
    /// assert!(moso_migrate::check::Drift::default().missing_in_database().is_empty());
    /// ```
    #[must_use]
    pub fn missing_in_database(&self) -> &[String] {
        &self.missing
    }

    /// Things the database has that no entity describes.
    ///
    /// ```
    /// assert!(moso_migrate::check::Drift::default().extra_in_database().is_empty());
    /// ```
    #[must_use]
    pub fn extra_in_database(&self) -> &[String] {
        &self.extra
    }

    /// Things both have, differently.
    ///
    /// ```
    /// assert!(moso_migrate::check::Drift::default().mismatched().is_empty());
    /// ```
    #[must_use]
    pub fn mismatched(&self) -> &[String] {
        &self.mismatched
    }

    /// Migrations on disk that have not been applied, which is usually the
    /// explanation for everything above.
    ///
    /// ```
    /// assert!(moso_migrate::check::Drift::default().pending().is_empty());
    /// ```
    #[must_use]
    pub fn pending(&self) -> &[Version] {
        &self.pending
    }

    /// Whether the database matches.
    ///
    /// Pending migrations alone are not drift: they are the fix for it. A
    /// deploy pipeline wants to know both, and it wants to know them apart.
    ///
    /// ```
    /// assert!(moso_migrate::check::Drift::default().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.missing.is_empty() && self.extra.is_empty() && self.mismatched.is_empty()
    }

    /// How many differences there are.
    ///
    /// ```
    /// assert_eq!(moso_migrate::check::Drift::default().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.missing.len() + self.extra.len() + self.mismatched.len()
    }

    /// Turns drift into the error a CI job should fail on.
    ///
    /// # Errors
    ///
    /// [`Error::Drift`](crate::Error::Drift) when there is any.
    ///
    /// ```
    /// assert!(moso_migrate::check::Drift::default().into_result().is_ok());
    /// ```
    pub fn into_result(self) -> Result<()> {
        if self.is_empty() {
            return Ok(());
        }
        Err(crate::Error::Drift(Box::new(self)))
    }
}

impl fmt::Display for Drift {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            if self.pending.is_empty() {
                return f.write_str("the database matches the expected schema");
            }
            return write!(
                f,
                "the database matches the expected schema\n\n  {} pending migration(s): {}",
                self.pending.len(),
                self.pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }

        writeln!(f, "database does not match the expected schema\n")?;
        for (label, items) in [
            ("missing in database", &self.missing),
            ("extra in database", &self.extra),
            ("mismatch", &self.mismatched),
        ] {
            for item in items {
                writeln!(f, "  {label:<20} {item}")?;
            }
        }
        if self.pending.is_empty() {
            write!(
                f,
                "\n  no pending migrations — the database was changed outside Moso, or an entity \
                 changed without `moso db make-migration`\n  \
                 help: run `moso db make-migration` and review what it produces"
            )
        } else {
            write!(
                f,
                "\n  {} pending migration(s): {}\n  help: run `moso db migrate`",
                self.pending.len(),
                self.pending
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
}

/// Reads the live schema and compares it with `expected`.
///
/// # Errors
///
/// [`Error::Database`](crate::Error::Database) when the catalogue cannot be
/// read.
///
/// ```no_run
/// # async fn example(
/// #     connection: &mut moso_migrate::conn::Connection,
/// #     expected: &moso_migrate::Schema,
/// # ) -> moso_migrate::Result<()> {
/// let drift = moso_migrate::check::check(connection, expected, Vec::new()).await?;
/// drift.into_result()?;
/// # Ok(())
/// # }
/// ```
pub async fn check(
    connection: &mut Connection,
    expected: &Schema,
    pending: Vec<Version>,
) -> Result<Drift> {
    // The named schemas the snapshot declares are off the connection's search
    // path by definition, so they have to be asked for explicitly.
    let named: Vec<String> = expected.schemas().map(ToOwned::to_owned).collect();
    let live = crate::introspect::read_schema_including(connection, &named).await?;
    let mut drift = compare(&live, expected)?;
    drift.pending = pending;
    Ok(drift)
}

/// Compares two schemas without touching a database, which is what makes drift
/// detection testable.
///
/// # Errors
///
/// [`Error::Snapshot`](crate::Error::Snapshot) when either side names a type
/// this build cannot parse.
///
/// ```
/// use moso_migrate::check::compare;
/// use moso_migrate::schema::{Schema, Table};
///
/// let mut live = Schema::empty();
/// live.add_table(Table::new("legacy"));
///
/// let drift = compare(&live, &Schema::empty())?;
/// assert_eq!(drift.extra_in_database().len(), 1);
/// assert!(drift.missing_in_database().is_empty());
/// # Ok::<(), moso_migrate::Error>(())
/// ```
pub fn compare(live: &Schema, expected: &Schema) -> Result<Drift> {
    // The differ already knows how to say "these two schemas differ, here is
    // how". Drift is that answer, sorted into the two directions a deploy
    // pipeline cares about.
    let diff = Diff::compute(live, expected, &DropAndAdd)?;
    let mut drift = Drift::default();

    for change in diff.changes() {
        match change {
            Change::CreateTable(table) => drift.missing.push(format!(
                "{} ({} columns)",
                table.qualified_name(),
                table.columns().len()
            )),
            Change::AddColumn { table, column, .. } => drift.missing.push(format!(
                "{table}.{} ({}{}{})",
                column.name(),
                column.type_name(),
                if column.is_nullable() {
                    ""
                } else {
                    " not null"
                },
                column
                    .default()
                    .map(|default| format!(" default {default}"))
                    .unwrap_or_default()
            )),
            Change::CreateIndex { table, index } => drift.missing.push(format!(
                "index {} on {table} ({})",
                index.name(),
                index
                    .columns()
                    .iter()
                    .map(crate::schema::IndexPart::expr)
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            Change::AddForeignKey { table, foreign_key } => drift.missing.push(format!(
                "foreign key {} on {table} -> {}",
                foreign_key.name(),
                foreign_key.target_table()
            )),
            Change::AddCheck { table, check } => {
                drift
                    .missing
                    .push(format!("check {} on {table}", check.name()));
            }
            Change::CreateEnum(enum_type) => drift
                .missing
                .push(format!("type {}", enum_type.qualified_name())),
            Change::AddEnumValues { name, values, .. } => drift
                .missing
                .push(format!("type {name} values {}", values.join(", "))),
            Change::CreateExtension(name) => drift.missing.push(format!("extension {name}")),
            Change::CreateSchema(name) => drift.missing.push(format!("schema {name}")),

            Change::DropTable(table) => drift.extra.push(table.qualified_name()),
            Change::DropColumn { table, column } => {
                drift.extra.push(format!(
                    "{table}.{} ({})",
                    column.name(),
                    column.type_name()
                ));
            }
            Change::DropIndex { table, index } => {
                drift
                    .extra
                    .push(format!("index {} on {table}", index.name()));
            }
            Change::DropForeignKey { table, foreign_key } => {
                drift
                    .extra
                    .push(format!("foreign key {} on {table}", foreign_key.name()));
            }
            Change::DropCheck { table, check } => {
                drift
                    .extra
                    .push(format!("check {} on {table}", check.name()));
            }
            Change::DropEnum(enum_type) => {
                drift
                    .extra
                    .push(format!("type {}", enum_type.qualified_name()));
            }

            Change::AlterColumnType {
                table,
                column,
                from,
                to,
                ..
            } => drift
                .mismatched
                .push(format!("{table}.{column}  expected {to} got {from}")),
            Change::SetNotNull { table, column, .. } => drift
                .mismatched
                .push(format!("{table}.{column}  expected not null, got nullable")),
            Change::DropNotNull { table, column } => drift
                .mismatched
                .push(format!("{table}.{column}  expected nullable, got not null")),
            Change::SetDefault {
                table,
                column,
                default,
                previous,
            } => drift.mismatched.push(format!(
                "{table}.{column}  expected default {default} got {}",
                previous.as_deref().unwrap_or("none")
            )),
            Change::DropDefault {
                table,
                column,
                previous,
            } => drift.mismatched.push(format!(
                "{table}.{column}  expected no default, got {previous}"
            )),
            Change::SetPrimaryKey {
                table,
                columns,
                previous,
            } => drift.mismatched.push(format!(
                "primary key of {table}  expected ({}) got ({})",
                columns.join(", "),
                previous.join(", ")
            )),
            Change::RewriteEnum { before, after } => drift.mismatched.push(format!(
                "type {}  expected ({}) got ({})",
                after.qualified_name(),
                after.labels().join(", "),
                before.labels().join(", ")
            )),
            Change::SetComment { table, column, .. } => drift.mismatched.push(match column {
                Some(column) => format!("comment on {table}.{column}"),
                None => format!("comment on {table}"),
            }),
            // A rename cannot appear: the oracle is `DropAndAdd`, so a renamed
            // object is reported as one extra and one missing, which is the
            // truthful description of what a drift check can actually see.
            Change::RenameTable { .. }
            | Change::RenameColumn { .. }
            | Change::RenameIndex { .. } => {}
        }
    }

    drift.missing.sort();
    drift.extra.sort();
    drift.mismatched.sort();
    Ok(drift)
}

#[cfg(test)]
mod tests {
    use moso_sql::DataType;

    use super::*;
    use crate::schema::{Column, Index, Table};

    fn users() -> Table {
        let mut table = Table::new("users");
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

    #[test]
    fn an_identical_schema_has_no_drift() {
        let schema = schema_with(users());
        let drift = compare(&schema, &schema).expect("compares");
        assert!(drift.is_empty());
        assert_eq!(
            drift.to_string(),
            "the database matches the expected schema"
        );
    }

    #[test]
    fn a_missing_column_is_reported_with_its_type() {
        let live = schema_with(users());
        let mut wanted = users();
        wanted.add_column(Column::new("locale", DataType::Text).with_default("'en'"));

        let drift = compare(&live, &schema_with(wanted)).expect("compares");
        assert_eq!(drift.missing_in_database().len(), 1);
        assert!(
            drift.missing_in_database()[0].contains("users.locale (text not null default 'en')"),
            "{:?}",
            drift.missing_in_database()
        );
        assert!(drift.extra_in_database().is_empty());
    }

    #[test]
    fn an_extra_column_is_reported_the_other_way() {
        let mut live_table = users();
        live_table.add_column(Column::new("legacy_id", DataType::Integer).nullable());
        let drift = compare(&schema_with(live_table), &schema_with(users())).expect("compares");
        assert_eq!(drift.extra_in_database(), ["users.legacy_id (integer)"]);
        assert!(drift.missing_in_database().is_empty());
    }

    #[test]
    fn an_index_mismatch_is_a_drop_and_an_add() {
        let mut live_table = users();
        live_table.add_index(Index::new("idx_posts_author", ["email", "id"]));
        let mut wanted = users();
        wanted.add_index(Index::new("idx_posts_author", ["email"]));

        let drift = compare(&schema_with(live_table), &schema_with(wanted)).expect("compares");
        assert_eq!(drift.len(), 2);
        assert!(drift.extra_in_database()[0].contains("idx_posts_author"));
        assert!(drift.missing_in_database()[0].contains("idx_posts_author"));
    }

    #[test]
    fn a_type_mismatch_says_which_way_round() {
        let mut live_table = users();
        live_table.add_column(Column::new("n", DataType::Integer).nullable());
        let mut wanted = users();
        wanted.add_column(Column::new("n", DataType::BigInt).nullable());

        let drift = compare(&schema_with(live_table), &schema_with(wanted)).expect("compares");
        assert_eq!(drift.mismatched(), ["users.n  expected bigint got integer"]);
    }

    #[test]
    fn the_report_names_both_directions_and_the_fix() {
        let mut live_table = users();
        live_table.add_column(Column::new("legacy_id", DataType::Integer).nullable());
        let mut wanted = users();
        wanted.add_column(Column::new("locale", DataType::Text).with_default("'en'"));

        let mut drift = compare(&schema_with(live_table), &schema_with(wanted)).expect("compares");
        drift.pending = vec![
            Version::from_parts(2026, 7, 29, 10, 15, 0),
            Version::from_parts(2026, 7, 30, 9, 0, 0),
        ];

        let report = drift.to_string();
        assert!(report.contains("missing in database"), "{report}");
        assert!(report.contains("extra in database"), "{report}");
        assert!(report.contains("2 pending migration(s)"), "{report}");
        assert!(report.contains("moso db migrate"), "{report}");
    }

    #[test]
    fn drift_with_no_pending_migrations_says_somebody_edited_the_database() {
        let mut live_table = users();
        live_table.add_column(Column::new("oops", DataType::Text).nullable());
        let drift = compare(&schema_with(live_table), &schema_with(users())).expect("compares");
        let report = drift.to_string();
        assert!(report.contains("outside Moso"), "{report}");
        assert!(report.contains("make-migration"), "{report}");
    }

    #[test]
    fn drift_becomes_an_error_for_ci() {
        let mut live_table = users();
        live_table.add_column(Column::new("oops", DataType::Text).nullable());
        let drift = compare(&schema_with(live_table), &schema_with(users())).expect("compares");
        let error = drift.into_result().expect_err("drift");
        assert!(error.to_string().contains("users.oops"), "{error}");
    }

    #[test]
    fn a_clean_check_with_pending_migrations_says_so_without_failing() {
        let schema = schema_with(users());
        let mut drift = compare(&schema, &schema).expect("compares");
        drift.pending = vec![Version::from_parts(2026, 1, 1, 0, 0, 0)];
        assert!(drift.is_empty());
        assert!(drift.to_string().contains("1 pending migration(s)"));
        assert!(drift.into_result().is_ok());
    }
}
