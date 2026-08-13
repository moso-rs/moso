//! [`Db`] — the handle, its configuration, and the pool underneath.
//!
//! `Db` is `Clone` and cheap (one `Arc`), so it is registered once with
//! `App::provide` and reached with `Inject<Db>`. Everything that runs a
//! statement takes `impl Executor<'_>`, which `&Db` implements, so the same
//! service function works inside and outside a transaction.
//!
//! # What opening a pool does, in order
//!
//! 1. [`DatabaseConfig::validate`] rejects a configuration no pool could
//!    honour, naming the field. Nothing has been opened yet, so a typo is a
//!    boot error rather than a deadlock under load.
//! 2. The URL picks the [`Backend`], and with it the dialect statements render
//!    for.
//! 3. On PostgreSQL, one probe connection asks whether a transaction-mode
//!    pooler is in the way — see [`PgBouncerMode`] — because that answer
//!    changes whether prepared statements may be cached at all.
//! 4. The pool is built with the configured sizes and timeouts, an
//!    `application_name` so `pg_stat_activity` can name this process, and a
//!    session `statement_timeout` and `lock_timeout`.
//! 5. Replica pools are opened, and a sampler starts measuring their lag every
//!    five seconds.
//!
//! # The three things that are load-bearing at 3 a.m.
//!
//! * **The pool is small by default.** [`DatabaseConfig::max_connections`].
//! * **Acquiring times out.** An exhausted pool is [`Error::PoolTimeout`],
//!   which renders as `503` with a `Retry-After`, never as a request that
//!   waits forever.
//! * **Reads follow writes.** [`Db::read`] returns the primary for
//!   [`DatabaseConfig::sticky_window`] after a write, because a replica that is
//!   two hundred milliseconds behind produces a bug class that only ever
//!   reproduces in production.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use core::time::Duration;
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

use moso_core::health::HealthStatus;
use moso_sql::{Dialect, Postgres, Sqlite};

use crate::error::{Error, Result};
use crate::executor::StatementCounter;
use crate::tx::{Tx, TxOptions};

// A build with neither `postgres` nor `sqlite` is not a shipping configuration:
// `Db::connect` fails at runtime through `open_pool`'s fallback arm with an
// `Error::Configuration` that names the missing driver. It is deliberately left
// to *compile* — inert, with each backend enum carrying an uninhabited
// `Unbacked` variant so no `match` over it is empty — so that
// `cargo hack check --each-feature` (gate G3) can build the crate with each
// feature on its own. A `compile_error!` here would fail that gate, and the
// runtime fallback already gives an operator a clear message.

// The configuration and tenancy types live in their own files and are
// re-exported here, because `moso_orm::db::DatabaseConfig` is the path the rest
// of the workspace already imports.
#[path = "config.rs"]
mod config;
#[path = "tenant.rs"]
mod tenant;

pub use crate::db::config::{DatabaseConfig, PgBouncerMode, ReplicaConfig, TlsMode};
pub use crate::db::tenant::{TenancyModel, TenantId, TenantRouter, TenantSource, UrlTemplate};

#[cfg(feature = "postgres")]
pub(crate) use crate::db::config::redact_password;

/// How often a replica's lag is measured.
///
/// Five seconds, so that a replica whose lag crosses `max_lag` is out of
/// rotation inside ten — which is the acceptance criterion in
/// `docs/02-data/24-transactions-pooling.md`.
const LAG_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// Which database a handle is connected to.
///
/// Runtime, not compile-time: a `DATABASE_URL` decides it, so a test can run
/// against SQLite and production against PostgreSQL with the same binary.
///
/// ```
/// use moso_orm::Backend;
///
/// assert_eq!(Backend::from_url("postgres://localhost/app").unwrap(), Backend::Postgres);
/// assert_eq!(Backend::from_url("sqlite://app.db").unwrap(), Backend::Sqlite);
/// assert!(Backend::from_url("mysql://localhost/app").is_err());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Backend {
    /// PostgreSQL 14 or later. The reference dialect (ADR-0010).
    Postgres,
    /// SQLite 3.40 or later. Fully supported, with documented divergences.
    Sqlite,
}

impl Backend {
    /// The backend a connection URL names.
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] naming the schemes that are supported, for a
    /// URL that names anything else.
    ///
    /// ```
    /// use moso_orm::Backend;
    ///
    /// assert_eq!(Backend::from_url("postgresql://h/db").unwrap(), Backend::Postgres);
    /// ```
    pub fn from_url(url: &str) -> Result<Self> {
        let scheme = url.split_once("://").map_or(url, |(scheme, _)| scheme);
        match scheme {
            "postgres" | "postgresql" => Ok(Self::Postgres),
            "sqlite" => Ok(Self::Sqlite),
            other => Err(Error::Configuration {
                detail: format!(
                    "`{other}` is not a database Moso can open\n  \
                     help: the URL must start with `postgres://`, `postgresql://` or `sqlite://`\n  \
                     note: MySQL is not in this release (ADR-0010)"
                ),
            }),
        }
    }

    /// The dialect that renders statements for this backend.
    ///
    /// ```
    /// use moso_orm::Backend;
    ///
    /// assert_eq!(Backend::Postgres.dialect().name(), "PostgreSQL");
    /// ```
    #[must_use]
    pub fn dialect(self) -> &'static dyn Dialect {
        const POSTGRES: Postgres = Postgres;
        const SQLITE: Sqlite = Sqlite;
        match self {
            Self::Postgres => &POSTGRES,
            Self::Sqlite => &SQLITE,
        }
    }

    /// The backend's name, as it appears in logs and errors.
    ///
    /// ```
    /// use moso_orm::Backend;
    ///
    /// assert_eq!(Backend::Sqlite.as_str(), "SQLite");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "PostgreSQL",
            Self::Sqlite => "SQLite",
        }
    }

    /// Whether the backend can run several statements concurrently on one
    /// handle. SQLite cannot, which is why its pool is capped at one writer.
    ///
    /// ```
    /// use moso_orm::Backend;
    ///
    /// assert!(Backend::Postgres.supports_concurrent_writers());
    /// assert!(!Backend::Sqlite.supports_concurrent_writers());
    /// ```
    #[must_use]
    pub const fn supports_concurrent_writers(self) -> bool {
        matches!(self, Self::Postgres)
    }

    /// The cheapest statement that proves a connection works.
    ///
    /// ```
    /// use moso_orm::Backend;
    ///
    /// assert_eq!(Backend::Postgres.ping_statement(), "select 1");
    /// ```
    #[must_use]
    pub const fn ping_statement(self) -> &'static str {
        "select 1"
    }

    /// Whether this backend was compiled in.
    ///
    /// Both are, by default. A deployment that turns one off gets a
    /// configuration error naming the feature rather than a link error.
    ///
    /// ```
    /// use moso_orm::Backend;
    ///
    /// assert!(Backend::Postgres.is_compiled_in() || Backend::Sqlite.is_compiled_in());
    /// ```
    #[must_use]
    pub const fn is_compiled_in(self) -> bool {
        match self {
            Self::Postgres => cfg!(feature = "postgres"),
            Self::Sqlite => cfg!(feature = "sqlite"),
        }
    }
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The database handle.
///
/// ```no_run
/// use moso_orm::{DatabaseConfig, Db};
///
/// # async fn open() -> moso_orm::Result<()> {
/// let db = Db::connect(&DatabaseConfig::from_url("postgres://localhost/shop")).await?;
/// assert_eq!(db.backend(), moso_orm::Backend::Postgres);
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Db {
    inner: Arc<DbInner>,
}

/// What a [`Db`] shares between its clones.
struct DbInner {
    backend: Backend,
    config: DatabaseConfig,
    pool: PoolHandle,
    replicas: Vec<Db>,
    counter: Arc<StatementCounter>,
    tenant: Option<TenantId>,
    prefer_primary: bool,
    /// Whether prepared statements may be cached and reused.
    persistent: bool,
    /// The pool's real maximum, which is the configured one except where a
    /// backend forces something else — see [`effective_max_connections`].
    effective_max: u32,
    /// The read-your-writes clock, shared with every clone and every replica
    /// handle so that a write anywhere is visible to `read()` everywhere.
    sticky: Arc<Sticky>,
    /// Replica health, and the cursor the weighted round-robin advances.
    rotation: Arc<Rotation>,
    /// Where pool samples go, when the application asked for them.
    metrics: Option<Arc<dyn DbMetrics>>,
    /// Tasks waiting to acquire a connection, which is the number to alert on.
    waiting: AtomicU32,
    /// Per-tenant pools, for the two tenancy models that route connections.
    tenant_pools: Arc<Mutex<tenant::TenantPools<Db>>>,
    /// Why this handle cannot run anything, when `for_tenant` was asked for a
    /// tenant it could not route. Carried rather than panicked, so that the
    /// infallible `for_tenant` signature never lies about which rows it reads.
    poisoned: Option<Arc<str>>,
}

/// The driver pool behind a [`Db`].
///
/// Private: which driver a handle holds is decided by its URL, and widening
/// this enum must not be a breaking change.
pub(crate) enum PoolHandle {
    /// A PostgreSQL pool.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::PgPool),
    /// A SQLite pool.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::SqlitePool),
    /// Present only when no database backend is compiled in, so the enum is
    /// never zero-variant — which would make every `match` over it
    /// non-exhaustive under `cargo hack --each-feature`. Uninhabited: it holds
    /// no value and no runtime build ever constructs it.
    #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
    Unbacked(core::convert::Infallible),
}

/// A connection checked out of a [`PoolHandle`].
pub(crate) enum PooledConnection {
    /// A PostgreSQL connection.
    #[cfg(feature = "postgres")]
    Postgres(sqlx::pool::PoolConnection<sqlx::Postgres>),
    /// A SQLite connection.
    #[cfg(feature = "sqlite")]
    Sqlite(sqlx::pool::PoolConnection<sqlx::Sqlite>),
    /// Present only when no database backend is compiled in, so the enum is
    /// never zero-variant — which would make every `match` over it
    /// non-exhaustive under `cargo hack --each-feature`. Uninhabited: it holds
    /// no value and no runtime build ever constructs it.
    #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
    Unbacked(core::convert::Infallible),
}

impl Db {
    /// Opens a pool.
    ///
    /// The pool is lazy: this validates the URL, resolves the backend and
    /// prepares the pool, and the first connection is opened on demand unless
    /// `min_connections` is above zero.
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] for a URL Moso cannot open, and
    /// [`Error::Connection`] when `min_connections` is above zero and the
    /// server refuses.
    ///
    /// ```no_run
    /// use moso_orm::{DatabaseConfig, Db};
    ///
    /// # async fn open() -> moso_orm::Result<Db> {
    /// Db::connect(&DatabaseConfig::from_url("sqlite://:memory:")).await
    /// # }
    /// ```
    pub async fn connect(config: &DatabaseConfig) -> Result<Self> {
        config.validate()?;
        let backend = config.backend()?;
        if !backend.is_compiled_in() {
            return Err(Error::Configuration {
                detail: format!(
                    "this build of `moso-orm` has no {backend} driver\n  \
                     help: turn the `{}` cargo feature back on",
                    match backend {
                        Backend::Postgres => "postgres",
                        Backend::Sqlite => "sqlite",
                    }
                ),
            });
        }

        // The pooler probe runs before the pool is built, because its answer
        // decides whether the pool may cache prepared statements at all.
        let pooler = detect_pooler(config, backend).await?;
        let persistent = config.statement_cache_allowed(pooler);
        let pool = open_pool(config, backend, persistent).await?;

        let mut replicas = Vec::with_capacity(config.replicas.len());
        for replica in &config.replicas {
            let handle = Box::pin(Self::connect(&replica.pool_config(config))).await?;
            replicas.push(handle);
        }

        let rotation = Arc::new(Rotation::new(&config.replicas));
        let sticky = Arc::new(Sticky::new());
        let db = Self {
            inner: Arc::new(DbInner {
                backend,
                config: config.clone(),
                pool,
                replicas,
                counter: Arc::new(StatementCounter::new()),
                tenant: None,
                prefer_primary: false,
                persistent,
                effective_max: effective_max_connections(config, backend),
                sticky: Arc::clone(&sticky),
                rotation: Arc::clone(&rotation),
                metrics: None,
                waiting: AtomicU32::new(0),
                tenant_pools: Arc::new(Mutex::new(tenant::TenantPools::default())),
                poisoned: None,
            }),
        };

        tracing::info!(
            backend = backend.as_str(),
            url = %config.redacted_url(),
            replicas = db.inner.replicas.len(),
            tenancy = config.tenancy.as_str(),
            prepared_statements = persistent,
            "{}",
            config.boot_summary()
        );
        if !persistent {
            tracing::warn!(
                "db: prepared-statement caching is off because a transaction-mode connection \
                 pooler was detected (or declared). Statements are re-parsed on every execution; \
                 use session pooling, or point Moso at the server directly, to get the cache back."
            );
        }

        if !db.inner.replicas.is_empty() {
            spawn_lag_sampler(Arc::downgrade(&rotation), db.inner.replicas.clone());
        }
        Ok(db)
    }

    /// Opens a pool with the default settings for `url`.
    ///
    /// # Errors
    ///
    /// As [`Db::connect`].
    ///
    /// ```no_run
    /// use moso_orm::Db;
    ///
    /// # async fn open() -> moso_orm::Result<Db> {
    /// Db::connect_url("postgres://moso:moso@localhost:5432/shop").await
    /// # }
    /// ```
    pub async fn connect_url(url: &str) -> Result<Self> {
        Self::connect(&DatabaseConfig::from_url(url)).await
    }

    /// Which backend this handle talks to.
    ///
    /// ```no_run
    /// # use moso_orm::{Backend, Db};
    /// fn is_pg(db: &Db) -> bool {
    ///     db.backend() == Backend::Postgres
    /// }
    /// ```
    #[must_use]
    pub fn backend(&self) -> Backend {
        self.inner.backend
    }

    /// The dialect statements are rendered for.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn dialect_name(db: &Db) -> &'static str {
    ///     db.dialect().name()
    /// }
    /// ```
    #[must_use]
    pub fn dialect(&self) -> &'static dyn Dialect {
        self.inner.backend.dialect()
    }

    /// The configuration this handle was opened with.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn pool_size(db: &Db) -> u32 {
    ///     db.config().max_connections
    /// }
    /// ```
    #[must_use]
    pub fn config(&self) -> &DatabaseConfig {
        &self.inner.config
    }

    /// Runs `operation` in a transaction, retrying transient failures.
    ///
    /// Commits on `Ok`, rolls back on `Err` **or panic**. Retries on
    /// serialisation failure and deadlock up to
    /// [`TxOptions::max_retries`], with jittered exponential backoff — which is
    /// why the argument is a closure rather than a handle: a retry has to be
    /// able to re-run the body.
    ///
    /// Side effects outside the database must not be inside the closure. It can
    /// run more than once.
    ///
    /// # Errors
    ///
    /// Whatever `operation` returns, or a commit failure. A retryable error
    /// that exhausts `max_retries` is returned as it was last seen.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result};
    /// # async fn example(db: &Db) -> Result<i64> {
    /// let total = db
    ///     .transaction(async |tx| {
    ///         let _ = tx;
    ///         Ok(1_i64)
    ///     })
    ///     .await?;
    /// # Ok(total)
    /// # }
    /// ```
    pub async fn transaction<F, T>(&self, operation: F) -> Result<T>
    where
        F: AsyncFnMut(&Tx) -> Result<T>,
    {
        self.transaction_with(TxOptions::default(), operation).await
    }

    /// Runs `operation` in a transaction with explicit options.
    ///
    /// # Errors
    ///
    /// As [`Db::transaction`].
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Isolation, Result, TxOptions};
    /// # async fn example(db: &Db) -> Result<()> {
    /// db.transaction_with(TxOptions::new().isolation(Isolation::Serializable), async |tx| {
    ///     let _ = tx;
    ///     Ok(())
    /// })
    /// .await
    /// # }
    /// ```
    pub async fn transaction_with<F, T>(&self, options: TxOptions, mut operation: F) -> Result<T>
    where
        F: AsyncFnMut(&Tx) -> Result<T>,
    {
        let mut attempt: u32 = 0;
        loop {
            let tx = self.begin_with(options.clone()).await?;

            // The closure's future is dropped here on either path. A panic
            // inside it unwinds through this frame, dropping `tx`, which is
            // what rolls the transaction back and hands the connection back to
            // the pool unpoisoned.
            let outcome = match operation(&tx).await {
                Ok(value) => match tx.commit().await {
                    Ok(()) => return Ok(value),
                    Err(error) => Err(error),
                },
                Err(error) => {
                    // A rollback failure is not the error worth reporting: the
                    // reason the body failed is.
                    let _ = tx.rollback().await;
                    Err(error)
                }
            };

            let error = match outcome {
                Ok(()) => unreachable!("the success path returns above"),
                Err(error) => error,
            };

            if !crate::tx::is_transient_conflict(&error) || attempt >= options.max_retries {
                return Err(error);
            }
            attempt += 1;
            let reason = if matches!(error, Error::Deadlock { .. }) {
                "deadlock"
            } else {
                "serialization"
            };
            if let Some(metrics) = &self.inner.metrics {
                metrics.retry(&RetrySample { reason, attempt });
            }
            let wait = options.backoff_jittered(attempt);
            tracing::warn!(
                reason,
                attempt,
                max_retries = options.max_retries,
                backoff_ms = wait.as_millis(),
                "db: retrying the transaction"
            );
            tokio::time::sleep(wait).await;
        }
    }

    /// Opens a transaction explicitly, for the cases a closure does not fit.
    ///
    /// There is no retry: retrying needs a re-runnable body, which only the
    /// closure form has. Prefer [`Db::transaction`].
    ///
    /// # Errors
    ///
    /// [`Error::PoolTimeout`] or [`Error::Connection`].
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result, Tx};
    /// # async fn example(db: &Db) -> Result<Tx> {
    /// let tx = db.begin().await?;
    /// # Ok(tx)
    /// # }
    /// ```
    pub async fn begin(&self) -> Result<Tx> {
        self.begin_with(TxOptions::default()).await
    }

    /// Opens a transaction with explicit options.
    ///
    /// # Errors
    ///
    /// As [`Db::begin`].
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result, Tx, TxOptions};
    /// # async fn example(db: &Db) -> Result<Tx> {
    /// db.begin_with(TxOptions::new().read_only()).await
    /// # }
    /// ```
    pub async fn begin_with(&self, options: TxOptions) -> Result<Tx> {
        Tx::open(self.clone(), options).await
    }

    /// A handle that reads from a replica.
    ///
    /// Picks by weighted round-robin, skipping replicas whose measured lag is
    /// over `max_lag`. Falls back to the primary when nothing is configured or
    /// nothing is healthy — and, after a write on this handle, for
    /// `sticky_window`, so that read-your-writes holds by default.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn reader(db: &Db) -> &Db {
    ///     db.read()
    /// }
    /// ```
    #[must_use]
    pub fn read(&self) -> &Db {
        if self.prefers_primary() {
            return self;
        }
        self.read_stale()
    }

    /// A replica handle that ignores the read-your-writes sticky window.
    ///
    /// For the reads that genuinely tolerate staleness — a dashboard, a
    /// nightly count.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn analytics(db: &Db) -> &Db {
    ///     db.read_stale()
    /// }
    /// ```
    #[must_use]
    pub fn read_stale(&self) -> &Db {
        match self.inner.rotation.pick() {
            Some(index) => self.inner.replicas.get(index).unwrap_or(self),
            None => self,
        }
    }

    /// The primary handle, for a read that must see this request's writes.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn writer(db: &Db) -> &Db {
    ///     db.primary()
    /// }
    /// ```
    #[must_use]
    pub fn primary(&self) -> &Db {
        self
    }

    /// A handle bound to one tenant.
    ///
    /// Every query built from it carries the tenant scope, which is what makes
    /// `Invoice::query()` runnable without a per-query `.scoped(..)`.
    ///
    /// Under [`TenancyModel::SchemaPerTenant`] and
    /// [`TenancyModel::DatabasePerTenant`] this also routes the connection: the
    /// returned handle draws from that tenant's own lazily-opened pool, and at
    /// most [`DatabaseConfig::max_tenant_pools`] such pools stay open.
    ///
    /// The signature cannot fail, so a tenant that cannot be routed — a key
    /// that is not usable as an identifier, a template with no placeholder —
    /// produces a handle that refuses every statement with the reason. It never
    /// produces one that silently reads the wrong tenant's rows. Use
    /// [`Db::try_for_tenant`] to see the error at the call site instead.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, TenantId};
    /// fn for_acme(db: &Db) -> Db {
    ///     db.for_tenant(TenantId::of(1_i64))
    /// }
    /// ```
    #[must_use]
    pub fn for_tenant(&self, tenant: TenantId) -> Db {
        match self.try_for_tenant(tenant.clone()) {
            Ok(db) => db,
            Err(error) => {
                let reason = error.to_string();
                tracing::error!(%tenant, "db: this tenant cannot be routed: {reason}");
                self.derive(|inner| {
                    inner.tenant = Some(tenant);
                    inner.poisoned = Some(Arc::from(reason.as_str()));
                })
            }
        }
    }

    /// A handle bound to one tenant, reporting why it could not be built.
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] for a tenant key that cannot name a schema or a
    /// database, or a tenancy model whose template is unusable.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result, TenantId};
    /// # fn example(db: &Db) -> Result<Db> {
    /// db.try_for_tenant(TenantId::of(1_i64))
    /// # }
    /// ```
    pub fn try_for_tenant(&self, tenant: TenantId) -> Result<Db> {
        match &self.inner.config.tenancy {
            TenancyModel::Discriminator => Ok(self.derive(|inner| inner.tenant = Some(tenant))),
            TenancyModel::SchemaPerTenant { prefix } => {
                let schema = tenant.schema(prefix)?;
                self.routed_handle(&tenant, |config| {
                    let mut config = config.clone();
                    config.replicas = Vec::new();
                    config.application_name =
                        format!("{} (schema {schema})", config.application_name);
                    (config, Some(schema.as_str().to_owned()))
                })
            }
            TenancyModel::DatabasePerTenant { .. } => {
                let url = self.inner.config.tenancy.tenant_url(&tenant)?;
                self.routed_handle(&tenant, |config| {
                    let mut config = config.clone();
                    config.url = moso_core::config::SecretString::new(url.clone());
                    config.replicas = Vec::new();
                    config.application_name = format!("{} (tenant)", config.application_name);
                    (config, None)
                })
            }
        }
    }

    /// The tenant this handle is bound to, when it is bound to one.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn tenant_of(db: &Db) -> bool {
    ///     db.tenant().is_some()
    /// }
    /// ```
    #[must_use]
    pub fn tenant(&self) -> Option<&TenantId> {
        self.inner.tenant.as_ref()
    }

    /// The raw sqlx pool, when this handle is connected to PostgreSQL.
    ///
    /// Non-negotiable N8, the escape hatch: everything sqlx can do that Moso
    /// does not wrap is reachable from here. Its type is sqlx's, not Moso's, so
    /// it is the one place where sqlx's major version is visible in Moso's API —
    /// deliberately, and named as such in `xtask/allow/sealed.toml`.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn is_pg(db: &Db) -> bool {
    ///     db.postgres_pool().is_some()
    /// }
    /// ```
    #[cfg(feature = "postgres")]
    #[must_use]
    pub fn postgres_pool(&self) -> Option<&sqlx::PgPool> {
        match &self.inner.pool {
            PoolHandle::Postgres(pool) => Some(pool),
            #[cfg(feature = "sqlite")]
            PoolHandle::Sqlite(_) => None,
        }
    }

    /// The raw sqlx pool, when this handle is connected to SQLite.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn is_sqlite(db: &Db) -> bool {
    ///     db.sqlite_pool().is_some()
    /// }
    /// ```
    #[cfg(feature = "sqlite")]
    #[must_use]
    pub fn sqlite_pool(&self) -> Option<&sqlx::SqlitePool> {
        match &self.inner.pool {
            PoolHandle::Sqlite(pool) => Some(pool),
            #[cfg(feature = "postgres")]
            PoolHandle::Postgres(_) => None,
        }
    }

    /// A snapshot of the pool's occupancy.
    ///
    /// Exported as metrics automatically; this is for a health endpoint or a
    /// test that asserts connections were returned.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn idle(db: &Db) -> u32 {
    ///     db.stats().idle
    /// }
    /// ```
    #[must_use]
    pub fn stats(&self) -> PoolStats {
        // The annotation pins the tuple type: in a backend-less build every
        // real arm is `cfg`-ed out, so only the diverging `Unbacked` arm
        // remains and inference has nothing else to read the type from.
        let (size, idle): (u32, usize) = match &self.inner.pool {
            #[cfg(feature = "postgres")]
            PoolHandle::Postgres(pool) => (pool.size(), pool.num_idle()),
            #[cfg(feature = "sqlite")]
            PoolHandle::Sqlite(pool) => (pool.size(), pool.num_idle()),
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            PoolHandle::Unbacked(never) => match *never {},
        };
        let idle = u32::try_from(idle).unwrap_or(u32::MAX).min(size);
        PoolStats::new(
            size,
            idle,
            size - idle,
            self.inner.waiting.load(Ordering::Relaxed),
            self.inner.effective_max,
        )
    }

    /// The statement counter, which is what `assert_queries!` reads.
    ///
    /// Counting is always on: it is one relaxed increment per statement, and
    /// the N+1 detector and the dev warning both need it.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn so_far(db: &Db) -> u64 {
    ///     db.statements().total()
    /// }
    /// ```
    #[must_use]
    pub fn statements(&self) -> &StatementCounter {
        &self.inner.counter
    }

    /// Whether this handle prefers the primary for reads, because a write
    /// happened inside the sticky window.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn sticky(db: &Db) -> bool {
    ///     db.prefers_primary()
    /// }
    /// ```
    #[must_use]
    pub fn prefers_primary(&self) -> bool {
        self.inner.prefer_primary || self.inner.sticky.within(self.inner.config.sticky_window)
    }

    /// The replica handles, in configuration order.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn replica_count(db: &Db) -> usize {
    ///     db.replicas().len()
    /// }
    /// ```
    #[must_use]
    pub fn replicas(&self) -> &[Db] {
        &self.inner.replicas
    }

    /// A handle whose read-your-writes window is its own.
    ///
    /// The sticky window is shared between clones by default, so a write
    /// anywhere in the process keeps every read on the primary for
    /// [`DatabaseConfig::sticky_window`]. That is always *correct* — it can
    /// only send a read somewhere fresher — and on a write-heavy service it
    /// costs the replicas most of their traffic. A request-scoped handle made
    /// here narrows the window to one request, which is what
    /// `docs/02-data/24-transactions-pooling.md` describes.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn per_request(db: &Db) -> Db {
    ///     db.request_scoped()
    /// }
    /// ```
    #[must_use]
    pub fn request_scoped(&self) -> Db {
        self.derive(|inner| {
            inner.sticky = Arc::new(Sticky::new());
            inner.counter = Some(Arc::new(StatementCounter::new()));
        })
    }

    /// Records that a write happened, so that reads stay on the primary for
    /// [`DatabaseConfig::sticky_window`].
    ///
    /// The executor calls this for every statement that is not a `SELECT`, so
    /// applications rarely need it. It is public for the raw-SQL escape hatch
    /// (non-negotiable N8): a write sent through [`Db::postgres_pool`] is
    /// invisible to Moso, and this is how a caller says so.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// fn after_a_raw_write(db: &Db) {
    ///     db.mark_write();
    ///     assert!(db.prefers_primary());
    /// }
    /// ```
    pub fn mark_write(&self) {
        self.inner.sticky.mark();
    }

    /// Sends the cheapest statement the backend has, to prove the pool works.
    ///
    /// # Errors
    ///
    /// [`Error::Connection`] or [`Error::PoolTimeout`].
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result};
    /// # async fn probe(db: &Db) -> Result<()> {
    /// db.ping().await
    /// # }
    /// ```
    pub async fn ping(&self) -> Result<()> {
        let mut connection = self.acquire().await?;
        let statement = self.inner.backend.ping_statement();
        match &mut connection {
            #[cfg(feature = "postgres")]
            PooledConnection::Postgres(conn) => {
                use sqlx::Executor as _;
                conn.execute(statement)
                    .await
                    .map_err(|error| self.translate(error, statement))?;
            }
            #[cfg(feature = "sqlite")]
            PooledConnection::Sqlite(conn) => {
                use sqlx::Executor as _;
                conn.execute(statement)
                    .await
                    .map_err(|error| self.translate(error, statement))?;
            }
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            PooledConnection::Unbacked(never) => match *never {},
        }
        Ok(())
    }

    /// Whether the pool answers, and whether the replicas are keeping up.
    ///
    /// `Degraded` rather than `Down` for a lagging replica: the instance can
    /// still serve, and taking it out of rotation for a replication hiccup
    /// turns a slow replica into an outage.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// # use moso_core::health::HealthStatus;
    /// # async fn probe(db: &Db) -> HealthStatus {
    /// db.health().await
    /// # }
    /// ```
    pub async fn health(&self) -> HealthStatus {
        if let Some(reason) = &self.inner.poisoned {
            return HealthStatus::Down(reason.to_string());
        }
        if let Err(error) = self.ping().await {
            return HealthStatus::Down(error.to_string());
        }
        let lagging = self.inner.rotation.lagging();
        if lagging > 0 {
            return HealthStatus::Degraded(format!(
                "{lagging} of {} read replicas are past `max_lag` and out of rotation",
                self.inner.replicas.len()
            ));
        }
        let stats = self.stats();
        if stats.is_saturated() {
            return HealthStatus::Degraded(format!(
                "the connection pool is full and {} task(s) are waiting",
                stats.waiting
            ));
        }
        HealthStatus::Up
    }

    /// A readiness check over this handle, for `App::health_check`.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// # use moso_orm::db::DatabaseCheck;
    /// fn check(db: &Db) -> DatabaseCheck {
    ///     db.health_check()
    /// }
    /// ```
    #[must_use]
    pub fn health_check(&self) -> DatabaseCheck {
        DatabaseCheck::new(self.clone())
    }

    /// Sends pool and query samples to `recorder`.
    ///
    /// Moso does not depend on a metrics facade — that is a choice every
    /// application has an opinion about — so this takes the same shape as
    /// `moso_core`'s request recorder: one dyn-compatible trait an exporter
    /// crate implements in twenty lines.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_orm::Db;
    /// # use moso_orm::db::{DbMetrics, QuerySample};
    /// /// Counts statements and throws the rest away.
    /// struct Count(std::sync::atomic::AtomicU64);
    /// impl DbMetrics for Count {
    ///     fn query(&self, _: &QuerySample<'_>) {
    ///         self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    ///     }
    /// }
    ///
    /// fn instrument(db: &Db) -> Db {
    ///     db.with_metrics(Arc::new(Count(std::sync::atomic::AtomicU64::new(0))))
    /// }
    /// ```
    #[must_use]
    pub fn with_metrics(&self, recorder: Arc<dyn DbMetrics>) -> Db {
        self.derive(|inner| inner.metrics = Some(recorder))
    }

    /// Takes a PostgreSQL advisory lock held until it is released.
    ///
    /// The lock lives on a connection of its own, checked out of the pool for
    /// as long as the guard exists — a session-level lock taken on a pooled
    /// connection that then went back to the pool would be held by whoever got
    /// the connection next, which is a deadlock nobody can find.
    ///
    /// Prefer [`Tx::advisory_lock`], which is scoped to a transaction and needs
    /// no guard at all.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] on SQLite, which has no advisory locks, and
    /// whatever acquiring a connection fails with.
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result};
    /// # use moso_orm::db::AdvisoryKey;
    /// # async fn example(db: &Db) -> Result<()> {
    /// let lock = db.advisory_lock(AdvisoryKey::hashed("nightly-rollup")).await?;
    /// // … the work only one instance may do …
    /// lock.unlock().await
    /// # }
    /// ```
    pub async fn advisory_lock(&self, key: AdvisoryKey) -> Result<AdvisoryLock> {
        self.take_advisory_lock(key, true)
            .await
            .map(|held| held.expect("a blocking lock waits rather than declining"))
    }

    /// Takes a PostgreSQL advisory lock, or reports that someone else has it.
    ///
    /// # Errors
    ///
    /// As [`Db::advisory_lock`].
    ///
    /// ```no_run
    /// # use moso_orm::{Db, Result};
    /// # use moso_orm::db::AdvisoryKey;
    /// # async fn example(db: &Db) -> Result<bool> {
    /// let held = db.try_advisory_lock(AdvisoryKey::of(42)).await?;
    /// Ok(held.is_some())
    /// # }
    /// ```
    pub async fn try_advisory_lock(&self, key: AdvisoryKey) -> Result<Option<AdvisoryLock>> {
        self.take_advisory_lock(key, false).await
    }

    /// Closes the pool, waiting for in-flight statements to finish.
    ///
    /// Called by the application's shutdown lifespan. Idempotent.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// # async fn shutdown(db: &Db) {
    /// db.close().await;
    /// # }
    /// ```
    pub async fn close(&self) {
        self.inner.rotation.closed.store(true, Ordering::Relaxed);
        let tenants = {
            let mut pools = self
                .inner
                .tenant_pools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            pools.drain()
        };
        for tenant in tenants {
            Box::pin(tenant.close()).await;
        }
        for replica in &self.inner.replicas {
            Box::pin(replica.close()).await;
        }
        match &self.inner.pool {
            #[cfg(feature = "postgres")]
            PoolHandle::Postgres(pool) => pool.close().await,
            #[cfg(feature = "sqlite")]
            PoolHandle::Sqlite(pool) => pool.close().await,
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            PoolHandle::Unbacked(never) => match *never {},
        }
    }

    // ── crate internals ───────────────────────────────────────────────────

    /// A clone of this handle with one or two fields changed.
    ///
    /// Everything that is `Arc` — the pool, the counter, the sticky clock, the
    /// rotation — is shared, so a derived handle is the same pool seen through
    /// a different lens rather than a second pool.
    fn derive(&self, edit: impl FnOnce(&mut DerivedDb)) -> Db {
        let mut derived = DerivedDb {
            pool: None,
            counter: None,
            tenant: self.inner.tenant.clone(),
            prefer_primary: self.inner.prefer_primary,
            sticky: Arc::clone(&self.inner.sticky),
            metrics: self.inner.metrics.clone(),
            config: None,
            poisoned: self.inner.poisoned.clone(),
        };
        edit(&mut derived);

        let shares_pool = derived.pool.is_none();
        Db {
            inner: Arc::new(DbInner {
                backend: self.inner.backend,
                config: derived.config.unwrap_or_else(|| self.inner.config.clone()),
                pool: derived
                    .pool
                    .unwrap_or_else(|| self.inner.pool.shallow_clone()),
                replicas: if shares_pool {
                    self.inner.replicas.clone()
                } else {
                    Vec::new()
                },
                counter: derived
                    .counter
                    .unwrap_or_else(|| Arc::clone(&self.inner.counter)),
                tenant: derived.tenant,
                prefer_primary: derived.prefer_primary,
                persistent: self.inner.persistent,
                effective_max: self.inner.effective_max,
                sticky: derived.sticky,
                rotation: if shares_pool {
                    Arc::clone(&self.inner.rotation)
                } else {
                    Arc::new(Rotation::new(&[]))
                },
                metrics: derived.metrics,
                waiting: AtomicU32::new(0),
                tenant_pools: Arc::clone(&self.inner.tenant_pools),
                poisoned: derived.poisoned,
            }),
        }
    }

    /// The handle for a tenant that gets its own pool, opening one lazily and
    /// evicting the least recently used past the cap.
    fn routed_handle(
        &self,
        tenant: &TenantId,
        build: impl FnOnce(&DatabaseConfig) -> (DatabaseConfig, Option<String>),
    ) -> Result<Db> {
        let key = tenant.key();
        {
            let mut pools = self
                .inner
                .tenant_pools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = pools.touch(&key) {
                return Ok(existing);
            }
        }

        let (config, schema) = build(&self.inner.config);
        let pool = open_pool_lazily(&config, self.inner.backend, self.inner.persistent, schema)?;
        let handle = self.derive(|inner| {
            inner.pool = Some(pool);
            inner.config = Some(config);
            inner.tenant = Some(tenant.clone());
            inner.poisoned = None;
        });

        let evicted = {
            let mut pools = self
                .inner
                .tenant_pools
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(existing) = pools.touch(&key) {
                return Ok(existing);
            }
            pools.insert(key, handle.clone(), self.inner.config.max_tenant_pools)
        };
        if let Some(evicted) = evicted {
            close_in_background(evicted);
        }
        Ok(handle)
    }

    /// Takes a connection out of the pool, or explains why it could not.
    pub(crate) async fn acquire(&self) -> Result<PooledConnection> {
        if let Some(reason) = &self.inner.poisoned {
            return Err(Error::Configuration {
                detail: reason.to_string(),
            });
        }
        let started = Instant::now();
        self.inner.waiting.fetch_add(1, Ordering::Relaxed);
        // Annotated so a backend-less build, where only the diverging
        // `Unbacked` arm survives, still knows the result type.
        let acquired: core::result::Result<PooledConnection, sqlx::Error> = match &self.inner.pool {
            #[cfg(feature = "postgres")]
            PoolHandle::Postgres(pool) => pool.acquire().await.map(PooledConnection::Postgres),
            #[cfg(feature = "sqlite")]
            PoolHandle::Sqlite(pool) => pool.acquire().await.map(PooledConnection::Sqlite),
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            PoolHandle::Unbacked(never) => match *never {},
        };
        self.inner.waiting.fetch_sub(1, Ordering::Relaxed);
        let waited = started.elapsed();

        if let Some(metrics) = &self.inner.metrics {
            metrics.acquire(&AcquireSample {
                waited,
                timed_out: acquired.is_err(),
            });
        }

        acquired.map_err(|error| match error {
            sqlx::Error::PoolTimedOut => {
                tracing::warn!(
                    waited_ms = waited.as_millis(),
                    size = self.inner.effective_max,
                    "db: the connection pool is exhausted"
                );
                Error::PoolTimeout {
                    waited,
                    size: self.inner.effective_max,
                }
            }
            other => self.translate(other, ""),
        })
    }

    /// Whether prepared statements may be cached on this handle's connections.
    pub(crate) fn persistent(&self) -> bool {
        self.inner.persistent
    }

    /// Where samples go, when the application asked for them.
    pub(crate) fn metrics(&self) -> Option<&Arc<dyn DbMetrics>> {
        self.inner.metrics.as_ref()
    }

    /// The driver pool, for the two callers that need it directly: opening a
    /// transaction, and streaming rows without buffering them.
    pub(crate) fn pool_handle(&self) -> &PoolHandle {
        &self.inner.pool
    }

    /// Turns a driver error into one that names the problem (N7).
    pub(crate) fn translate(&self, error: sqlx::Error, sql: &str) -> Error {
        crate::executor::translate_driver_error(
            error,
            sql,
            self.inner.backend,
            &self.inner.config,
            "row",
        )
    }

    /// As [`Db::translate`], with the wait already measured — for the paths
    /// that acquire a connection as a side effect of something else, such as
    /// `BEGIN`, and would otherwise report the configured timeout rather than
    /// the time actually spent.
    pub(crate) fn map_acquire_error(
        &self,
        error: sqlx::Error,
        sql: &str,
        waited: Duration,
    ) -> Error {
        if matches!(error, sqlx::Error::PoolTimedOut) {
            tracing::warn!(
                waited_ms = waited.as_millis(),
                size = self.inner.effective_max,
                "db: the connection pool is exhausted"
            );
            if let Some(metrics) = &self.inner.metrics {
                metrics.acquire(&AcquireSample {
                    waited,
                    timed_out: true,
                });
            }
            return Error::PoolTimeout {
                waited,
                size: self.inner.effective_max,
            };
        }
        self.translate(error, sql)
    }

    /// The shared implementation of the two advisory-lock entry points.
    async fn take_advisory_lock(
        &self,
        key: AdvisoryKey,
        blocking: bool,
    ) -> Result<Option<AdvisoryLock>> {
        if self.inner.backend != Backend::Postgres {
            return Err(Error::Unsupported {
                feature: "advisory locks",
                backend: self.inner.backend,
            });
        }
        #[cfg_attr(
            not(feature = "postgres"),
            expect(unused_mut, reason = "the mutable borrow is in the PostgreSQL branch")
        )]
        let mut connection = self.acquire().await?;
        #[cfg(feature = "postgres")]
        {
            let conn = match &mut connection {
                PooledConnection::Postgres(conn) => conn,
                #[cfg(feature = "sqlite")]
                PooledConnection::Sqlite(_) => {
                    return Err(Error::Unsupported {
                        feature: "advisory locks",
                        backend: self.inner.backend,
                    });
                }
            };
            if blocking {
                // `pg_advisory_lock` returns void and waits; there is nothing
                // to inspect, only something to wait for.
                let statement = "select pg_advisory_lock($1)";
                sqlx::query(statement)
                    .bind(key.as_i64())
                    .execute(&mut **conn)
                    .await
                    .map_err(|error| self.translate(error, statement))?;
            } else {
                let statement = "select pg_try_advisory_lock($1)";
                let taken = sqlx::query_scalar::<_, bool>(statement)
                    .bind(key.as_i64())
                    .fetch_one(&mut **conn)
                    .await
                    .map_err(|error| self.translate(error, statement))?;
                if !taken {
                    return Ok(None);
                }
            }
        }
        #[cfg(not(feature = "postgres"))]
        let _ = blocking;
        Ok(Some(AdvisoryLock {
            key,
            connection: Some(connection),
        }))
    }
}

/// The fields [`Db::derive`] may change.
struct DerivedDb {
    pool: Option<PoolHandle>,
    counter: Option<Arc<StatementCounter>>,
    tenant: Option<TenantId>,
    prefer_primary: bool,
    sticky: Arc<Sticky>,
    metrics: Option<Arc<dyn DbMetrics>>,
    config: Option<DatabaseConfig>,
    poisoned: Option<Arc<str>>,
}

impl PoolHandle {
    /// Another handle on the same pool. sqlx pools are `Arc` inside, so this is
    /// a refcount bump rather than a second set of connections.
    fn shallow_clone(&self) -> Self {
        match self {
            #[cfg(feature = "postgres")]
            Self::Postgres(pool) => Self::Postgres(pool.clone()),
            #[cfg(feature = "sqlite")]
            Self::Sqlite(pool) => Self::Sqlite(pool.clone()),
            #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
            Self::Unbacked(never) => match *never {},
        }
    }
}

impl fmt::Debug for Db {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Db")
            .field("backend", &self.inner.backend)
            .field("replicas", &self.inner.replicas.len())
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------
// Read-your-writes
// ---------------------------------------------------------------------------

/// When the last write happened, so that reads can follow it.
struct Sticky {
    origin: Instant,
    last_write_ms: AtomicU64,
    ever: AtomicBool,
}

impl Sticky {
    fn new() -> Self {
        Self {
            origin: Instant::now(),
            last_write_ms: AtomicU64::new(0),
            ever: AtomicBool::new(false),
        }
    }

    fn mark(&self) {
        let now = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_write_ms.store(now, Ordering::Relaxed);
        self.ever.store(true, Ordering::Relaxed);
    }

    fn within(&self, window: Duration) -> bool {
        if window.is_zero() || !self.ever.load(Ordering::Relaxed) {
            return false;
        }
        let now = u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX);
        let last = self.last_write_ms.load(Ordering::Relaxed);
        let window_ms = u64::try_from(window.as_millis()).unwrap_or(u64::MAX);
        now.saturating_sub(last) < window_ms
    }
}

// ---------------------------------------------------------------------------
// Replica rotation
// ---------------------------------------------------------------------------

/// The weighted round-robin over healthy replicas.
struct Rotation {
    /// Replica indices repeated by weight, so the round-robin is a modulo.
    slots: Vec<usize>,
    health: Vec<ReplicaHealth>,
    cursor: AtomicUsize,
    closed: AtomicBool,
}

/// One replica's measured state.
struct ReplicaHealth {
    max_lag: Duration,
    lag_ms: AtomicU64,
    healthy: AtomicBool,
    announced: AtomicBool,
}

impl Rotation {
    fn new(replicas: &[ReplicaConfig]) -> Self {
        let mut slots = Vec::new();
        let mut health = Vec::with_capacity(replicas.len());
        for (index, replica) in replicas.iter().enumerate() {
            for _ in 0..replica.weight.max(1) {
                slots.push(index);
            }
            health.push(ReplicaHealth {
                max_lag: replica.max_lag,
                lag_ms: AtomicU64::new(0),
                // Healthy until measured otherwise: a replica that is refused
                // at the first sample is out of rotation five seconds later,
                // and assuming the worst at boot would send every read to the
                // primary until the sampler had run.
                healthy: AtomicBool::new(true),
                announced: AtomicBool::new(false),
            });
        }
        Self {
            slots,
            health,
            cursor: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        }
    }

    /// The next healthy replica, or `None` when every one is out.
    fn pick(&self) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        for offset in 0..self.slots.len() {
            let index = self.slots[(start.wrapping_add(offset)) % self.slots.len()];
            if self.health[index].healthy.load(Ordering::Relaxed) {
                return Some(index);
            }
        }
        None
    }

    /// How many replicas are out of rotation.
    fn lagging(&self) -> usize {
        self.health
            .iter()
            .filter(|replica| !replica.healthy.load(Ordering::Relaxed))
            .count()
    }

    /// Records a measurement, logging the first time a replica changes state
    /// and not again until it changes back.
    fn record(&self, index: usize, lag: Option<Duration>) {
        let Some(replica) = self.health.get(index) else {
            return;
        };
        let healthy = match lag {
            // A replica that cannot be reached is not a replica.
            None => false,
            Some(lag) => lag <= replica.max_lag,
        };
        let lag_ms = lag.map_or(u64::MAX, |lag| {
            u64::try_from(lag.as_millis()).unwrap_or(u64::MAX)
        });
        replica.lag_ms.store(lag_ms, Ordering::Relaxed);

        let was = replica.healthy.swap(healthy, Ordering::Relaxed);
        if was == healthy {
            return;
        }
        if healthy {
            replica.announced.store(false, Ordering::Relaxed);
            tracing::info!(replica = index, lag_ms, "db: replica is back in rotation");
        } else if !replica.announced.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                replica = index,
                lag_ms,
                max_lag_ms = replica.max_lag.as_millis(),
                "db: replica is out of rotation until its lag comes back under `max_lag`"
            );
        }
    }
}

/// Measures every replica's lag every [`LAG_SAMPLE_INTERVAL`], until the
/// primary handle is dropped or closed.
fn spawn_lag_sampler(rotation: Weak<Rotation>, replicas: Vec<Db>) {
    if tokio::runtime::Handle::try_current().is_err() {
        // No runtime to spawn onto. Every replica stays in rotation, which is
        // the same behaviour as a `max_lag` nobody configured.
        return;
    }
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(LAG_SAMPLE_INTERVAL).await;
            let Some(rotation) = rotation.upgrade() else {
                return;
            };
            if rotation.closed.load(Ordering::Relaxed) {
                return;
            }
            for (index, replica) in replicas.iter().enumerate() {
                rotation.record(index, measure_lag(replica).await);
            }
        }
    });
}

/// How far behind the primary a replica is, or `None` when it cannot say.
///
/// `pg_last_xact_replay_timestamp()` is `NULL` on a primary and on a replica
/// that has not replayed anything yet. Both mean "no measurable lag", not
/// "infinitely far behind" — reading them as unhealthy would take a perfectly
/// good replica out of rotation for being idle.
#[cfg(feature = "postgres")]
async fn measure_lag(replica: &Db) -> Option<Duration> {
    let pool = replica.postgres_pool()?;
    let statement = "select extract(epoch from (now() - pg_last_xact_replay_timestamp()))";
    match sqlx::query_scalar::<_, Option<f64>>(statement)
        .fetch_one(pool)
        .await
    {
        Ok(None) => Some(Duration::ZERO),
        Ok(Some(seconds)) if seconds.is_finite() && seconds > 0.0 => {
            Some(Duration::from_secs_f64(seconds))
        }
        Ok(Some(_)) => Some(Duration::ZERO),
        Err(error) => {
            tracing::debug!("db: could not measure replica lag: {error}");
            None
        }
    }
}

/// Replication lag is a PostgreSQL question, and this build has no PostgreSQL.
#[cfg(not(feature = "postgres"))]
#[expect(
    clippy::unused_async,
    reason = "the signature matches the PostgreSQL build"
)]
async fn measure_lag(_replica: &Db) -> Option<Duration> {
    Some(Duration::ZERO)
}

/// Closes a pool without making the caller wait, for an eviction on a path that
/// cannot be `async`.
fn close_in_background(db: Db) {
    if tokio::runtime::Handle::try_current().is_ok() {
        tokio::spawn(async move { db.close().await });
    }
    // Without a runtime the pool is dropped here, which closes its sockets.
}

// ---------------------------------------------------------------------------
// Opening pools
// ---------------------------------------------------------------------------

/// Whether a SQLite URL names a database that lives only in memory.
///
/// It matters because every connection to `:memory:` is a **different**
/// database unless the pool is capped at one connection, and because recycling
/// that connection would throw the schema away mid-test.
fn is_in_memory(url: &str) -> bool {
    url.contains(":memory:") || url.contains("mode=memory")
}

/// Builds the pool, connecting eagerly when `min_connections` asks for it.
async fn open_pool(
    config: &DatabaseConfig,
    backend: Backend,
    #[cfg_attr(
        not(feature = "postgres"),
        expect(
            unused_variables,
            reason = "only PostgreSQL caches prepared statements"
        )
    )]
    persistent: bool,
) -> Result<PoolHandle> {
    match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let options = postgres_options(config, persistent)?;
            let pool = pool_options::<sqlx::Postgres>(config, backend);
            let pool = if config.min_connections > 0 {
                pool.connect_with(options)
                    .await
                    .map_err(|error| connection_error(error, config))?
            } else {
                pool.connect_lazy_with(options)
            };
            Ok(PoolHandle::Postgres(pool))
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => {
            let options = sqlite_options(config)?;
            let pool = pool_options::<sqlx::Sqlite>(config, backend);
            let pool = if config.min_connections > 0 || is_in_memory(config.url.expose()) {
                pool.connect_with(options)
                    .await
                    .map_err(|error| connection_error(error, config))?
            } else {
                pool.connect_lazy_with(options)
            };
            Ok(PoolHandle::Sqlite(pool))
        }
        #[allow(unreachable_patterns, reason = "only when a backend feature is off")]
        other => Err(Error::Configuration {
            detail: format!("this build has no {other} driver"),
        }),
    }
}

/// Builds a pool without connecting, for the tenant handles that are created on
/// a synchronous path.
fn open_pool_lazily(
    config: &DatabaseConfig,
    backend: Backend,
    #[cfg_attr(
        not(feature = "postgres"),
        expect(
            unused_variables,
            reason = "only PostgreSQL caches prepared statements"
        )
    )]
    persistent: bool,
    schema: Option<String>,
) -> Result<PoolHandle> {
    match backend {
        #[cfg(feature = "postgres")]
        Backend::Postgres => {
            let options = postgres_options(config, persistent)?;
            let mut pool = pool_options::<sqlx::Postgres>(config, backend);
            if let Some(schema) = schema {
                // The schema is a validated `Ident`, so this is the one place a
                // tenant-derived name reaches SQL — quoted, and only ever
                // `[A-Za-z0-9_]`.
                let statement = format!("set search_path to \"{schema}\", public");
                pool = pool.after_connect(move |conn, _| {
                    let statement = statement.clone();
                    Box::pin(async move {
                        use sqlx::Executor as _;
                        conn.execute(sqlx::raw_sql(sqlx::AssertSqlSafe(statement)))
                            .await?;
                        Ok(())
                    })
                });
            }
            Ok(PoolHandle::Postgres(pool.connect_lazy_with(options)))
        }
        #[cfg(feature = "sqlite")]
        Backend::Sqlite => {
            if schema.is_some() {
                return Err(Error::Unsupported {
                    feature: "schema-per-tenant",
                    backend: Backend::Sqlite,
                });
            }
            let options = sqlite_options(config)?;
            let pool = pool_options::<sqlx::Sqlite>(config, backend);
            Ok(PoolHandle::Sqlite(pool.connect_lazy_with(options)))
        }
        #[allow(unreachable_patterns, reason = "only when a backend feature is off")]
        other => Err(Error::Configuration {
            detail: format!("this build has no {other} driver"),
        }),
    }
}

/// The pool size the backend will actually allow.
///
/// Every connection to an in-memory SQLite database is a **different**
/// database, so a pool of more than one would hand out an empty one at random.
/// [`PoolStats::max`] and [`Error::PoolTimeout`] report this rather than the
/// configured number, because a saturation metric measured against a limit that
/// is not in force is worse than no metric.
fn effective_max_connections(config: &DatabaseConfig, backend: Backend) -> u32 {
    if backend == Backend::Sqlite && is_in_memory(config.url.expose()) {
        return 1;
    }
    config.max_connections
}

/// The sizes and timeouts, which are the same for both drivers.
fn pool_options<DB: sqlx::Database>(
    config: &DatabaseConfig,
    backend: Backend,
) -> sqlx::pool::PoolOptions<DB> {
    let in_memory = backend == Backend::Sqlite && is_in_memory(config.url.expose());
    let max = effective_max_connections(config, backend);

    let options = sqlx::pool::PoolOptions::<DB>::new()
        .max_connections(max)
        .min_connections(config.min_connections.min(max))
        .acquire_timeout(config.acquire_timeout)
        .test_before_acquire(config.test_before_acquire);

    if in_memory {
        return options.idle_timeout(None).max_lifetime(None);
    }
    options
        .idle_timeout(nonzero(config.idle_timeout))
        .max_lifetime(nonzero(config.max_lifetime))
}

/// `None` for a zero duration, which is how sqlx spells "never".
fn nonzero(duration: Duration) -> Option<Duration> {
    (!duration.is_zero()).then_some(duration)
}

/// The PostgreSQL connect options: the URL, plus the settings that make a
/// production incident findable.
#[cfg(feature = "postgres")]
fn postgres_options(
    config: &DatabaseConfig,
    persistent: bool,
) -> Result<sqlx::postgres::PgConnectOptions> {
    use core::str::FromStr as _;

    let url = config.url.expose();
    let mut options =
        sqlx::postgres::PgConnectOptions::from_str(url).map_err(|error| Error::Configuration {
            detail: format!(
                "`{}` is not a PostgreSQL URL: {error}\n  \
                 help: `postgres://user:password@host:5432/database`",
                redact_password(url)
            ),
        })?;

    options = options.application_name(&config.application_name);

    if !config::url_sets_sslmode(url) {
        options = options.ssl_mode(tls_mode(config.tls)?);
    }

    // Session settings, applied through the startup packet so that they survive
    // a connection the pool recycles and cost no round trip. A transaction-mode
    // pooler that refuses unknown startup parameters is the reason these are
    // also re-applied per transaction — see `TxOptions`.
    options = options.options([
        (
            "statement_timeout",
            format!("{}", config.statement_timeout.as_millis()),
        ),
        (
            "lock_timeout",
            format!("{}", config.lock_timeout.as_millis()),
        ),
    ]);

    if !persistent {
        options = options.statement_cache_capacity(0);
    }
    Ok(options)
}

/// The `sslmode` a [`TlsMode`] asks for.
#[cfg(all(feature = "postgres", feature = "tls"))]
fn tls_mode(mode: TlsMode) -> Result<sqlx::postgres::PgSslMode> {
    Ok(match mode {
        TlsMode::Disable => sqlx::postgres::PgSslMode::Disable,
        TlsMode::Prefer => sqlx::postgres::PgSslMode::Prefer,
        TlsMode::Require => sqlx::postgres::PgSslMode::Require,
        TlsMode::VerifyFull => sqlx::postgres::PgSslMode::VerifyFull,
    })
}

/// Without the `tls` feature there is nothing to negotiate, so asking for TLS
/// is a configuration error rather than a connection that quietly goes over the
/// wire in the clear.
#[cfg(all(feature = "postgres", not(feature = "tls")))]
fn tls_mode(mode: TlsMode) -> Result<sqlx::postgres::PgSslMode> {
    if mode.requires_tls() {
        return Err(Error::Configuration {
            detail: format!(
                "`database.tls` is `{mode}`, and this build of `moso-orm` has the `tls` feature \
                 turned off\n  \
                 help: turn `moso-orm/tls` back on, or set `database.tls = \"disable\"` — every \
                 managed PostgreSQL refuses a plaintext connection"
            ),
        });
    }
    Ok(sqlx::postgres::PgSslMode::Disable)
}

/// The SQLite connect options.
///
/// Foreign keys are **on**, which SQLite does not do by default and which every
/// application assumes; the busy timeout follows `lock_timeout`, so that
/// "database is locked" becomes a wait rather than an error.
#[cfg(feature = "sqlite")]
fn sqlite_options(config: &DatabaseConfig) -> Result<sqlx::sqlite::SqliteConnectOptions> {
    use core::str::FromStr as _;

    let url = config.url.expose();
    let options = sqlx::sqlite::SqliteConnectOptions::from_str(url).map_err(|error| {
        Error::Configuration {
            detail: format!(
                "`{url}` is not a SQLite URL: {error}\n  \
                 help: `sqlite://app.db`, or `sqlite://:memory:` for a test"
            ),
        }
    })?;
    Ok(options
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(config.lock_timeout))
}

/// Whether a transaction-mode pooler sits in front of PostgreSQL.
///
/// Returns `true` only when it is **certain**: a direct connection cannot
/// change its backend process id, so a change during the probe proves the
/// server connection was reassigned. A `pgbouncer=true` marker in the URL is
/// taken at its word.
async fn detect_pooler(config: &DatabaseConfig, backend: Backend) -> Result<bool> {
    if config.pgbouncer.disables_the_statement_cache() {
        return Ok(true);
    }
    if config::url_declares_pgbouncer(config.url.expose()) {
        tracing::debug!("db: the URL declares `pgbouncer=true`");
        return Ok(true);
    }
    if backend != Backend::Postgres || !config.pgbouncer.probes() {
        return Ok(false);
    }
    probe_for_pooler(config).await
}

/// The backend-process-id probe. See [`PgBouncerMode::Detect`].
#[cfg(feature = "postgres")]
async fn probe_for_pooler(config: &DatabaseConfig) -> Result<bool> {
    use sqlx::Connection as _;

    let options = postgres_options(config, false)?;
    let mut connection = match sqlx::PgConnection::connect_with(&options).await {
        Ok(connection) => connection,
        Err(error) => {
            // A server that is not up at boot is a real deployment — a lazy
            // pool, a database container that starts a moment later. It is only
            // fatal when the configuration asked for eager connections.
            if config.min_connections > 0 {
                return Err(connection_error(error, config));
            }
            tracing::debug!(
                "db: skipping the connection-pooler probe, the server is not up yet: {error}"
            );
            return Ok(false);
        }
    };

    let mut first: Option<i32> = None;
    let mut reassigned = false;
    for _ in 0..4 {
        // Each round is its own transaction, which is the unit a
        // transaction-mode pooler reassigns.
        let pid: Option<i32> = match connection.begin().await {
            Ok(mut transaction) => {
                let pid = sqlx::query_scalar::<_, i32>("select pg_backend_pid()")
                    .fetch_one(&mut *transaction)
                    .await
                    .ok();
                let _ = transaction.rollback().await;
                pid
            }
            Err(_) => None,
        };
        let Some(pid) = pid else { break };
        match first {
            None => first = Some(pid),
            Some(seen) if seen != pid => {
                reassigned = true;
                break;
            }
            Some(_) => {}
        }
    }
    let _ = connection.close().await;

    if reassigned {
        tracing::info!(
            "db: a transaction-mode connection pooler is in front of this database (the backend \
             process id changed between transactions)"
        );
    }
    Ok(reassigned)
}

/// Without the PostgreSQL driver there is no pooler to find.
#[cfg(not(feature = "postgres"))]
#[expect(
    clippy::unused_async,
    reason = "the signature matches the PostgreSQL build"
)]
async fn probe_for_pooler(_config: &DatabaseConfig) -> Result<bool> {
    Ok(false)
}

/// A driver failure at connect time, with the URL redacted.
fn connection_error(error: sqlx::Error, config: &DatabaseConfig) -> Error {
    Error::Connection {
        detail: format!("{error} (url: {})", config.redacted_url()),
    }
}

// ---------------------------------------------------------------------------
// Advisory locks
// ---------------------------------------------------------------------------

/// The key a PostgreSQL advisory lock is taken on.
///
/// PostgreSQL advisory locks are namespaced by a 64-bit integer and nothing
/// else: two unrelated parts of a system that both pick `1` deadlock on each
/// other. [`AdvisoryKey::hashed`] exists so that the number comes from a name.
///
/// ```
/// use moso_orm::db::AdvisoryKey;
///
/// assert_eq!(AdvisoryKey::of(7).as_i64(), 7);
/// assert_eq!(AdvisoryKey::hashed("nightly"), AdvisoryKey::hashed("nightly"));
/// assert_ne!(AdvisoryKey::hashed("nightly"), AdvisoryKey::hashed("weekly"));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct AdvisoryKey(i64);

impl AdvisoryKey {
    /// The lock identified by `key`.
    ///
    /// ```
    /// use moso_orm::db::AdvisoryKey;
    ///
    /// assert_eq!(AdvisoryKey::of(-1).as_i64(), -1);
    /// ```
    #[must_use]
    pub const fn of(key: i64) -> Self {
        Self(key)
    }

    /// The lock identified by two 32-bit halves, which is the other spelling
    /// PostgreSQL accepts and the one an application that namespaces its locks
    /// by module usually wants.
    ///
    /// ```
    /// use moso_orm::db::AdvisoryKey;
    ///
    /// assert_eq!(AdvisoryKey::pair(1, 2), AdvisoryKey::pair(1, 2));
    /// assert_ne!(AdvisoryKey::pair(1, 2), AdvisoryKey::pair(2, 1));
    /// ```
    #[must_use]
    pub const fn pair(namespace: i32, key: i32) -> Self {
        Self(((namespace as i64) << 32) | (key as i64 & 0xFFFF_FFFF))
    }

    /// The lock identified by a name.
    ///
    /// A stable 64-bit FNV-1a of the name: the same name gives the same key in
    /// every process and every release, which a `DefaultHasher` would not.
    ///
    /// ```
    /// use moso_orm::db::AdvisoryKey;
    ///
    /// assert_eq!(AdvisoryKey::hashed("import"), AdvisoryKey::hashed("import"));
    /// ```
    #[must_use]
    pub const fn hashed(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut index = 0;
        while index < bytes.len() {
            hash ^= bytes[index] as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            index += 1;
        }
        Self(hash as i64)
    }

    /// The key as PostgreSQL sees it.
    ///
    /// ```
    /// use moso_orm::db::AdvisoryKey;
    ///
    /// assert_eq!(AdvisoryKey::of(9).as_i64(), 9);
    /// ```
    #[must_use]
    pub const fn as_i64(self) -> i64 {
        self.0
    }
}

impl fmt::Display for AdvisoryKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A held session-level advisory lock.
///
/// Holds a connection out of the pool for as long as it exists, because that is
/// what "session level" means: the lock belongs to the connection, and a
/// connection that went back to the pool would carry the lock to whoever got it
/// next.
///
/// Dropping without [`AdvisoryLock::unlock`] closes the connection instead of
/// returning it, which releases the lock. That is correct and costs a
/// reconnect, so `unlock` is worth writing.
///
/// ```no_run
/// # use moso_orm::{Db, Result};
/// # use moso_orm::db::AdvisoryKey;
/// # async fn example(db: &Db) -> Result<()> {
/// let lock = db.advisory_lock(AdvisoryKey::hashed("rollup")).await?;
/// assert!(lock.is_held());
/// lock.unlock().await
/// # }
/// ```
pub struct AdvisoryLock {
    key: AdvisoryKey,
    connection: Option<PooledConnection>,
}

impl AdvisoryLock {
    /// The key this lock was taken on.
    ///
    /// ```no_run
    /// # use moso_orm::db::{AdvisoryKey, AdvisoryLock};
    /// fn key(lock: &AdvisoryLock) -> AdvisoryKey {
    ///     lock.key()
    /// }
    /// ```
    #[must_use]
    pub const fn key(&self) -> AdvisoryKey {
        self.key
    }

    /// Whether the lock is still held.
    ///
    /// ```no_run
    /// # use moso_orm::db::AdvisoryLock;
    /// fn held(lock: &AdvisoryLock) -> bool {
    ///     lock.is_held()
    /// }
    /// ```
    #[must_use]
    pub const fn is_held(&self) -> bool {
        self.connection.is_some()
    }

    /// Releases the lock and returns the connection to the pool.
    ///
    /// # Errors
    ///
    /// Whatever `pg_advisory_unlock` fails with. The lock is released either
    /// way: an error here means the connection is closed rather than reused.
    ///
    /// ```no_run
    /// # use moso_orm::Result;
    /// # use moso_orm::db::AdvisoryLock;
    /// # async fn release(lock: AdvisoryLock) -> Result<()> {
    /// lock.unlock().await
    /// # }
    /// ```
    #[allow(
        clippy::unused_async,
        reason = "async on every build; the body is behind the `postgres` feature"
    )]
    pub async fn unlock(mut self) -> Result<()> {
        #[cfg_attr(
            not(feature = "postgres"),
            expect(unused_mut, reason = "the mutable borrow is in the PostgreSQL branch")
        )]
        let Some(mut connection) = self.connection.take() else {
            return Ok(());
        };
        #[cfg(feature = "postgres")]
        #[allow(
            irrefutable_let_patterns,
            reason = "one variant on a single-backend build, two on the default one"
        )]
        if let PooledConnection::Postgres(conn) = &mut connection {
            let statement = "select pg_advisory_unlock($1)";
            let released = sqlx::query_scalar::<_, bool>(statement)
                .bind(self.key.as_i64())
                .fetch_one(&mut **conn)
                .await;
            return match released {
                Ok(_) => Ok(()),
                Err(error) => {
                    // The connection is not returned to the pool: it may still
                    // hold the lock, and handing it to the next caller would
                    // move the problem rather than solve it.
                    conn.close_on_drop();
                    Err(Error::Connection {
                        detail: format!("releasing an advisory lock failed: {error}"),
                    })
                }
            };
        }
        drop(connection);
        Ok(())
    }
}

impl Drop for AdvisoryLock {
    fn drop(&mut self) {
        // Closing the connection is what releases a session-level lock without
        // an `await`. Returning it to the pool would leave the lock held by a
        // connection somebody else is about to be handed.
        if let Some(connection) = &mut self.connection {
            match connection {
                #[cfg(feature = "postgres")]
                PooledConnection::Postgres(conn) => conn.close_on_drop(),
                #[cfg(feature = "sqlite")]
                PooledConnection::Sqlite(conn) => conn.close_on_drop(),
                #[cfg(not(any(feature = "postgres", feature = "sqlite")))]
                PooledConnection::Unbacked(never) => match *never {},
            }
        }
    }
}

impl fmt::Debug for AdvisoryLock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AdvisoryLock")
            .field("key", &self.key)
            .field("held", &self.is_held())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Health
// ---------------------------------------------------------------------------

/// The readiness check for a [`Db`], as `App::health_check` registers it.
///
/// Critical by default: an instance that cannot reach its database should not
/// be in the load balancer.
///
/// ```no_run
/// # use moso_orm::Db;
/// # use moso_orm::db::DatabaseCheck;
/// fn register(db: &Db) -> DatabaseCheck {
///     DatabaseCheck::new(db.clone())
/// }
/// ```
pub struct DatabaseCheck {
    db: Db,
    critical: bool,
}

impl DatabaseCheck {
    /// A critical check over `db`.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// # use moso_orm::db::DatabaseCheck;
    /// # fn example(db: &Db) {
    /// let check = DatabaseCheck::new(db.clone());
    /// assert!(check.is_critical());
    /// # }
    /// ```
    #[must_use]
    pub const fn new(db: Db) -> Self {
        Self { db, critical: true }
    }

    /// A check whose failure degrades the report without pulling the instance
    /// out of rotation.
    ///
    /// For a read replica, or a reporting database the request path does not
    /// need.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// # use moso_orm::db::DatabaseCheck;
    /// # fn example(db: &Db) {
    /// assert!(!DatabaseCheck::new(db.clone()).non_critical().is_critical());
    /// # }
    /// ```
    #[must_use]
    pub const fn non_critical(mut self) -> Self {
        self.critical = false;
        self
    }

    /// Whether failing takes the instance out of rotation.
    ///
    /// ```no_run
    /// # use moso_orm::Db;
    /// # use moso_orm::db::DatabaseCheck;
    /// # fn example(db: &Db) -> bool {
    /// DatabaseCheck::new(db.clone()).is_critical()
    /// # }
    /// ```
    #[must_use]
    pub const fn is_critical(&self) -> bool {
        self.critical
    }
}

impl moso_core::health::HealthCheck for DatabaseCheck {
    fn check<'a>(
        &'a self,
        _resolver: &'a moso_core::app::Resolver,
    ) -> moso_core::BoxFuture<'a, HealthStatus> {
        Box::pin(async move { self.db.health().await })
    }

    fn critical(&self) -> bool {
        self.critical
    }
}

impl fmt::Debug for DatabaseCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DatabaseCheck")
            .field("backend", &self.db.backend())
            .field("critical", &self.critical)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// The query-duration histogram's name.
pub const QUERY_DURATION_METRIC: &str = "moso_db_query_duration_seconds";
/// The pool-occupancy gauge's name.
pub const POOL_CONNECTIONS_METRIC: &str = "moso_db_pool_connections";
/// The acquire-wait histogram's name.
pub const POOL_ACQUIRE_METRIC: &str = "moso_db_pool_acquire_seconds";
/// The transaction-retry counter's name.
pub const TRANSACTION_RETRIES_METRIC: &str = "moso_db_transaction_retries_total";

/// One completed statement, as a recorder sees it.
///
/// `#[non_exhaustive]`: a recorder reads the fields it cares about, and a
/// future measurement joining them is not a breaking change. Only `moso-orm`
/// constructs one.
///
/// ```
/// use moso_orm::db::{DbMetrics, QuerySample, QUERY_DURATION_METRIC};
///
/// /// Labels the histogram the way `docs/02-data/24` says to.
/// struct Histogram;
///
/// impl DbMetrics for Histogram {
///     fn query(&self, sample: &QuerySample<'_>) {
///         let entity = sample.entity.unwrap_or("-");
///         println!(
///             "{QUERY_DURATION_METRIC}{{operation=\"{}\",entity=\"{entity}\"}} {}",
///             sample.operation,
///             sample.elapsed.as_secs_f64(),
///         );
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct QuerySample<'a> {
    /// `select`, `insert`, `update`, `delete`, `ddl` or `raw`.
    pub operation: &'a str,
    /// The entity, when the statement was built from one.
    pub entity: Option<&'a str>,
    /// How long the round trip took.
    pub elapsed: Duration,
    /// Rows returned, or rows affected.
    pub rows: u64,
    /// Whether the statement ran inside a transaction.
    pub in_transaction: bool,
    /// Whether it ended in an error.
    pub failed: bool,
}

/// One attempt to check a connection out of the pool.
///
/// ```
/// use moso_orm::db::{AcquireSample, DbMetrics, POOL_ACQUIRE_METRIC};
///
/// /// Shouts about the waits that end in a 503.
/// struct Waits;
///
/// impl DbMetrics for Waits {
///     fn acquire(&self, sample: &AcquireSample) {
///         if sample.timed_out {
///             eprintln!("{POOL_ACQUIRE_METRIC}: gave up after {:?}", sample.waited);
///         }
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct AcquireSample {
    /// How long the caller waited.
    pub waited: Duration,
    /// Whether the wait ended in [`Error::PoolTimeout`].
    pub timed_out: bool,
}

/// One transaction retry.
///
/// ```
/// use moso_orm::db::{DbMetrics, RetrySample, TRANSACTION_RETRIES_METRIC};
///
/// /// Counts retries by reason, which is the series worth alerting on.
/// struct Retries;
///
/// impl DbMetrics for Retries {
///     fn retry(&self, sample: &RetrySample<'_>) {
///         println!("{TRANSACTION_RETRIES_METRIC}{{reason=\"{}\"}} 1", sample.reason);
///     }
/// }
/// ```
#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct RetrySample<'a> {
    /// `serialization` or `deadlock`.
    pub reason: &'a str,
    /// Which attempt is about to run: 1 is the first retry.
    pub attempt: u32,
}

/// Where the data layer's measurements go.
///
/// Every method has a default that does nothing, so a recorder that only wants
/// query durations implements one. Called on the caller's task, so it must not
/// block: an exporter that needs I/O should push onto a channel here.
///
/// ```
/// use moso_orm::db::{DbMetrics, QuerySample};
///
/// /// Prints every statement that took longer than a tenth of a second.
/// pub struct Slow;
///
/// impl DbMetrics for Slow {
///     fn query(&self, sample: &QuerySample<'_>) {
///         if sample.elapsed.as_millis() > 100 {
///             eprintln!("{} took {:?}", sample.operation, sample.elapsed);
///         }
///     }
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot record database metrics",
    label = "not a metrics recorder",
    note = "implement any of `query`, `acquire`, `retry` and `pool` — each has a do-nothing \
            default, so a recorder that only counts statements implements one method",
    note = "help: `db.with_metrics(std::sync::Arc::new({Self}::new()))`"
)]
pub trait DbMetrics: Send + Sync + 'static {
    /// One statement finished. See [`QUERY_DURATION_METRIC`].
    fn query(&self, sample: &QuerySample<'_>) {
        let _ = sample;
    }

    /// One connection was checked out, or the wait timed out. See
    /// [`POOL_ACQUIRE_METRIC`].
    fn acquire(&self, sample: &AcquireSample) {
        let _ = sample;
    }

    /// A transaction is about to be retried. See
    /// [`TRANSACTION_RETRIES_METRIC`].
    fn retry(&self, sample: &RetrySample<'_>) {
        let _ = sample;
    }

    /// The pool's occupancy, sampled. See [`POOL_CONNECTIONS_METRIC`].
    fn pool(&self, stats: &PoolStats) {
        let _ = stats;
    }
}

// ---------------------------------------------------------------------------
// PoolStats
// ---------------------------------------------------------------------------

/// A snapshot of the pool's occupancy.
///
/// ```
/// use moso_orm::PoolStats;
///
/// let stats = PoolStats::new(8, 6, 2, 0, 8);
/// assert!(!stats.is_saturated());
/// assert_eq!(stats.utilisation(), 0.25);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PoolStats {
    /// Connections currently open.
    pub size: u32,
    /// Open connections nobody is using.
    pub idle: u32,
    /// Connections currently running a statement.
    pub in_flight: u32,
    /// Tasks waiting to acquire one.
    pub waiting: u32,
    /// The configured maximum.
    pub max: u32,
}

impl PoolStats {
    /// A snapshot with every counter given.
    ///
    /// The struct is `#[non_exhaustive]` so that a future pool metric is not a
    /// breaking change; this is how a test or a health endpoint builds one.
    ///
    /// ```
    /// use moso_orm::PoolStats;
    ///
    /// assert_eq!(PoolStats::new(8, 6, 2, 0, 8).idle, 6);
    /// ```
    #[must_use]
    pub const fn new(size: u32, idle: u32, in_flight: u32, waiting: u32, max: u32) -> Self {
        Self {
            size,
            idle,
            in_flight,
            waiting,
            max,
        }
    }

    /// Whether every connection is busy and something is waiting.
    ///
    /// This is the shape of the outage that ends in a 503: the metric to alert
    /// on is `waiting`, not `size`.
    ///
    /// ```
    /// use moso_orm::PoolStats;
    ///
    /// let busy = PoolStats::new(8, 0, 8, 3, 8);
    /// assert!(busy.is_saturated());
    /// ```
    #[must_use]
    pub const fn is_saturated(&self) -> bool {
        self.idle == 0 && self.waiting > 0
    }

    /// The fraction of the configured maximum that is running a statement.
    ///
    /// ```
    /// use moso_orm::PoolStats;
    ///
    /// let half = PoolStats::new(4, 2, 2, 0, 4);
    /// assert_eq!(half.utilisation(), 0.5);
    /// ```
    #[must_use]
    pub fn utilisation(&self) -> f64 {
        if self.max == 0 {
            return 0.0;
        }
        f64::from(self.in_flight) / f64::from(self.max)
    }
}

/// What the crate's real-database tests share.
///
/// Every test that needs a server gates on `DATABASE_URL` and skips with a
/// message when it is unset, so the suite still passes on a machine without
/// Docker. Everything that SQLite can answer is tested on SQLite as well, and
/// that half needs nothing installed at all.
#[cfg(test)]
pub(crate) mod test_support {
    use super::{DatabaseConfig, Db};

    /// The PostgreSQL test server, or a printed reason there is not one.
    pub(crate) fn postgres_url() -> Option<String> {
        match std::env::var("DATABASE_URL") {
            Ok(url) if !url.is_empty() => Some(url),
            _ => {
                eprintln!(
                    "skipping: DATABASE_URL is not set. Start the test server with \
                     `scripts/test-db.sh` and re-run to exercise the PostgreSQL path."
                );
                None
            }
        }
    }

    /// A PostgreSQL handle, or `None` when there is no server to talk to.
    pub(crate) async fn postgres() -> Option<Db> {
        let url = postgres_url()?;
        let config = DatabaseConfig::from_url(url)
            .with_max_connections(6)
            .with_application_name("moso-orm tests");
        Some(
            Db::connect(&config)
                .await
                .expect("the test server accepts connections"),
        )
    }

    /// An in-memory SQLite handle. Pinned to one connection by `Db::connect`,
    /// which is what makes a temporary table survive between statements.
    pub(crate) async fn sqlite() -> Db {
        Db::connect(&DatabaseConfig::from_url("sqlite://:memory:"))
            .await
            .expect("an in-memory database always opens")
    }

    /// A table name no other test — or other run — uses.
    pub(crate) fn unique_table(prefix: &str) -> String {
        format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moso_sql::Value;

    #[test]
    fn a_url_scheme_picks_the_backend_and_the_dialect() {
        assert_eq!(
            Backend::from_url("postgres://h/db").expect("postgres"),
            Backend::Postgres
        );
        assert_eq!(
            Backend::from_url("postgresql://h/db").expect("postgresql"),
            Backend::Postgres
        );
        assert_eq!(
            Backend::from_url("sqlite://x.db").expect("sqlite"),
            Backend::Sqlite
        );
        assert_eq!(Backend::Postgres.dialect().name(), "PostgreSQL");
        assert_eq!(Backend::Sqlite.dialect().name(), "SQLite");
    }

    #[test]
    fn an_unsupported_scheme_says_which_ones_work() {
        let error = Backend::from_url("mysql://h/db").expect_err("mysql is out of scope");
        let text = error.to_string();
        assert!(text.contains("postgres://"), "{text}");
        assert!(text.contains("sqlite://"), "{text}");
        assert!(text.contains("help:"), "{text}");
    }

    #[test]
    fn the_default_pool_is_small_on_purpose() {
        let config = DatabaseConfig::from_url("postgres://h/db");
        assert!(
            (4..=20).contains(&config.max_connections),
            "{}",
            config.max_connections
        );
        assert_eq!(config.acquire_timeout, Duration::from_secs(10));
        assert_eq!(config.max_lifetime, Duration::from_secs(1800));
        assert!(config.boot_summary().contains("max_connections"));
    }

    #[test]
    fn the_url_is_a_secret() {
        let config = DatabaseConfig::from_url("postgres://user:hunter2@h/db");
        let rendered = format!("{:?}", config.url);
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[test]
    fn pool_saturation_is_about_waiting_not_size() {
        let busy = PoolStats::new(8, 0, 8, 0, 8);
        assert!(
            !busy.is_saturated(),
            "full is not saturated until someone waits"
        );
        let saturated = PoolStats { waiting: 1, ..busy };
        assert!(saturated.is_saturated());
        assert!((saturated.utilisation() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn a_tenant_id_accepts_any_column_type() {
        assert_eq!(TenantId::of(1_i64).value(), &Value::I64(1));
        assert_eq!(
            TenantId::of(String::from("acme")).value(),
            &Value::text("acme")
        );
    }

    #[test]
    fn tls_defaults_to_prefer_and_only_verify_full_checks_the_chain() {
        assert_eq!(TlsMode::default(), TlsMode::Prefer);
        assert!(!TlsMode::Prefer.requires_tls());
        assert!(TlsMode::Require.requires_tls());
        assert!(!TlsMode::Require.verifies_the_certificate());
        assert!(TlsMode::VerifyFull.verifies_the_certificate());
    }

    #[test]
    fn the_sticky_window_expires() {
        let sticky = Sticky::new();
        assert!(
            !sticky.within(Duration::from_secs(3)),
            "nothing has been written yet"
        );

        sticky.mark();
        assert!(sticky.within(Duration::from_secs(3)));
        assert!(
            !sticky.within(Duration::ZERO),
            "a zero window turns read-your-writes off"
        );

        std::thread::sleep(Duration::from_millis(12));
        assert!(
            !sticky.within(Duration::from_millis(5)),
            "the window is measured from the write"
        );
    }

    #[test]
    fn the_rotation_is_weighted_and_skips_the_unhealthy() {
        let replicas = [
            ReplicaConfig::from_url("postgres://a/db").with_weight(3),
            ReplicaConfig::from_url("postgres://b/db").with_weight(1),
        ];
        let rotation = Rotation::new(&replicas);

        let mut counts = [0_u32; 2];
        for _ in 0..400 {
            let index = rotation.pick().expect("both replicas are healthy");
            counts[index] += 1;
        }
        assert_eq!(counts[0], 300, "weight 3 of 4");
        assert_eq!(counts[1], 100, "weight 1 of 4");

        // A replica past `max_lag` leaves the rotation, and the other one takes
        // every read rather than the reads being dropped.
        rotation.record(0, Some(Duration::from_secs(30)));
        assert_eq!(rotation.lagging(), 1);
        for _ in 0..20 {
            assert_eq!(rotation.pick(), Some(1));
        }

        // And when every replica is out, `pick` says so rather than choosing a
        // stale one; `Db::read` falls back to the primary.
        rotation.record(1, None);
        assert_eq!(rotation.lagging(), 2);
        assert_eq!(rotation.pick(), None);

        // Recovery puts it back.
        rotation.record(0, Some(Duration::from_millis(10)));
        assert_eq!(rotation.pick(), Some(0));
    }

    #[test]
    fn a_rotation_with_no_replicas_never_picks_one() {
        let rotation = Rotation::new(&[]);
        assert_eq!(rotation.pick(), None);
        assert_eq!(rotation.lagging(), 0);
    }

    #[test]
    fn an_advisory_key_is_stable_across_processes() {
        // A literal rather than a recomputation: the point of the constant is
        // that it does not change between releases, so an application that
        // stores a key can still take the same lock.
        assert_eq!(
            AdvisoryKey::hashed("").as_i64(),
            0xcbf2_9ce4_8422_2325_u64 as i64
        );
        assert_eq!(AdvisoryKey::hashed("moso"), AdvisoryKey::hashed("moso"));
        assert_ne!(AdvisoryKey::hashed("a"), AdvisoryKey::hashed("b"));
        assert_eq!(AdvisoryKey::pair(0, 5).as_i64(), 5);
        assert_eq!(AdvisoryKey::pair(1, 0).as_i64(), 1_i64 << 32);
    }

    #[test]
    fn in_memory_sqlite_is_recognised_so_the_pool_can_be_pinned() {
        assert!(is_in_memory("sqlite://:memory:"));
        assert!(is_in_memory("sqlite://file:x?mode=memory&cache=shared"));
        assert!(!is_in_memory("sqlite://app.db"));
    }

    #[test]
    fn the_metric_names_are_the_documented_ones() {
        assert_eq!(QUERY_DURATION_METRIC, "moso_db_query_duration_seconds");
        assert_eq!(POOL_CONNECTIONS_METRIC, "moso_db_pool_connections");
        assert_eq!(POOL_ACQUIRE_METRIC, "moso_db_pool_acquire_seconds");
        assert_eq!(
            TRANSACTION_RETRIES_METRIC,
            "moso_db_transaction_retries_total"
        );
    }
}

#[cfg(test)]
mod real_database {
    use super::test_support::{postgres, sqlite, unique_table};
    use super::*;
    use moso_sql::Sql;

    #[tokio::test]
    async fn an_in_memory_database_opens_pings_and_closes() {
        let db = sqlite().await;
        assert_eq!(db.backend(), Backend::Sqlite);
        db.ping().await.expect("`select 1` on a fresh pool");

        // Closing twice is what a shutdown lifespan does when two components
        // both hold the handle.
        db.close().await;
        db.close().await;
    }

    #[tokio::test]
    async fn the_pool_reports_what_it_is_holding() {
        let db = sqlite().await;
        db.ping().await.expect("ping");

        let stats = db.stats();
        assert_eq!(
            stats.max, 1,
            "an in-memory database is pinned to one connection"
        );
        assert!(stats.size >= 1, "the eager connection is open: {stats:?}");
        assert_eq!(stats.waiting, 0);
        assert!(!stats.is_saturated());
    }

    /// Acceptance criterion 4: an exhausted pool is a bounded failure, not a
    /// request that never answers.
    #[tokio::test]
    async fn an_exhausted_pool_times_out_rather_than_hanging() {
        let config = DatabaseConfig::from_url("sqlite://:memory:")
            .with_acquire_timeout(Duration::from_millis(150));
        let db = Db::connect(&config).await.expect("open");

        // The only connection is inside this transaction.
        let held = db.begin().await.expect("begin");

        let started = Instant::now();
        let error = db.ping().await.expect_err("there is no second connection");
        let waited = started.elapsed();

        assert!(
            matches!(error, Error::PoolTimeout { .. }),
            "expected a pool timeout, got {error:?}"
        );
        assert!(
            waited < Duration::from_secs(2),
            "the acquire waited {waited:?}, which is a hang rather than a timeout"
        );
        let text = error.to_string();
        assert!(text.contains("help:"), "{text}");
        assert!(text.contains("max_connections"), "{text}");

        held.rollback().await.expect("rollback");
        db.ping().await.expect("the connection came back");
    }

    #[tokio::test]
    async fn a_tenant_that_cannot_be_routed_refuses_every_statement() {
        let config = DatabaseConfig::from_url("sqlite://:memory:")
            .with_tenancy(TenancyModel::schema_per_tenant("t_"));
        let db = Db::connect(&config).await.expect("open");

        // A key with a space cannot name a schema, so the handle is poisoned
        // rather than quietly reading the untenanted rows.
        let hostile = db.for_tenant(TenantId::of(String::from("two words")));
        let error = hostile
            .ping()
            .await
            .expect_err("a poisoned handle runs nothing");
        assert!(error.to_string().contains("help:"), "{error}");
        assert!(
            hostile.health().await.is_down(),
            "and it reports itself unhealthy rather than pretending"
        );
    }

    #[tokio::test]
    async fn a_discriminator_tenant_shares_the_pool_and_carries_the_scope() {
        let db = sqlite().await;
        let acme = db.for_tenant(TenantId::of(1_i64));

        assert_eq!(acme.tenant(), Some(&TenantId::of(1_i64)));
        assert!(db.tenant().is_none());
        acme.ping()
            .await
            .expect("the discriminator model needs no second pool");
    }

    #[tokio::test]
    async fn read_your_writes_keeps_reads_on_the_primary_after_a_write() {
        let Some(url) = super::test_support::postgres_url() else {
            return;
        };
        // A "replica" pointing at the same server: enough to exercise the
        // rotation, the sticky window and the fallback without a second
        // container. `pg_last_xact_replay_timestamp()` is NULL on a primary,
        // which the sampler reads as no measurable lag.
        let config = DatabaseConfig::from_url(url.clone())
            .with_max_connections(4)
            .with_sticky_window(Duration::from_millis(400))
            .with_replica(ReplicaConfig::from_url(url));
        let db = Db::connect(&config).await.expect("open");

        assert_eq!(db.replicas().len(), 1);
        assert!(
            !core::ptr::eq(db.read(), &db),
            "with a healthy replica and no recent write, a read goes to the replica"
        );

        db.execute_raw("select 1")
            .await
            .expect("a read does not move the window");
        assert!(!core::ptr::eq(db.read(), &db), "a `select` is not a write");

        db.mark_write();
        assert!(
            core::ptr::eq(db.read(), &db),
            "acceptance 5: after a write, a read in the same window hits the primary"
        );
        assert!(
            !core::ptr::eq(db.read_stale(), &db),
            "`read_stale` is the opt-out for a read that tolerates staleness"
        );

        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !core::ptr::eq(db.read(), &db),
            "and the window expires rather than pinning the primary forever"
        );
        db.close().await;
    }

    #[tokio::test]
    async fn an_advisory_lock_is_held_against_a_second_taker() {
        let Some(db) = postgres().await else { return };
        let key = AdvisoryKey::hashed(&unique_table("moso_lock"));

        let held = db.advisory_lock(key).await.expect("nobody holds it");
        assert!(held.is_held());
        assert_eq!(held.key(), key);

        let refused = db
            .try_advisory_lock(key)
            .await
            .expect("asking is not an error");
        assert!(
            refused.is_none(),
            "a session-level lock is held against every other session, including ours"
        );

        held.unlock().await.expect("release");

        let now_free = db.try_advisory_lock(key).await.expect("ask again");
        assert!(now_free.is_some(), "the lock was released");
        drop(now_free);

        db.close().await;
    }

    #[tokio::test]
    async fn advisory_locks_say_so_on_sqlite_rather_than_pretending() {
        let db = sqlite().await;
        let error = db
            .advisory_lock(AdvisoryKey::of(1))
            .await
            .expect_err("SQLite has no advisory locks");
        assert!(
            matches!(
                error,
                Error::Unsupported {
                    backend: Backend::Sqlite,
                    ..
                }
            ),
            "{error:?}"
        );
        assert!(error.to_string().contains("SQLite"));
    }

    #[tokio::test]
    async fn a_real_server_answers_the_health_check() {
        let Some(db) = postgres().await else { return };
        assert!(db.health().await.is_up());

        let check = db.health_check();
        assert!(check.is_critical());
        assert!(!check.non_critical().is_critical());
        db.close().await;
    }

    #[tokio::test]
    async fn a_closed_pool_reports_the_reason_instead_of_hanging() {
        let db = sqlite().await;
        db.close().await;
        let error = db.ping().await.expect_err("the pool is closed");
        assert!(
            matches!(error, Error::Connection { .. } | Error::PoolTimeout { .. }),
            "{error:?}"
        );
    }

    /// A convenience for the tests above: run raw SQL with no parameters.
    impl Db {
        async fn execute_raw(&self, sql: &str) -> Result<u64> {
            use crate::executor::Executor as _;
            self.handle()
                .execute_sql(Sql::new(sql.to_owned(), []))
                .await
        }
    }
}
