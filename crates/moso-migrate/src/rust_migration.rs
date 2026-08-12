//! Migrations written in Rust, for the ones that need application logic.
//!
//! SQL is the default because SQL is what you review. A backfill that has to
//! compute a slug, re-encrypt a column, or call something is not SQL, and
//! pretending otherwise produces a 200-line `DO $$ … $$` block nobody can test.
//!
//! ```
//! use futures_util::future::BoxFuture;
//! use moso_migrate::rust_migration::{Migrator, RustMigration};
//! use moso_migrate::{Result, Version};
//!
//! pub struct BackfillSlugs;
//!
//! impl RustMigration for BackfillSlugs {
//!     fn version(&self) -> Version {
//!         Version::from_parts(2026, 7, 30, 9, 0, 0)
//!     }
//!
//!     fn name(&self) -> &str {
//!         "backfill_slugs"
//!     }
//!
//!     /// Runs outside a transaction so it can batch without holding locks.
//!     fn is_transactional(&self) -> bool {
//!         false
//!     }
//!
//!     fn up<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
//!         Box::pin(async move {
//!             migrator
//!                 .batched("SELECT id, title FROM posts WHERE slug IS NULL", 1000, |rows| async move {
//!                     Ok(rows
//!                         .iter()
//!                         .filter_map(|row| Some((row.first()?.as_ref()?, row.get(1)?.as_ref()?)))
//!                         .map(|(id, title)| {
//!                             format!(
//!                                 "UPDATE posts SET slug = {} WHERE id = {id}",
//!                                 moso_migrate::emit::quote_literal(&title.to_lowercase()),
//!                             )
//!                         })
//!                         .collect())
//!                 })
//!                 .await?;
//!             Ok(())
//!         })
//!     }
//! }
//!
//! assert_eq!(BackfillSlugs.name(), "backfill_slugs");
//! assert!(!BackfillSlugs.is_reversible());
//! ```

use std::future::Future;

use futures_util::future::BoxFuture;
use moso_orm::Backend;

use crate::conn::Connection;
use crate::error::Result;
use crate::hash::Checksum;
use crate::version::Version;

/// A migration written in Rust.
///
/// Dyn-compatible on purpose (decision D4): a registry holds
/// `Box<dyn RustMigration>`, so the futures are boxed.
///
/// ```
/// use futures_util::future::BoxFuture;
/// use moso_migrate::rust_migration::{Migrator, RustMigration};
/// use moso_migrate::{Result, Version};
///
/// struct Noop;
///
/// impl RustMigration for Noop {
///     fn version(&self) -> Version { Version::from_parts(2026, 1, 1, 0, 0, 0) }
///     fn name(&self) -> &str { "noop" }
///     fn up<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
///         Box::pin(async { Ok(()) })
///     }
/// }
///
/// let registry: Vec<Box<dyn RustMigration>> = vec![Box::new(Noop)];
/// assert_eq!(registry.len(), 1);
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Moso migration",
    label = "not a migration",
    note = "a Rust migration needs `version`, `name` and `up`; `down` and the two flags have \
            defaults",
    note = "help: implement it — \
            `impl RustMigration for {Self} {{ fn version(&self) -> Version {{ \
            Version::from_parts(2026, 1, 1, 0, 0, 0) }} fn name(&self) -> &str {{ \"…\" }} \
            fn up<'a>(&'a self, m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {{ \
            Box::pin(async {{ Ok(()) }}) }} }}`",
    note = "help: or write the migration as SQL, which is what `moso db make-migration` generates"
)]
pub trait RustMigration: Send + Sync + 'static {
    /// The version, which decides where it sorts among the SQL migrations.
    fn version(&self) -> Version;

    /// The name, which becomes the second half of the file name.
    fn name(&self) -> &str;

    /// Whether [`RustMigration::down`] does anything.
    ///
    /// `false` by default: most data migrations cannot be undone, and claiming
    /// otherwise is worse than admitting it.
    fn is_reversible(&self) -> bool {
        false
    }

    /// Whether it runs inside a transaction.
    ///
    /// `true` by default. A batched backfill should say `false` so that it does
    /// not hold locks for the length of the whole table.
    fn is_transactional(&self) -> bool {
        true
    }

    /// A stable identity for the checksum, so that editing the migration is
    /// detected the same way editing a SQL file is.
    ///
    /// The default hashes the version and the name, which catches a *renamed*
    /// migration but not an edited body — Rust has no way to hash a function.
    /// Override it with a hash of whatever decides the behaviour if that
    /// matters to you.
    fn fingerprint(&self) -> Checksum {
        Checksum::of(format!("{}:{}", self.version(), self.name()).as_bytes())
    }

    /// Applies it.
    fn up<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>>;

    /// Undoes it. The default does nothing, matching
    /// [`RustMigration::is_reversible`]'s default of `false`.
    fn down<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
        let _ = migrator;
        Box::pin(async { Ok(()) })
    }
}

/// What a Rust migration is given to work with.
///
/// Deliberately small: raw SQL, rows as text, and a batching helper. It is not
/// the ORM. An entity's Rust type is the *current* one, and a migration written
/// six months ago has to keep working against the schema of six months ago —
/// which is exactly why every migration framework that lets you use the ORM
/// inside a migration eventually tells you not to.
///
/// ```no_run
/// use moso_migrate::rust_migration::Migrator;
///
/// # async fn example(migrator: &mut Migrator<'_>) -> moso_migrate::Result<()> {
/// migrator.execute("UPDATE posts SET slug = lower(title) WHERE slug IS NULL").await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct Migrator<'a> {
    connection: &'a mut Connection,
    statements: usize,
}

impl<'a> Migrator<'a> {
    /// Wraps a connection.
    ///
    /// ```no_run
    /// # async fn example(connection: &mut moso_migrate::conn::Connection) {
    /// let migrator = moso_migrate::rust_migration::Migrator::new(connection);
    /// assert_eq!(migrator.statements_run(), 0);
    /// # }
    /// ```
    #[must_use]
    pub fn new(connection: &'a mut Connection) -> Self {
        Self {
            connection,
            statements: 0,
        }
    }

    /// Which database this is, for a migration that has to spell something two
    /// ways.
    ///
    /// ```no_run
    /// # async fn example(migrator: &moso_migrate::rust_migration::Migrator<'_>) {
    /// if migrator.backend() == moso_orm::Backend::Postgres {
    ///     // …
    /// }
    /// # }
    /// ```
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.connection.backend()
    }

    /// How many statements it has run, which is what the runner reports.
    ///
    /// ```no_run
    /// # async fn example(migrator: &moso_migrate::rust_migration::Migrator<'_>) {
    /// println!("{} statements", migrator.statements_run());
    /// # }
    /// ```
    #[must_use]
    pub const fn statements_run(&self) -> usize {
        self.statements
    }

    /// Runs one statement.
    ///
    /// # Errors
    ///
    /// [`Error::Database`](crate::Error::Database) naming the statement.
    ///
    /// ```no_run
    /// # async fn example(migrator: &mut moso_migrate::rust_migration::Migrator<'_>) -> moso_migrate::Result<()> {
    /// let updated = migrator.execute("UPDATE posts SET slug = '' WHERE slug IS NULL").await?;
    /// println!("{updated} rows");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn execute(&mut self, sql: &str) -> Result<u64> {
        self.statements += 1;
        self.connection.execute(sql).await
    }

    /// Runs a query and returns its rows as text.
    ///
    /// # Errors
    ///
    /// [`Error::Database`](crate::Error::Database) naming the statement.
    ///
    /// ```no_run
    /// # async fn example(migrator: &mut moso_migrate::rust_migration::Migrator<'_>) -> moso_migrate::Result<()> {
    /// let rows = migrator.fetch("SELECT id FROM posts").await?;
    /// println!("{} posts", rows.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn fetch(&mut self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        self.statements += 1;
        self.connection.fetch_text(sql).await
    }

    /// Reads a query in batches and applies the statements each batch produces.
    ///
    /// The closure takes the rows and returns the SQL to run for them, rather
    /// than being handed the connection back. That is a deliberate narrowing of
    /// the shape `docs/02-data/23-migrations.md` sketches: handing a `&mut`
    /// connection into a closure that the same `&mut` connection is driving
    /// does not type-check without either an `Rc<RefCell<..>>` or a
    /// higher-ranked boxed future that infects the signature. Returning
    /// statements keeps the borrow straight and keeps the closure testable
    /// without a database.
    ///
    /// Batching is by `LIMIT`/`OFFSET` over the caller's query. The caller's
    /// query must therefore be stable under the updates it produces — the
    /// usual shape, `WHERE slug IS NULL`, shrinks as it goes, so this walks it
    /// from the front each time rather than paging.
    ///
    /// # Errors
    ///
    /// [`Error::Database`](crate::Error::Database) from either the read or any
    /// statement the closure produced.
    ///
    /// ```no_run
    /// # async fn example(migrator: &mut moso_migrate::rust_migration::Migrator<'_>) -> moso_migrate::Result<()> {
    /// let rows = migrator
    ///     .batched("SELECT id FROM posts WHERE slug IS NULL", 500, |rows| async move {
    ///         Ok(rows
    ///             .iter()
    ///             .filter_map(|row| row.first()?.clone())
    ///             .map(|id| format!("UPDATE posts SET slug = 'x' WHERE id = {id}"))
    ///             .collect())
    ///     })
    ///     .await?;
    /// println!("{rows} rows");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn batched<F, Fut>(
        &mut self,
        select: &str,
        batch: usize,
        mut apply: F,
    ) -> Result<usize>
    where
        F: FnMut(Vec<Vec<Option<String>>>) -> Fut,
        Fut: Future<Output = Result<Vec<String>>>,
    {
        let batch = batch.max(1);
        let mut total = 0_usize;
        loop {
            let page = format!("{select} LIMIT {batch}");
            let rows = self.fetch(&page).await?;
            if rows.is_empty() {
                return Ok(total);
            }
            let read = rows.len();
            let statements = apply(rows).await?;
            if statements.is_empty() {
                // The closure produced nothing, so the next read would return
                // the same rows for ever. Stopping is the only safe answer.
                return Ok(total);
            }
            for statement in &statements {
                self.execute(statement).await?;
            }
            total += read;
            if read < batch {
                return Ok(total);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Connection;

    struct AddARow;

    impl RustMigration for AddARow {
        fn version(&self) -> Version {
            Version::from_parts(2026, 1, 1, 0, 0, 0)
        }

        fn name(&self) -> &str {
            "add_a_row"
        }

        fn is_reversible(&self) -> bool {
            true
        }

        fn up<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                migrator.execute("INSERT INTO t (id) VALUES (1)").await?;
                Ok(())
            })
        }

        fn down<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                migrator.execute("DELETE FROM t WHERE id = 1").await?;
                Ok(())
            })
        }
    }

    #[tokio::test]
    async fn a_rust_migration_runs_and_reverses() {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        connection
            .execute("CREATE TABLE t (id integer primary key)")
            .await
            .expect("creates");

        let migration = AddARow;
        {
            let mut migrator = Migrator::new(&mut connection);
            migration.up(&mut migrator).await.expect("runs");
            assert_eq!(migrator.statements_run(), 1);
        }
        assert_eq!(
            connection
                .count_rows("SELECT * FROM t")
                .await
                .expect("counts"),
            1
        );

        {
            let mut migrator = Migrator::new(&mut connection);
            migration.down(&mut migrator).await.expect("reverses");
        }
        assert_eq!(
            connection
                .count_rows("SELECT * FROM t")
                .await
                .expect("counts"),
            0
        );
    }

    #[tokio::test]
    async fn batched_walks_the_whole_query() {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        connection
            .execute("CREATE TABLE posts (id integer primary key, slug text)")
            .await
            .expect("creates");
        for id in 1..=25 {
            connection
                .execute(&format!("INSERT INTO posts (id) VALUES ({id})"))
                .await
                .expect("inserts");
        }

        let mut migrator = Migrator::new(&mut connection);
        let touched = migrator
            .batched(
                "SELECT id FROM posts WHERE slug IS NULL",
                10,
                |rows| async move {
                    Ok(rows
                        .iter()
                        .filter_map(|row| row.first().cloned().flatten())
                        .map(|id| format!("UPDATE posts SET slug = 'p{id}' WHERE id = {id}"))
                        .collect())
                },
            )
            .await
            .expect("batches");
        assert_eq!(touched, 25);

        let remaining = connection
            .count_rows("SELECT id FROM posts WHERE slug IS NULL")
            .await
            .expect("counts");
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn batched_stops_rather_than_looping_when_nothing_changes() {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        connection
            .execute("CREATE TABLE posts (id integer primary key)")
            .await
            .expect("creates");
        connection
            .execute("INSERT INTO posts (id) VALUES (1)")
            .await
            .expect("inserts");

        let mut migrator = Migrator::new(&mut connection);
        let touched = migrator
            .batched(
                "SELECT id FROM posts",
                10,
                |_rows| async move { Ok(Vec::new()) },
            )
            .await
            .expect("stops");
        assert_eq!(touched, 0);
    }

    #[test]
    fn the_defaults_are_conservative() {
        assert!(!AddARow.is_transactional() || AddARow.is_transactional());
        struct Bare;
        impl RustMigration for Bare {
            fn version(&self) -> Version {
                Version::from_parts(2026, 1, 1, 0, 0, 0)
            }
            fn name(&self) -> &str {
                "bare"
            }
            fn up<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        assert!(
            !Bare.is_reversible(),
            "irreversible unless it says otherwise"
        );
        assert!(
            Bare.is_transactional(),
            "transactional unless it says otherwise"
        );
    }

    #[test]
    fn fingerprints_distinguish_migrations() {
        struct A;
        struct B;
        impl RustMigration for A {
            fn version(&self) -> Version {
                Version::from_parts(2026, 1, 1, 0, 0, 0)
            }
            fn name(&self) -> &str {
                "a"
            }
            fn up<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        impl RustMigration for B {
            fn version(&self) -> Version {
                Version::from_parts(2026, 1, 1, 0, 0, 0)
            }
            fn name(&self) -> &str {
                "b"
            }
            fn up<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async { Ok(()) })
            }
        }
        assert_ne!(A.fingerprint(), B.fingerprint());
    }
}
