//! `moso db squash` — collapsing old migrations into one baseline.
//!
//! After two years a project has four hundred migrations, and a fresh
//! development database takes ten minutes to build. Squashing replaces the old
//! ones with a single `CREATE TABLE …` baseline that produces the same schema.
//!
//! # The rule that makes it safe
//!
//! A database that has already applied the collapsed migrations must **not**
//! run the baseline: it would try to create tables that exist. The baseline
//! therefore carries `-- moso:replaces`, and the runner records it as applied —
//! without running it — when every version it replaces is already in the
//! ledger. This is Django's `replaces`, and it is the only mechanism that works
//! for a team where some databases are old and some are new.
//!
//! ```
//! use moso_migrate::squash::Squash;
//! use moso_migrate::schema::{Column, Schema, Table};
//! use moso_migrate::{MigrationId, Version};
//! use moso_orm::Backend;
//! use moso_sql::DataType;
//!
//! let mut schema = Schema::empty();
//! let mut users = Table::new("users");
//! users.add_column(Column::new("id", DataType::BigSerial));
//! users.set_primary_key(["id"]);
//! schema.add_table(users);
//!
//! let id = MigrationId::new(Version::from_parts(2026, 7, 29, 10, 15, 0), "baseline");
//! let squash = Squash::build(
//!     &schema,
//!     &[Version::from_parts(2026, 1, 1, 0, 0, 0)],
//!     &id,
//!     Backend::Postgres,
//! )?;
//! assert!(squash.migration().contains("-- moso:replaces 20260101T000000"));
//! # Ok::<(), moso_migrate::Error>(())
//! ```

use std::path::{Path, PathBuf};

use moso_orm::Backend;

use crate::diff::Diff;
use crate::error::Result;
use crate::file::{MigrationFile, write_migration};
use crate::plan::Plan;
use crate::rename::DropAndAdd;
use crate::schema::Schema;
use crate::version::{MigrationId, Version};

/// A baseline migration and the files it replaces.
///
/// ```
/// # fn example(squash: &moso_migrate::squash::Squash) {
/// println!("{} files collapse", squash.replaced().len());
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct Squash {
    id: MigrationId,
    migration: String,
    replaced: Vec<Version>,
    removable: Vec<PathBuf>,
}

impl Squash {
    /// Builds a baseline that produces `schema` and replaces `replaced`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when the schema cannot
    /// be created on the backend — the same refusals
    /// [`Plan::build`](crate::plan::Plan::build) makes.
    ///
    /// ```
    /// # use moso_migrate::squash::Squash;
    /// # use moso_migrate::{MigrationId, Schema, Version};
    /// # use moso_orm::Backend;
    /// let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "baseline");
    /// let squash = Squash::build(&Schema::empty(), &[], &id, Backend::Postgres)?;
    /// assert!(squash.replaced().is_empty());
    /// # Ok::<(), moso_migrate::Error>(())
    /// ```
    pub fn build(
        schema: &Schema,
        replaced: &[Version],
        id: &MigrationId,
        backend: Backend,
    ) -> Result<Self> {
        let diff = Diff::compute(&Schema::empty(), schema, &DropAndAdd)?;
        let plan = Plan::build(&diff, &Schema::empty(), schema, backend)?;
        let body = write_migration(id, &plan, Some(&schema.checksum().short()));

        // The `replaces` directive has to be in the header, above the first
        // marker, so the parser sees it.
        let mut migration = String::with_capacity(body.len() + 64);
        let mut inserted = false;
        for line in body.lines() {
            if !inserted && line.starts_with("-- moso:") {
                migration.push_str(&format!(
                    "-- moso:replaces {}\n",
                    replaced
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(",")
                ));
                inserted = true;
            }
            migration.push_str(line);
            migration.push('\n');
        }

        Ok(Self {
            id: id.clone(),
            migration,
            replaced: replaced.to_vec(),
            removable: Vec::new(),
        })
    }

    /// Plans a squash over a directory: everything before `before` collapses.
    ///
    /// # Errors
    ///
    /// [`Error::Io`](crate::Error::Io) when the directory cannot be read, plus
    /// everything [`Squash::build`] returns.
    ///
    /// ```no_run
    /// use moso_migrate::squash::Squash;
    /// use moso_migrate::{Schema, Version};
    /// use moso_orm::Backend;
    ///
    /// # fn example(schema: &Schema) -> moso_migrate::Result<()> {
    /// let squash = Squash::over_directory(
    ///     "migrations",
    ///     Version::from_parts(2026, 1, 1, 0, 0, 0),
    ///     schema,
    ///     Backend::Postgres,
    ///     Version::now(),
    /// )?;
    /// println!("{} files replaced", squash.replaced().len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn over_directory(
        directory: impl AsRef<Path>,
        before: Version,
        schema: &Schema,
        backend: Backend,
        at: Version,
    ) -> Result<Self> {
        let directory = directory.as_ref();
        let files = crate::runner::read_directory(directory)?;
        let collapsed: Vec<&MigrationFile> = files
            .iter()
            .filter(|file| file.version() < before)
            .collect();

        let replaced: Vec<Version> = collapsed.iter().map(|file| file.version()).collect();
        // The baseline has to sort *before* everything it does not replace, so
        // that a fresh database applies it first. Its version is therefore the
        // oldest collapsed one, not `now` — which is also what makes a squash
        // idempotent under `git pull`.
        let version = replaced.first().copied().unwrap_or(at);
        let id = MigrationId::new(version, "squashed");

        let mut squash = Self::build(schema, &replaced, &id, backend)?;
        squash.removable = collapsed
            .iter()
            .map(|file| directory.join(file.id().file_name("sql")))
            .collect();
        Ok(squash)
    }

    /// The baseline's identity.
    ///
    /// ```
    /// # fn example(squash: &moso_migrate::squash::Squash) {
    /// println!("{}", squash.id());
    /// # }
    /// ```
    #[must_use]
    pub const fn id(&self) -> &MigrationId {
        &self.id
    }

    /// The baseline's text.
    ///
    /// ```
    /// # fn example(squash: &moso_migrate::squash::Squash) {
    /// assert!(squash.migration().contains("-- +migrate up"));
    /// # }
    /// ```
    #[must_use]
    pub fn migration(&self) -> &str {
        &self.migration
    }

    /// The versions it stands in for.
    ///
    /// ```
    /// # fn example(squash: &moso_migrate::squash::Squash) {
    /// println!("{} replaced", squash.replaced().len());
    /// # }
    /// ```
    #[must_use]
    pub fn replaced(&self) -> &[Version] {
        &self.replaced
    }

    /// The files that can be deleted once the baseline is committed.
    ///
    /// Returned rather than deleted: a squash is a change to version-controlled
    /// history, and deleting files behind someone's back during a `--dry-run`
    /// would be unforgivable.
    ///
    /// ```
    /// # fn example(squash: &moso_migrate::squash::Squash) {
    /// for path in squash.removable() {
    ///     println!("rm {}", path.display());
    /// }
    /// # }
    /// ```
    #[must_use]
    pub fn removable(&self) -> &[PathBuf] {
        &self.removable
    }

    /// Writes the baseline and deletes the files it replaces.
    ///
    /// # Errors
    ///
    /// [`Error::Io`](crate::Error::Io) naming the path that failed. The
    /// baseline is written first, so a failure half-way leaves a directory with
    /// both — which is recoverable — rather than with neither.
    ///
    /// ```no_run
    /// # fn example(squash: &moso_migrate::squash::Squash) -> moso_migrate::Result<()> {
    /// squash.apply("migrations")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn apply(&self, directory: impl AsRef<Path>) -> Result<()> {
        let directory = directory.as_ref();
        let path = directory.join(self.id.file_name("sql"));
        std::fs::write(&path, &self.migration).map_err(|source| {
            crate::Error::io(
                "writing",
                &path,
                "check the directory's permissions",
                source,
            )
        })?;
        for removable in &self.removable {
            if removable == &path {
                continue;
            }
            std::fs::remove_file(removable).map_err(|source| {
                crate::Error::io(
                    "removing",
                    removable,
                    "the baseline was written; delete the collapsed files by hand",
                    source,
                )
            })?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use moso_sql::DataType;

    use super::*;
    use crate::schema::{Column, Table};

    fn schema() -> Schema {
        let mut schema = Schema::empty();
        let mut users = Table::new("users");
        users.add_column(Column::new("id", DataType::BigSerial));
        users.add_column(Column::new("email", DataType::Text));
        users.set_primary_key(["id"]);
        schema.add_table(users);
        schema
    }

    #[test]
    fn a_baseline_creates_the_whole_schema() {
        let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "squashed");
        let squash = Squash::build(
            &schema(),
            &[
                Version::from_parts(2026, 1, 1, 0, 0, 0),
                Version::from_parts(2026, 2, 1, 0, 0, 0),
            ],
            &id,
            Backend::Postgres,
        )
        .expect("builds");

        assert!(squash.migration().contains("CREATE TABLE \"users\""));
        assert!(
            squash
                .migration()
                .contains("-- moso:replaces 20260101T000000,20260201T000000"),
            "{}",
            squash.migration()
        );
    }

    #[test]
    fn the_baseline_parses_and_declares_what_it_replaces() {
        let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "squashed");
        let squash = Squash::build(
            &schema(),
            &[Version::from_parts(2026, 1, 1, 0, 0, 0)],
            &id,
            Backend::Postgres,
        )
        .expect("builds");

        let parsed =
            MigrationFile::parse(&id.file_name("sql"), squash.migration()).expect("parses");
        assert_eq!(
            parsed.replaces(),
            [Version::from_parts(2026, 1, 1, 0, 0, 0)]
        );
        assert!(!parsed.up().is_empty());
    }

    #[test]
    fn an_empty_schema_squashes_to_an_empty_baseline() {
        let id = MigrationId::new(Version::from_parts(2026, 1, 1, 0, 0, 0), "squashed");
        let squash = Squash::build(&Schema::empty(), &[], &id, Backend::Postgres).expect("builds");
        assert!(squash.replaced().is_empty());
        assert!(
            squash.migration().contains("-- moso:replaces \n")
                || squash.migration().contains("-- moso:replaces\n")
        );
    }

    #[test]
    fn squashing_a_directory_takes_the_oldest_version() {
        let dir = std::env::temp_dir().join(format!("moso-squash-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("creates");
        for name in [
            "20260101T000000_a.sql",
            "20260201T000000_b.sql",
            "20260301T000000_c.sql",
        ] {
            std::fs::write(dir.join(name), "-- +migrate up\nSELECT 1;\n").expect("writes");
        }

        let squash = Squash::over_directory(
            &dir,
            Version::from_parts(2026, 3, 1, 0, 0, 0),
            &schema(),
            Backend::Postgres,
            Version::now(),
        )
        .expect("plans");

        assert_eq!(squash.replaced().len(), 2);
        assert_eq!(
            squash.id().version(),
            Version::from_parts(2026, 1, 1, 0, 0, 0)
        );
        assert_eq!(squash.removable().len(), 2);

        squash.apply(&dir).expect("applies");
        let remaining = crate::runner::read_directory(&dir).expect("reads");
        let names: Vec<String> = remaining
            .iter()
            .map(|file| file.id().file_name("sql"))
            .collect();
        assert_eq!(
            names,
            ["20260101T000000_squashed.sql", "20260301T000000_c.sql"]
        );

        std::fs::remove_dir_all(&dir).expect("cleans up");
    }
}
