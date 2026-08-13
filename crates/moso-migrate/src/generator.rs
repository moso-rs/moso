//! `moso db make-migration` — the whole loop, from entities to a file on disk.
//!
//! ```text
//!    src/models/*.rs  (#[derive(Entity)])
//!             │  EntityDescriptor
//!             ▼
//!    ┌─────────────────┐   diff    ┌────────────────────────────┐
//!    │ desired schema  │──────────▶│ migrations/00042_add_x.sql │
//!    └─────────────────┘           └────────────────────────────┘
//!             ▲                                  │ apply
//!    ┌─────────────────┐                         ▼
//!    │ migrations/     │◀──────── update ──── database
//!    │  .schema.json   │                    (moso_migrations table)
//!    └─────────────────┘
//! ```
//!
//! # Idempotence
//!
//! Running it twice must produce exactly one migration. Everything about how
//! the snapshot is written — ordered maps, canonical type spellings, normalised
//! defaults — exists to make the second run's diff empty. It is an acceptance
//! criterion because a generator that emits a spurious migration is a generator
//! nobody runs.
//!
//! ```no_run
//! use moso_migrate::generator::Generator;
//! use moso_migrate::rename::RefuseToGuess;
//! use moso_orm::Backend;
//!
//! # fn example(entities: &[&moso_orm::descriptor::EntityDescriptor]) -> moso_migrate::Result<()> {
//! let generator = Generator::new("migrations", Backend::Postgres);
//! match generator.make_migration(entities, None, &RefuseToGuess)? {
//!     Some(generated) => println!("wrote {}", generated.path().display()),
//!     None => println!("no changes"),
//! }
//! # Ok(())
//! # }
//! ```

use std::path::{Path, PathBuf};

use moso_orm::Backend;
use moso_orm::descriptor::EntityDescriptor;

use crate::advice::Advice;
use crate::diff::Diff;
use crate::error::{Error, Result};
use crate::file::write_migration;
use crate::plan::Plan;
use crate::rename::Oracle;
use crate::schema::Schema;
use crate::version::{MigrationId, Version};

/// The snapshot's file name inside the migrations directory.
///
/// ```
/// assert_eq!(moso_migrate::generator::SNAPSHOT_FILE, ".schema.json");
/// ```
pub const SNAPSHOT_FILE: &str = ".schema.json";

/// What one `make-migration` produced.
///
/// The generator does not write anything by itself: it returns the file and the
/// snapshot, and [`Generated::write`] puts them on disk. That is what lets
/// `--dry-run` show you the migration it would have written, and what lets the
/// idempotence test run the generator twice without touching a filesystem.
///
/// ```
/// use moso_migrate::generator::Generated;
/// # fn example(generated: &Generated) {
/// println!("{}", generated.migration());
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Generated {
    id: MigrationId,
    path: PathBuf,
    migration: String,
    snapshot_path: PathBuf,
    snapshot: String,
    diff: Diff,
    advice: Vec<Advice>,
}

impl Generated {
    /// The migration's identity.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// println!("{}", generated.id());
    /// # }
    /// ```
    #[must_use]
    pub const fn id(&self) -> &MigrationId {
        &self.id
    }

    /// Where the migration goes.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// assert_eq!(generated.path().extension().and_then(|e| e.to_str()), Some("sql"));
    /// # }
    /// ```
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The migration's text.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// assert!(generated.migration().contains("-- +migrate up"));
    /// # }
    /// ```
    #[must_use]
    pub fn migration(&self) -> &str {
        &self.migration
    }

    /// Where the snapshot goes.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// assert!(generated.snapshot_path().ends_with(".schema.json"));
    /// # }
    /// ```
    #[must_use]
    pub fn snapshot_path(&self) -> &Path {
        &self.snapshot_path
    }

    /// The snapshot's text.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// assert!(generated.snapshot().starts_with('{'));
    /// # }
    /// ```
    #[must_use]
    pub fn snapshot(&self) -> &str {
        &self.snapshot
    }

    /// The changes it describes.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// assert!(!generated.diff().is_empty());
    /// # }
    /// ```
    #[must_use]
    pub const fn diff(&self) -> &Diff {
        &self.diff
    }

    /// Expand/contract warnings, which the CLI prints after the file name.
    ///
    /// ```
    /// # fn example(generated: &moso_migrate::generator::Generated) {
    /// for advice in generated.advice() {
    ///     eprintln!("{}", advice.summary());
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn advice(&self) -> &[Advice] {
        &self.advice
    }

    /// Writes both files, creating the directory if it is not there.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] naming the path and what to do about it.
    ///
    /// ```no_run
    /// # fn example(generated: &moso_migrate::generator::Generated) -> moso_migrate::Result<()> {
    /// generated.write()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn write(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| {
                Error::io(
                    "creating",
                    parent,
                    "check the working directory and its permissions",
                    source,
                )
            })?;
        }
        std::fs::write(&self.path, &self.migration).map_err(|source| {
            Error::io(
                "writing",
                &self.path,
                "check the directory's permissions",
                source,
            )
        })?;
        std::fs::write(&self.snapshot_path, &self.snapshot).map_err(|source| {
            Error::io(
                "writing",
                &self.snapshot_path,
                "the migration was written but the snapshot was not; delete the migration and \
                 try again, or the next `make-migration` will regenerate it",
                source,
            )
        })
    }
}

/// Generates migrations from an entity graph.
///
/// ```
/// use moso_migrate::generator::Generator;
/// use moso_orm::Backend;
///
/// let generator = Generator::new("migrations", Backend::Postgres);
/// assert_eq!(generator.backend(), Backend::Postgres);
/// ```
#[derive(Clone, Debug)]
pub struct Generator {
    directory: PathBuf,
    backend: Backend,
    now: Option<Version>,
}

impl Generator {
    /// A generator for a directory and a backend.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// let generator = Generator::new("migrations", Backend::Sqlite);
    /// assert_eq!(generator.directory().to_str(), Some("migrations"));
    /// ```
    #[must_use]
    pub fn new(directory: impl Into<PathBuf>, backend: Backend) -> Self {
        Self {
            directory: directory.into(),
            backend,
            now: None,
        }
    }

    /// Fixes the version the next migration gets, which is what makes the
    /// generator's output testable byte for byte.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_migrate::Version;
    /// # use moso_orm::Backend;
    /// let generator = Generator::new("migrations", Backend::Postgres)
    ///     .at(Version::from_parts(2026, 7, 29, 10, 15, 0));
    /// assert_eq!(generator.clock().map(|v| v.to_string()).as_deref(), Some("20260729T101500"));
    /// ```
    #[must_use]
    pub const fn at(mut self, version: Version) -> Self {
        self.now = Some(version);
        self
    }

    /// The directory.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// assert!(Generator::new("migrations", Backend::Postgres).directory().is_relative());
    /// ```
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// The backend.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// assert_eq!(Generator::new("m", Backend::Sqlite).backend(), Backend::Sqlite);
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.backend
    }

    /// The fixed version, if one was set.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// assert!(Generator::new("m", Backend::Postgres).clock().is_none());
    /// ```
    #[must_use]
    pub const fn clock(&self) -> Option<Version> {
        self.now
    }

    /// The path to the snapshot.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// let path = Generator::new("migrations", Backend::Postgres).snapshot_path();
    /// assert!(path.ends_with(".schema.json"));
    /// ```
    #[must_use]
    pub fn snapshot_path(&self) -> PathBuf {
        self.directory.join(SNAPSHOT_FILE)
    }

    /// Reads the committed snapshot, or an empty schema when there is none.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the file exists and cannot be read,
    /// [`Error::Snapshot`] when it is not a snapshot this build understands.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// let generator = Generator::new("does-not-exist", Backend::Postgres);
    /// assert!(generator.read_snapshot()?.is_empty());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn read_snapshot(&self) -> Result<Schema> {
        let path = self.snapshot_path();
        if !path.exists() {
            return Ok(Schema::empty());
        }
        let text = std::fs::read_to_string(&path).map_err(|source| {
            Error::io(
                "reading",
                &path,
                "it is committed to version control; check out the branch that has it",
                source,
            )
        })?;
        Schema::from_json(&text)
    }

    /// Diffs the entities against the snapshot and builds the migration.
    ///
    /// Returns `None` when there is nothing to do — the answer the idempotence
    /// test asserts on the second run.
    ///
    /// # Errors
    ///
    /// [`Error::NeedsAnswer`] when a rename candidate needs a human and the
    /// oracle cannot supply one, [`Error::Unsupported`] when a change cannot be
    /// expressed on the backend, and everything [`Generator::read_snapshot`]
    /// returns.
    ///
    /// ```
    /// use moso_migrate::generator::Generator;
    /// use moso_migrate::rename::DropAndAdd;
    /// use moso_orm::Backend;
    ///
    /// let generator = Generator::new("does-not-exist", Backend::Postgres);
    /// assert!(generator.make_migration(&[], None, &DropAndAdd)?.is_none());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn make_migration(
        &self,
        entities: &[&EntityDescriptor],
        name: Option<&str>,
        oracle: &dyn Oracle,
    ) -> Result<Option<Generated>> {
        let before = self.read_snapshot()?;
        let after = Schema::from_entities(entities.iter().copied())?;
        self.make_migration_between(&before, &after, name, oracle)
    }

    /// The same, between two schemas the caller already has.
    ///
    /// Used by the round-trip and idempotence tests, and by anyone generating a
    /// migration for a schema that did not come from entities.
    ///
    /// # Errors
    ///
    /// As [`Generator::make_migration`].
    ///
    /// ```
    /// use moso_migrate::generator::Generator;
    /// use moso_migrate::rename::DropAndAdd;
    /// use moso_migrate::Schema;
    /// use moso_orm::Backend;
    ///
    /// let generator = Generator::new("migrations", Backend::Postgres);
    /// let empty = Schema::empty();
    /// assert!(generator.make_migration_between(&empty, &empty, None, &DropAndAdd)?.is_none());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn make_migration_between(
        &self,
        before: &Schema,
        after: &Schema,
        name: Option<&str>,
        oracle: &dyn Oracle,
    ) -> Result<Option<Generated>> {
        let diff = Diff::compute(before, after, oracle)?;
        if diff.is_empty() {
            return Ok(None);
        }

        let plan = Plan::build(&diff, before, after, self.backend)?;
        let version = self.next_version()?;
        let id = MigrationId::new(version, name.unwrap_or(&diff.suggested_name()));
        let snapshot = after.to_json();
        let migration = write_migration(&id, &plan, Some(&after.checksum().short()));

        Ok(Some(Generated {
            path: self.directory.join(id.file_name("sql")),
            snapshot_path: self.snapshot_path(),
            id,
            migration,
            snapshot,
            advice: Advice::for_diff(&diff),
            diff,
        }))
    }

    /// The version the next migration gets: now, or one past the newest file if
    /// two are generated in the same second.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the directory cannot be listed.
    ///
    /// ```
    /// # use moso_migrate::generator::Generator;
    /// # use moso_orm::Backend;
    /// let version = Generator::new("does-not-exist", Backend::Postgres).next_version()?;
    /// assert!(version.year() >= 2026);
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn next_version(&self) -> Result<Version> {
        let mut version = self.now.unwrap_or_else(Version::now);
        let existing = crate::runner::read_directory(&self.directory)?;
        while existing.iter().any(|file| file.version() == version) {
            version = version.next();
        }
        Ok(version)
    }
}

#[cfg(test)]
mod tests {
    use moso_sql::DataType;

    use super::*;
    use crate::rename::{DropAndAdd, Scripted};
    use crate::schema::{Column, Table};

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

    fn generator() -> Generator {
        Generator::new("does-not-exist", Backend::Postgres)
            .at(Version::from_parts(2026, 7, 29, 10, 15, 0))
    }

    #[test]
    fn no_changes_produces_no_migration() {
        let schema = schema_with(users());
        let generated = generator()
            .make_migration_between(&schema, &schema, None, &DropAndAdd)
            .expect("diffs");
        assert!(generated.is_none());
    }

    #[test]
    fn the_generator_is_idempotent() {
        // Run one: empty -> users. Run two: the snapshot it wrote -> users.
        // The second must produce nothing. This is the acceptance criterion.
        let after = schema_with(users());
        let first = generator()
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");

        let snapshot = Schema::from_json(first.snapshot()).expect("round trips");
        let second = generator()
            .make_migration_between(&snapshot, &after, None, &DropAndAdd)
            .expect("diffs");
        assert!(second.is_none(), "the second run must find nothing");
    }

    #[test]
    fn idempotence_holds_for_every_construct_at_once() {
        let mut table = users();
        table.add_column(Column::new("locale", DataType::VarChar(Some(8))).with_default("'en'"));
        table.add_column(Column::new("bio", DataType::Text).nullable());
        table.add_column(Column::new("doc", DataType::JsonB).with_default("'{}'"));
        table.add_index(
            crate::schema::Index::new("users_email_key", ["email"])
                .unique()
                .backing_a_constraint(),
        );
        table.add_index(
            crate::schema::Index::over(
                "idx_users_lower_email",
                [crate::schema::IndexPart::expression("lower(email)")],
            )
            .r#where("bio is not null"),
        );
        table.add_check(crate::schema::Check::new("users_id_positive", "id > 0"));
        table.add_foreign_key(crate::schema::ForeignKey::new(
            "users_org_fkey",
            ["id"],
            "orgs",
            ["id"],
        ));

        let mut after = schema_with(table);
        after.add_enum(crate::schema::EnumType::new(
            "user_role",
            ["admin", "member"],
        ));
        after.add_extension("pg_trgm");

        let first = generator()
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        let snapshot = Schema::from_json(first.snapshot()).expect("round trips");
        assert_eq!(snapshot, after, "the snapshot IS the schema it came from");

        let second = generator()
            .make_migration_between(&snapshot, &after, None, &DropAndAdd)
            .expect("diffs");
        assert!(
            second.is_none(),
            "spurious diff: {:?}",
            second.map(|g| g.diff().summary())
        );
    }

    #[test]
    fn the_output_is_byte_stable() {
        let after = schema_with(users());
        let first = generator()
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        for _ in 0..4 {
            let again = generator()
                .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
                .expect("diffs")
                .expect("a migration");
            assert_eq!(again.migration(), first.migration());
            assert_eq!(again.snapshot(), first.snapshot());
        }
    }

    #[test]
    fn the_header_names_the_snapshot_it_came_from() {
        let after = schema_with(users());
        let generated = generator()
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        assert!(
            generated
                .migration()
                .contains(&format!("@{}", after.checksum().short())),
            "{}",
            generated.migration()
        );
    }

    #[test]
    fn a_name_is_suggested_when_none_is_given() {
        let generated = generator()
            .make_migration_between(&Schema::empty(), &schema_with(users()), None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        assert_eq!(generated.id().name(), "create_users");
        assert_eq!(
            generated.path().file_name().and_then(|n| n.to_str()),
            Some("20260729T101500_create_users.sql")
        );
    }

    #[test]
    fn a_given_name_is_slugified() {
        let generated = generator()
            .make_migration_between(
                &Schema::empty(),
                &schema_with(users()),
                Some("Create the users table!"),
                &DropAndAdd,
            )
            .expect("diffs")
            .expect("a migration");
        assert_eq!(generated.id().name(), "create_the_users_table");
    }

    #[test]
    fn a_rename_needs_the_oracle_and_then_produces_one_statement() {
        let before = schema_with(users());
        let mut renamed = Table::new("users").for_entity("User");
        renamed.add_column(Column::new("id", DataType::BigSerial).for_field("id"));
        renamed.add_column(Column::new("email_address", DataType::Text).for_field("email"));
        renamed.set_primary_key(["id"]);

        let oracle = Scripted::parse(["users.email:email_address"]).expect("parses");
        let generated = generator()
            .make_migration_between(&before, &schema_with(renamed), None, &oracle)
            .expect("diffs")
            .expect("a migration");
        assert!(
            generated
                .migration()
                .contains("RENAME COLUMN \"email\" TO \"email_address\""),
            "{}",
            generated.migration()
        );
        assert!(!generated.advice().is_empty(), "a rename earns advice");
    }

    #[test]
    fn a_destructive_change_is_written_commented_with_a_warning() {
        let mut before_table = users();
        before_table.add_column(Column::new("legacy_id", DataType::Integer).nullable());
        let generated = generator()
            .make_migration_between(
                &schema_with(before_table),
                &schema_with(users()),
                None,
                &DropAndAdd,
            )
            .expect("diffs")
            .expect("a migration");
        let text = generated.migration();
        assert!(text.contains("-- moso:destructive"), "{text}");
        assert!(text.contains("⚠ DESTRUCTIVE"), "{text}");
        assert!(
            text.contains("-- ALTER TABLE \"users\" DROP COLUMN \"legacy_id\""),
            "{text}"
        );
        assert_eq!(generated.advice().len(), 1);
    }

    #[test]
    fn the_migration_parses_back() {
        let after = schema_with(users());
        let generated = generator()
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        let parsed = crate::file::MigrationFile::parse(
            &generated.id().file_name("sql"),
            generated.migration(),
        )
        .expect("parses");
        assert!(!parsed.up().is_empty());
        assert!(parsed.is_reversible());
    }

    #[test]
    fn sqlite_and_postgres_produce_different_sql_for_the_same_diff() {
        let after = schema_with(users());
        let postgres = generator()
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");
        let sqlite = Generator::new("does-not-exist", Backend::Sqlite)
            .at(Version::from_parts(2026, 7, 29, 10, 15, 0))
            .make_migration_between(&Schema::empty(), &after, None, &DropAndAdd)
            .expect("diffs")
            .expect("a migration");

        assert!(postgres.migration().contains("bigserial"));
        assert!(
            sqlite
                .migration()
                .contains("integer PRIMARY KEY AUTOINCREMENT")
        );
        assert_eq!(
            postgres.snapshot(),
            sqlite.snapshot(),
            "the snapshot is dialect-neutral"
        );
    }
}
