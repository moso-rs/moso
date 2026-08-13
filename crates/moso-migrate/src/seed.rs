//! `moso db seed` — putting data in a development database.
//!
//! A seed is not a migration: it is not versioned, it is not recorded, and it
//! is meant to be run again. `docs/02-data/23-migrations.md` says seeds should
//! be "idempotent by convention (upsert on natural keys)", which is a
//! convention this module cannot enforce and does not pretend to — what it does
//! enforce is that a seed cannot run against production by accident.
//!
//! # What is here and what is not
//!
//! The document's example uses `#[moso::seed]` and `#[derive(Factory)]`. The
//! attribute macro belongs to `moso-macros` and the factory to `moso-test`;
//! this module is the runtime half they will register into, and it is usable on
//! its own today.
//!
//! ```
//! use futures_util::future::BoxFuture;
//! use moso_migrate::rust_migration::Migrator;
//! use moso_migrate::seed::{Seed, Seeds};
//! use moso_migrate::Result;
//!
//! struct Dev;
//!
//! impl Seed for Dev {
//!     fn name(&self) -> &str {
//!         "dev"
//!     }
//!
//!     fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
//!         Box::pin(async move {
//!             migrator
//!                 .execute("INSERT INTO users (email) VALUES ('admin@local') ON CONFLICT DO NOTHING")
//!                 .await?;
//!             Ok(())
//!         })
//!     }
//! }
//!
//! let mut seeds = Seeds::default();
//! seeds.add(Dev);
//! assert_eq!(seeds.names(), ["dev"]);
//! ```

use futures_util::future::BoxFuture;

use crate::conn::Connection;
use crate::error::{Error, Result};
use crate::rust_migration::Migrator;

/// One named set of fixture data.
///
/// ```
/// use futures_util::future::BoxFuture;
/// use moso_migrate::rust_migration::Migrator;
/// use moso_migrate::seed::Seed;
/// use moso_migrate::Result;
///
/// struct Empty;
///
/// impl Seed for Empty {
///     fn name(&self) -> &str { "empty" }
///     fn run<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
///         Box::pin(async { Ok(()) })
///     }
/// }
///
/// assert_eq!(Empty.name(), "empty");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a Moso seed",
    label = "not a seed",
    note = "a seed needs a name and a body; both are required because `moso db seed --file` picks \
            one by name",
    note = "help: implement it — \
            `impl Seed for {Self} {{ fn name(&self) -> &str {{ \"dev\" }} \
            fn run<'a>(&'a self, m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {{ \
            Box::pin(async {{ Ok(()) }}) }} }}`"
)]
pub trait Seed: Send + Sync + 'static {
    /// The name `moso db seed <name>` takes.
    fn name(&self) -> &str;

    /// Whether it may run in production with `--force`.
    ///
    /// `false` by default. A seed that creates an `admin@local` account is a
    /// security incident in production, and the default should reflect that.
    fn is_safe_in_production(&self) -> bool {
        false
    }

    /// Inserts the data.
    fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>>;
}

/// How a seed run is allowed to behave.
///
/// ```
/// use moso_migrate::seed::SeedOptions;
///
/// assert!(!SeedOptions::default().is_forced());
/// assert_eq!(SeedOptions::default().active_profile(), "dev");
/// ```
#[derive(Clone, Debug)]
pub struct SeedOptions {
    profile: String,
    force: bool,
    transactional: bool,
}

impl Default for SeedOptions {
    fn default() -> Self {
        Self {
            profile: "dev".to_owned(),
            force: false,
            transactional: true,
        }
    }
}

impl SeedOptions {
    /// The active profile.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert_eq!(SeedOptions::default().profile("test").active_profile(), "test");
    /// ```
    #[must_use]
    pub fn profile(mut self, profile: impl Into<String>) -> Self {
        self.profile = profile.into();
        self
    }

    /// Runs even in production.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert!(SeedOptions::default().force().is_forced());
    /// ```
    #[must_use]
    pub const fn force(mut self) -> Self {
        self.force = true;
        self
    }

    /// Runs every seed outside a transaction.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert!(!SeedOptions::default().without_a_transaction().is_transactional());
    /// ```
    #[must_use]
    pub const fn without_a_transaction(mut self) -> Self {
        self.transactional = false;
        self
    }

    /// The profile.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert_eq!(SeedOptions::default().active_profile(), "dev");
    /// ```
    #[must_use]
    pub fn active_profile(&self) -> &str {
        &self.profile
    }

    /// Whether `--force` was given.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert!(!SeedOptions::default().is_forced());
    /// ```
    #[must_use]
    pub const fn is_forced(&self) -> bool {
        self.force
    }

    /// Whether each seed runs in a transaction.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert!(SeedOptions::default().is_transactional());
    /// ```
    #[must_use]
    pub const fn is_transactional(&self) -> bool {
        self.transactional
    }

    /// Whether the profile is one where a seed is refused by default.
    ///
    /// ```
    /// # use moso_migrate::seed::SeedOptions;
    /// assert!(SeedOptions::default().profile("production").is_production());
    /// ```
    #[must_use]
    pub fn is_production(&self) -> bool {
        matches!(self.profile.as_str(), "production" | "prod" | "live")
    }
}

/// A registry of seeds.
///
/// ```
/// let seeds = moso_migrate::seed::Seeds::default();
/// assert!(seeds.names().is_empty());
/// ```
#[derive(Default)]
pub struct Seeds {
    seeds: Vec<Box<dyn Seed>>,
}

impl std::fmt::Debug for Seeds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seeds")
            .field("names", &self.names())
            .finish()
    }
}

impl Seeds {
    /// Registers one.
    ///
    /// ```
    /// # use futures_util::future::BoxFuture;
    /// # use moso_migrate::rust_migration::Migrator;
    /// # use moso_migrate::seed::{Seed, Seeds};
    /// # use moso_migrate::Result;
    /// # struct Dev;
    /// # impl Seed for Dev {
    /// #     fn name(&self) -> &str { "dev" }
    /// #     fn run<'a>(&'a self, _m: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
    /// #         Box::pin(async { Ok(()) })
    /// #     }
    /// # }
    /// let mut seeds = Seeds::default();
    /// seeds.add(Dev);
    /// assert_eq!(seeds.names().len(), 1);
    /// ```
    pub fn add(&mut self, seed: impl Seed) {
        self.seeds.push(Box::new(seed));
    }

    /// The registered names, in registration order.
    ///
    /// ```
    /// assert!(moso_migrate::seed::Seeds::default().names().is_empty());
    /// ```
    #[must_use]
    pub fn names(&self) -> Vec<&str> {
        self.seeds.iter().map(|seed| seed.name()).collect()
    }

    /// Runs one seed by name, or every seed when `name` is `None`.
    ///
    /// # Errors
    ///
    /// [`Error::RefusedInProduction`] when the profile is production and
    /// neither `--force` nor the seed's own opt-in allows it;
    /// [`Error::NeedsAnswer`] when the name matches nothing, with the available
    /// names in the help line; whatever the seed's own body returns.
    ///
    /// ```no_run
    /// # use moso_migrate::seed::{Seeds, SeedOptions};
    /// # async fn example(
    /// #     seeds: &Seeds,
    /// #     connection: &mut moso_migrate::conn::Connection,
    /// # ) -> moso_migrate::Result<()> {
    /// let ran = seeds.run(connection, Some("dev"), &SeedOptions::default()).await?;
    /// println!("{ran:?}");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn run(
        &self,
        connection: &mut Connection,
        name: Option<&str>,
        options: &SeedOptions,
    ) -> Result<Vec<String>> {
        let selected: Vec<&Box<dyn Seed>> = match name {
            Some(name) => {
                let matched: Vec<&Box<dyn Seed>> = self
                    .seeds
                    .iter()
                    .filter(|seed| seed.name() == name)
                    .collect();
                if matched.is_empty() {
                    return Err(Error::NeedsAnswer {
                        question: format!("there is no seed called `{name}`"),
                        flag: format!(
                            "moso db seed <name>, where <name> is one of: {}",
                            if self.seeds.is_empty() {
                                "(none registered)".to_owned()
                            } else {
                                self.names().join(", ")
                            }
                        ),
                    });
                }
                matched
            }
            None => self.seeds.iter().collect(),
        };

        let mut ran = Vec::with_capacity(selected.len());
        for seed in selected {
            if options.is_production() && !options.is_forced() && !seed.is_safe_in_production() {
                return Err(Error::RefusedInProduction {
                    command: "moso db seed",
                    profile: options.active_profile().to_owned(),
                    help: "seeds create fixture data — an `admin@local` account in production is \
                           an incident, not a convenience; pass `--force` if you are certain",
                });
            }

            if options.is_transactional() {
                connection.execute("BEGIN").await?;
            }
            let outcome = {
                let mut migrator = Migrator::new(connection);
                seed.run(&mut migrator).await
            };
            match outcome {
                Ok(()) => {
                    if options.is_transactional() {
                        connection.execute("COMMIT").await?;
                    }
                    ran.push(seed.name().to_owned());
                }
                Err(error) => {
                    if options.is_transactional() {
                        let _ = connection.execute("ROLLBACK").await;
                    }
                    return Err(error);
                }
            }
        }
        Ok(ran)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dev;

    impl Seed for Dev {
        fn name(&self) -> &str {
            "dev"
        }

        fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                migrator
                    .execute("INSERT INTO users (email) VALUES ('admin@local')")
                    .await?;
                Ok(())
            })
        }
    }

    struct Broken;

    impl Seed for Broken {
        fn name(&self) -> &str {
            "broken"
        }

        fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
            Box::pin(async move {
                migrator
                    .execute("INSERT INTO users (email) VALUES ('a@b')")
                    .await?;
                migrator.execute("INSERT INTO nope (x) VALUES (1)").await?;
                Ok(())
            })
        }
    }

    async fn database() -> Connection {
        let mut connection = Connection::open("sqlite::memory:").await.expect("opens");
        connection
            .execute("CREATE TABLE users (email text primary key)")
            .await
            .expect("creates");
        connection
    }

    #[tokio::test]
    async fn a_seed_runs_and_reports_its_name() {
        let mut connection = database().await;
        let mut seeds = Seeds::default();
        seeds.add(Dev);

        let ran = seeds
            .run(&mut connection, Some("dev"), &SeedOptions::default())
            .await
            .expect("runs");
        assert_eq!(ran, ["dev"]);
        assert_eq!(
            connection
                .count_rows("SELECT * FROM users")
                .await
                .expect("counts"),
            1
        );
        connection.close().await;
    }

    #[tokio::test]
    async fn a_failing_seed_rolls_back() {
        let mut connection = database().await;
        let mut seeds = Seeds::default();
        seeds.add(Broken);

        seeds
            .run(&mut connection, None, &SeedOptions::default())
            .await
            .expect_err("no such table");
        assert_eq!(
            connection
                .count_rows("SELECT * FROM users")
                .await
                .expect("counts"),
            0,
            "the first insert rolled back too"
        );
        connection.close().await;
    }

    #[tokio::test]
    async fn production_is_refused_and_force_allows_it() {
        let mut connection = database().await;
        let mut seeds = Seeds::default();
        seeds.add(Dev);

        let options = SeedOptions::default().profile("production");
        let error = seeds
            .run(&mut connection, None, &options)
            .await
            .expect_err("refused");
        assert!(error.to_string().contains("--force"), "{error}");
        assert!(error.to_string().contains("incident"), "{error}");

        seeds
            .run(&mut connection, None, &options.clone().force())
            .await
            .expect("forced");
        connection.close().await;
    }

    #[tokio::test]
    async fn a_seed_can_opt_in_to_production() {
        struct Reference;
        impl Seed for Reference {
            fn name(&self) -> &str {
                "countries"
            }
            fn is_safe_in_production(&self) -> bool {
                true
            }
            fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    migrator
                        .execute("INSERT INTO users (email) VALUES ('ref@x')")
                        .await?;
                    Ok(())
                })
            }
        }

        let mut connection = database().await;
        let mut seeds = Seeds::default();
        seeds.add(Reference);
        seeds
            .run(
                &mut connection,
                None,
                &SeedOptions::default().profile("production"),
            )
            .await
            .expect("reference data is allowed");
        connection.close().await;
    }

    #[tokio::test]
    async fn an_unknown_name_lists_the_known_ones() {
        let mut connection = database().await;
        let mut seeds = Seeds::default();
        seeds.add(Dev);
        let error = seeds
            .run(&mut connection, Some("nope"), &SeedOptions::default())
            .await
            .expect_err("unknown");
        assert!(error.to_string().contains("dev"), "{error}");
        connection.close().await;
    }

    #[tokio::test]
    async fn seeds_are_re_runnable_when_they_are_written_that_way() {
        struct Upsert;
        impl Seed for Upsert {
            fn name(&self) -> &str {
                "upsert"
            }
            fn run<'a>(&'a self, migrator: &'a mut Migrator<'_>) -> BoxFuture<'a, Result<()>> {
                Box::pin(async move {
                    migrator
                        .execute(
                            "INSERT INTO users (email) VALUES ('admin@local') \
                             ON CONFLICT (email) DO NOTHING",
                        )
                        .await?;
                    Ok(())
                })
            }
        }

        let mut connection = database().await;
        let mut seeds = Seeds::default();
        seeds.add(Upsert);
        for _ in 0..3 {
            seeds
                .run(&mut connection, None, &SeedOptions::default())
                .await
                .expect("runs");
        }
        assert_eq!(
            connection
                .count_rows("SELECT * FROM users")
                .await
                .expect("counts"),
            1
        );
        connection.close().await;
    }
}
