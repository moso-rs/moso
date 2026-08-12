//! How a pool is opened: [`DatabaseConfig`], [`ReplicaConfig`], [`TlsMode`].
//!
//! The defaults here are the operationally safe ones rather than the fastest
//! ones, and each is argued for in its own documentation. Three are worth
//! reading before changing anything:
//!
//! * [`DatabaseConfig::max_connections`] is small on purpose. The classic
//!   outage is thirty pods times a hundred connections against a
//!   five-hundred-connection server, and a framework whose default makes that
//!   easy has chosen a side.
//! * [`DatabaseConfig::acquire_timeout`] exists so that an exhausted pool is a
//!   `503` with a `Retry-After` rather than a request that never answers.
//! * [`DatabaseConfig::application_name`] is always set, because the first
//!   question during a lock incident is *which service is holding it* and
//!   `pg_stat_activity` can only answer it if somebody filled the column in.
//!
//! This module is private; every type in it is re-exported from
//! [`crate::db`], which is where the frozen paths live.

use core::fmt;
use core::time::Duration;

use moso_core::config::SecretString;

use crate::db::Backend;
use crate::db::tenant::TenancyModel;
use crate::error::{Error, Result};

/// How to open the pool.
///
/// The defaults are the operationally safe ones, and the reasons are in the
/// field documentation — particularly [`DatabaseConfig::max_connections`],
/// whose small default exists because "thirty pods times a hundred connections
/// against a five-hundred-connection server" is a recurring outage.
///
/// ```
/// use moso_orm::DatabaseConfig;
///
/// let config = DatabaseConfig::from_url("postgres://localhost/shop")
///     .with_max_connections(12)
///     .with_application_name("shop-api");
///
/// assert_eq!(config.max_connections, 12);
/// assert_eq!(config.application_name, "shop-api");
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct DatabaseConfig {
    /// The connection URL. Secret, because it usually carries a password.
    pub url: SecretString,

    /// The most connections this process opens.
    ///
    /// Default: `max(4, cpus * 2)`, capped at 20. Small on purpose — the boot
    /// log prints the number next to a reminder that it is multiplied by the
    /// instance count.
    pub max_connections: u32,

    /// Connections kept open when idle. Default 1, and 0 in `dev`.
    pub min_connections: u32,

    /// How long a request waits for a connection before failing.
    ///
    /// Default 10 s, and the failure is a 503 with a `Retry-After`, not a hang.
    pub acquire_timeout: Duration,

    /// How long an unused connection stays open. Default 10 minutes.
    pub idle_timeout: Duration,

    /// How long any connection stays open. Default 30 minutes, which is what
    /// makes a failover recover without a restart.
    pub max_lifetime: Duration,

    /// Whether to round-trip before handing a connection out. Default `false`.
    pub test_before_acquire: bool,

    /// The session `statement_timeout`. Default 30 s.
    pub statement_timeout: Duration,

    /// The session `lock_timeout`. Default 5 s.
    pub lock_timeout: Duration,

    /// The name this process reports to the server, so `pg_stat_activity` says
    /// which service holds a lock. Default: the application's name.
    pub application_name: String,

    /// Statements slower than this log at `warn` in every profile.
    pub slow_query_ms: u64,

    /// Whether to attach the plan to a slow-query warning.
    pub explain_slow: bool,

    /// How many statements one request may issue before the N+1 detector warns.
    pub n_plus_one_threshold: u32,

    /// How long after a write reads stay on the primary. Default 3 s.
    pub sticky_window: Duration,

    /// The read replicas.
    pub replicas: Vec<ReplicaConfig>,

    /// How to negotiate TLS.
    pub tls: TlsMode,

    /// Which multi-tenancy model the application uses.
    ///
    /// Default [`TenancyModel::Discriminator`], which needs no pool support at
    /// all: the tenant is a bound parameter in every statement. The other two
    /// models change what [`Db::for_tenant`](crate::Db::for_tenant) does.
    pub tenancy: TenancyModel,

    /// Whether a transaction-mode connection pooler sits in front of the
    /// server.
    ///
    /// Default [`PgBouncerMode::Detect`]. See that type for what detection
    /// costs and why the answer changes how statements are prepared.
    pub pgbouncer: PgBouncerMode,

    /// The most per-tenant pools kept open at once, for the two tenancy models
    /// that need one. Default 32; the least recently used is closed past it.
    pub max_tenant_pools: usize,
}

impl DatabaseConfig {
    /// A configuration with `url` and the documented defaults.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://:memory:");
    /// assert_eq!(config.min_connections, 1);
    /// assert!(config.replicas.is_empty());
    /// ```
    #[must_use]
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: SecretString::new(url.into()),
            max_connections: default_max_connections(),
            min_connections: 1,
            acquire_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(600),
            max_lifetime: Duration::from_secs(1800),
            test_before_acquire: false,
            statement_timeout: Duration::from_secs(30),
            lock_timeout: Duration::from_secs(5),
            application_name: String::from("moso"),
            slow_query_ms: 200,
            explain_slow: false,
            n_plus_one_threshold: 20,
            sticky_window: Duration::from_secs(3),
            replicas: Vec::new(),
            tls: TlsMode::default(),
            tenancy: TenancyModel::default(),
            pgbouncer: PgBouncerMode::default(),
            max_tenant_pools: 32,
        }
    }

    /// Sets the pool's maximum size.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://x.db").with_max_connections(4);
    /// assert_eq!(config.max_connections, 4);
    /// ```
    #[must_use]
    pub fn with_max_connections(mut self, max: u32) -> Self {
        self.max_connections = max;
        self
    }

    /// Sets the acquire timeout.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://x.db")
    ///     .with_acquire_timeout(Duration::from_secs(2));
    /// assert_eq!(config.acquire_timeout, Duration::from_secs(2));
    /// ```
    #[must_use]
    pub fn with_acquire_timeout(mut self, timeout: Duration) -> Self {
        self.acquire_timeout = timeout;
        self
    }

    /// Sets the name reported to the server.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://x.db").with_application_name("shop");
    /// assert_eq!(config.application_name, "shop");
    /// ```
    #[must_use]
    pub fn with_application_name(mut self, name: impl Into<String>) -> Self {
        self.application_name = name.into();
        self
    }

    /// Adds a read replica.
    ///
    /// ```
    /// use moso_orm::{DatabaseConfig, ReplicaConfig};
    ///
    /// let config = DatabaseConfig::from_url("postgres://primary/shop")
    ///     .with_replica(ReplicaConfig::from_url("postgres://replica/shop"));
    /// assert_eq!(config.replicas.len(), 1);
    /// ```
    #[must_use]
    pub fn with_replica(mut self, replica: ReplicaConfig) -> Self {
        self.replicas.push(replica);
        self
    }

    /// Sets the TLS mode.
    ///
    /// ```
    /// use moso_orm::{DatabaseConfig, TlsMode};
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db").with_tls(TlsMode::Require);
    /// assert_eq!(config.tls, TlsMode::Require);
    /// ```
    #[must_use]
    pub fn with_tls(mut self, tls: TlsMode) -> Self {
        self.tls = tls;
        self
    }

    /// Sets how many connections stay open when nothing is happening.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// assert_eq!(DatabaseConfig::from_url("sqlite://x.db").with_min_connections(0).min_connections, 0);
    /// ```
    #[must_use]
    pub fn with_min_connections(mut self, min: u32) -> Self {
        self.min_connections = min;
        self
    }

    /// Sets how long an unused connection stays open.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://x.db")
    ///     .with_idle_timeout(Duration::from_secs(60));
    /// assert_eq!(config.idle_timeout, Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn with_idle_timeout(mut self, timeout: Duration) -> Self {
        self.idle_timeout = timeout;
        self
    }

    /// Sets how long any connection stays open.
    ///
    /// This is the setting that makes a failover recover on its own: every
    /// connection is retired within `max_lifetime`, so a pool that is pinned to
    /// a demoted primary drains without a restart.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://x.db")
    ///     .with_max_lifetime(Duration::from_secs(900));
    /// assert_eq!(config.max_lifetime, Duration::from_secs(900));
    /// ```
    #[must_use]
    pub fn with_max_lifetime(mut self, lifetime: Duration) -> Self {
        self.max_lifetime = lifetime;
        self
    }

    /// Whether to round-trip a connection before handing it out.
    ///
    /// Costs one round trip per acquire, and buys a pool that never hands out a
    /// connection the server has already closed. Worth it behind a proxy that
    /// drops idle connections silently.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// assert!(DatabaseConfig::from_url("sqlite://x.db").with_test_before_acquire(true).test_before_acquire);
    /// ```
    #[must_use]
    pub fn with_test_before_acquire(mut self, test: bool) -> Self {
        self.test_before_acquire = test;
        self
    }

    /// Sets the session `statement_timeout`.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db")
    ///     .with_statement_timeout(Duration::from_secs(5));
    /// assert_eq!(config.statement_timeout, Duration::from_secs(5));
    /// ```
    #[must_use]
    pub fn with_statement_timeout(mut self, timeout: Duration) -> Self {
        self.statement_timeout = timeout;
        self
    }

    /// Sets the session `lock_timeout`.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db")
    ///     .with_lock_timeout(Duration::from_secs(1));
    /// assert_eq!(config.lock_timeout, Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn with_lock_timeout(mut self, timeout: Duration) -> Self {
        self.lock_timeout = timeout;
        self
    }

    /// Sets the threshold above which a statement logs at `warn`.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// assert_eq!(DatabaseConfig::from_url("sqlite://x.db").with_slow_query_ms(50).slow_query_ms, 50);
    /// ```
    #[must_use]
    pub fn with_slow_query_ms(mut self, threshold: u64) -> Self {
        self.slow_query_ms = threshold;
        self
    }

    /// Whether a slow-query warning carries the plan.
    ///
    /// Off by default: `EXPLAIN` is a second round trip on a server that is
    /// already struggling, which is exactly when the warning fires.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// assert!(DatabaseConfig::from_url("postgres://h/db").with_explain_slow(true).explain_slow);
    /// ```
    #[must_use]
    pub fn with_explain_slow(mut self, explain: bool) -> Self {
        self.explain_slow = explain;
        self
    }

    /// Sets how many statements a request may issue before the N+1 detector
    /// says something.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("sqlite://x.db").with_n_plus_one_threshold(50);
    /// assert_eq!(config.n_plus_one_threshold, 50);
    /// ```
    #[must_use]
    pub fn with_n_plus_one_threshold(mut self, threshold: u32) -> Self {
        self.n_plus_one_threshold = threshold;
        self
    }

    /// Sets how long after a write reads stay on the primary.
    ///
    /// Zero turns read-your-writes off, which is a decision worth writing down
    /// rather than discovering.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db")
    ///     .with_sticky_window(Duration::from_secs(10));
    /// assert_eq!(config.sticky_window, Duration::from_secs(10));
    /// ```
    #[must_use]
    pub fn with_sticky_window(mut self, window: Duration) -> Self {
        self.sticky_window = window;
        self
    }

    /// Sets the multi-tenancy model.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    /// use moso_orm::db::TenancyModel;
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db")
    ///     .with_tenancy(TenancyModel::schema_per_tenant("tenant_"));
    /// assert!(config.tenancy.routes_connections());
    /// ```
    #[must_use]
    pub fn with_tenancy(mut self, tenancy: TenancyModel) -> Self {
        self.tenancy = tenancy;
        self
    }

    /// Declares whether a transaction-mode pooler is in front of the server.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    /// use moso_orm::db::PgBouncerMode;
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db")
    ///     .with_pgbouncer(PgBouncerMode::Assume);
    /// assert_eq!(config.pgbouncer, PgBouncerMode::Assume);
    /// ```
    #[must_use]
    pub fn with_pgbouncer(mut self, mode: PgBouncerMode) -> Self {
        self.pgbouncer = mode;
        self
    }

    /// Sets how many per-tenant pools stay open at once.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// assert_eq!(DatabaseConfig::from_url("postgres://h/db").with_max_tenant_pools(4).max_tenant_pools, 4);
    /// ```
    #[must_use]
    pub fn with_max_tenant_pools(mut self, cap: usize) -> Self {
        self.max_tenant_pools = cap;
        self
    }

    /// The development defaults: no idle connections held, statements logged
    /// sooner, the N+1 detector on a hair trigger.
    ///
    /// Holding a connection open from every developer's laptop against a shared
    /// database is how a team of eight exhausts a hundred-connection server
    /// without anyone deploying anything.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db").for_dev();
    /// assert_eq!(config.min_connections, 0);
    /// assert!(config.slow_query_ms < 200);
    /// ```
    #[must_use]
    pub fn for_dev(mut self) -> Self {
        self.min_connections = 0;
        self.slow_query_ms = 50;
        self.n_plus_one_threshold = 10;
        self
    }

    /// The backend the URL names.
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] for an unsupported scheme.
    ///
    /// ```
    /// use moso_orm::{Backend, DatabaseConfig};
    ///
    /// let config = DatabaseConfig::from_url("postgres://h/db");
    /// assert_eq!(config.backend()?, Backend::Postgres);
    /// # Ok::<(), moso_orm::Error>(())
    /// ```
    pub fn backend(&self) -> Result<Backend> {
        Backend::from_url(self.url.expose())
    }

    /// The line the boot log prints, which names the multiplication that
    /// exhausts a server's connection limit.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let line = DatabaseConfig::from_url("sqlite://x.db").boot_summary();
    /// assert!(line.contains("pool max="));
    /// ```
    #[must_use]
    pub fn boot_summary(&self) -> String {
        format!(
            "db: pool max={} min={} acquire_timeout={}s \
             (×N instances must stay under your server's max_connections)",
            self.max_connections,
            self.min_connections,
            self.acquire_timeout.as_secs()
        )
    }

    /// The URL with its password replaced, for a log line.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let config = DatabaseConfig::from_url("postgres://sam:hunter2@db/shop");
    /// assert_eq!(config.redacted_url(), "postgres://sam:***@db/shop");
    /// ```
    #[must_use]
    pub fn redacted_url(&self) -> String {
        redact_password(self.url.expose())
    }

    /// Rejects a configuration the pool could not honour.
    ///
    /// Called by [`Db::connect`](crate::Db::connect) before anything is opened,
    /// so a typo in `database.min_connections` is a boot error naming the field
    /// rather than a pool that deadlocks under load.
    ///
    /// # Errors
    ///
    /// [`Error::Configuration`] naming the field and the rule it broke.
    ///
    /// ```
    /// use moso_orm::DatabaseConfig;
    ///
    /// let bad = DatabaseConfig::from_url("postgres://h/db")
    ///     .with_max_connections(2)
    ///     .with_min_connections(8);
    /// assert!(bad.validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.url.is_empty() {
            return Err(Error::Configuration {
                detail: String::from(
                    "`database.url` is empty\n  \
                     help: set `DATABASE_URL`, or `database.url` in the configuration file",
                ),
            });
        }
        let backend = self.backend()?;
        if self.max_connections == 0 {
            return Err(Error::Configuration {
                detail: String::from(
                    "`database.max_connections` is 0, so no statement could ever run\n  \
                     help: 4 to 20 is the usual range; remember it is multiplied by the \
                     number of instances",
                ),
            });
        }
        if self.min_connections > self.max_connections {
            return Err(Error::Configuration {
                detail: format!(
                    "`database.min_connections` ({}) is above `database.max_connections` ({})\n  \
                     help: lower the minimum, or raise the maximum",
                    self.min_connections, self.max_connections
                ),
            });
        }
        if self.acquire_timeout.is_zero() {
            return Err(Error::Configuration {
                detail: String::from(
                    "`database.acquire_timeout` is 0, so every acquire fails immediately\n  \
                     help: 10s is the default; the point of the timeout is a 503 rather than \
                     a hang, not a 503 rather than a query",
                ),
            });
        }
        if backend == Backend::Sqlite && !self.replicas.is_empty() {
            return Err(Error::Configuration {
                detail: String::from(
                    "SQLite has no read replicas, and `database.replicas` is not empty\n  \
                     help: remove the replicas, or point `database.url` at PostgreSQL",
                ),
            });
        }
        for replica in &self.replicas {
            if replica.weight == 0 {
                return Err(Error::Configuration {
                    detail: format!(
                        "the replica `{}` has weight 0, so it would never be chosen\n  \
                         help: remove it, or give it a weight of at least 1",
                        redact_password(replica.url.expose())
                    ),
                });
            }
        }
        Ok(())
    }

    /// Whether prepared statements may be cached on this connection.
    ///
    /// `false` behind a transaction-mode pooler, where the server connection
    /// under a client connection changes between transactions and a cached
    /// statement handle refers to a statement the new backend has never seen.
    pub(crate) fn statement_cache_allowed(&self, detected: bool) -> bool {
        match self.pgbouncer {
            PgBouncerMode::Assume => false,
            PgBouncerMode::Never => true,
            PgBouncerMode::Detect => !detected,
        }
    }
}

/// The default pool size: two per core, at least 4, at most 20.
fn default_max_connections() -> u32 {
    let cpus = std::thread::available_parallelism().map_or(2, core::num::NonZeroUsize::get);
    let doubled = u32::try_from(cpus.saturating_mul(2)).unwrap_or(u32::MAX);
    doubled.clamp(4, 20)
}

/// Replaces the password in a `scheme://user:password@host/db` URL.
///
/// Substring surgery rather than a URL parse: this runs on the error path and
/// on a URL the parser may just have rejected, and a redactor that panics on
/// malformed input is worse than useless.
pub(crate) fn redact_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    // The authority ends at the first `/`, `?` or `#`; a `@` after that is part
    // of a path or a query and must not be mistaken for the credentials.
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(authority_end);
    let Some((credentials, host)) = authority.rsplit_once('@') else {
        return url.to_owned();
    };
    let user = credentials.split_once(':').map_or(credentials, |(u, _)| u);
    if credentials.contains(':') {
        format!("{scheme}://{user}:***@{host}{tail}")
    } else {
        url.to_owned()
    }
}

/// One read replica.
///
/// ```
/// use core::time::Duration;
/// use moso_orm::ReplicaConfig;
///
/// let replica = ReplicaConfig::from_url("postgres://replica/shop")
///     .with_weight(2)
///     .with_max_lag(Duration::from_secs(5));
///
/// assert_eq!(replica.weight, 2);
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct ReplicaConfig {
    /// The replica's connection URL.
    pub url: SecretString,
    /// Its share of the round-robin. Default 1.
    pub weight: u32,
    /// Lag above which the replica leaves the rotation. Default 5 s.
    pub max_lag: Duration,
}

impl ReplicaConfig {
    /// A replica with the default weight and lag tolerance.
    ///
    /// ```
    /// use moso_orm::ReplicaConfig;
    ///
    /// assert_eq!(ReplicaConfig::from_url("postgres://r/db").weight, 1);
    /// ```
    #[must_use]
    pub fn from_url(url: impl Into<String>) -> Self {
        Self {
            url: SecretString::new(url.into()),
            weight: 1,
            max_lag: Duration::from_secs(5),
        }
    }

    /// Sets the round-robin weight.
    ///
    /// ```
    /// use moso_orm::ReplicaConfig;
    ///
    /// assert_eq!(ReplicaConfig::from_url("postgres://r/db").with_weight(3).weight, 3);
    /// ```
    #[must_use]
    pub fn with_weight(mut self, weight: u32) -> Self {
        self.weight = weight;
        self
    }

    /// Sets the lag above which the replica is skipped.
    ///
    /// ```
    /// use core::time::Duration;
    /// use moso_orm::ReplicaConfig;
    ///
    /// let replica = ReplicaConfig::from_url("postgres://r/db").with_max_lag(Duration::from_secs(1));
    /// assert_eq!(replica.max_lag, Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn with_max_lag(mut self, lag: Duration) -> Self {
        self.max_lag = lag;
        self
    }

    /// The configuration a replica pool is opened with: the primary's, with
    /// this replica's URL and no replicas of its own.
    ///
    /// A replica pool is deliberately smaller than the primary's — reads are
    /// the traffic you can shed, and a replica that eats the connection budget
    /// takes the primary down with it.
    pub(crate) fn pool_config(&self, primary: &DatabaseConfig) -> DatabaseConfig {
        let mut config = primary.clone();
        config.url = self.url.clone();
        config.replicas = Vec::new();
        config.min_connections = 0;
        config.application_name = format!("{} (replica)", primary.application_name);
        config
    }
}

/// How hard to insist on TLS.
///
/// ```
/// use moso_orm::TlsMode;
///
/// assert_eq!(TlsMode::default(), TlsMode::Prefer);
/// assert!(TlsMode::VerifyFull.verifies_the_certificate());
/// assert!(!TlsMode::Require.verifies_the_certificate());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum TlsMode {
    /// Never negotiate TLS. For a unix socket or a local SQLite file.
    Disable,
    /// Use TLS when the server offers it. The default.
    #[default]
    Prefer,
    /// Insist on TLS, without checking the certificate. Stops passive
    /// eavesdropping and not an active attacker.
    Require,
    /// Insist on TLS and verify the certificate chain and the hostname. What a
    /// production deployment across a network should use.
    VerifyFull,
}

impl TlsMode {
    /// Whether the certificate is checked.
    ///
    /// ```
    /// use moso_orm::TlsMode;
    ///
    /// assert!(TlsMode::VerifyFull.verifies_the_certificate());
    /// ```
    #[must_use]
    pub const fn verifies_the_certificate(self) -> bool {
        matches!(self, Self::VerifyFull)
    }

    /// Whether a plaintext connection is refused.
    ///
    /// ```
    /// use moso_orm::TlsMode;
    ///
    /// assert!(TlsMode::Require.requires_tls());
    /// assert!(!TlsMode::Prefer.requires_tls());
    /// ```
    #[must_use]
    pub const fn requires_tls(self) -> bool {
        matches!(self, Self::Require | Self::VerifyFull)
    }

    /// The `sslmode` spelling PostgreSQL uses, so that a log line and a
    /// `psql` invocation agree.
    ///
    /// ```
    /// use moso_orm::TlsMode;
    ///
    /// assert_eq!(TlsMode::VerifyFull.as_sslmode(), "verify-full");
    /// ```
    #[must_use]
    pub const fn as_sslmode(self) -> &'static str {
        match self {
            Self::Disable => "disable",
            Self::Prefer => "prefer",
            Self::Require => "require",
            Self::VerifyFull => "verify-full",
        }
    }
}

impl fmt::Display for TlsMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_sslmode())
    }
}

/// Whether a transaction-mode connection pooler sits between this process and
/// PostgreSQL.
///
/// The mismatch this exists for is a top-tier production footgun. A
/// transaction-mode pooler hands a *different* server connection to each
/// transaction, so a prepared statement created on one is not there on the
/// next, and the application starts failing intermittently with
/// `prepared statement "sqlx_s_3" does not exist` under load and never in
/// staging. When Moso knows a pooler is there it stops caching prepared
/// statements and says so in the boot log.
///
/// ```
/// use moso_orm::db::PgBouncerMode;
///
/// assert_eq!(PgBouncerMode::default(), PgBouncerMode::Detect);
/// assert!(PgBouncerMode::Assume.disables_the_statement_cache());
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PgBouncerMode {
    /// Probe once at boot, and disable the statement cache if a pooler is
    /// found. The default.
    ///
    /// The probe asks the same connection for `pg_backend_pid()` inside
    /// several separate transactions. A direct connection cannot change its
    /// backend process id, so a change **proves** a pooler is reassigning the
    /// server connection: the detection has no false positives. It can have
    /// false negatives — an idle pooler often reuses the same server
    /// connection — which is why [`PgBouncerMode::Assume`] exists.
    #[default]
    Detect,
    /// Take it as given. Use this when the probe cannot see the pooler because
    /// the deployment is quiet at boot.
    Assume,
    /// There is no pooler; skip the probe and keep the statement cache.
    Never,
}

impl PgBouncerMode {
    /// Whether this mode turns the prepared-statement cache off on its own,
    /// before any probe runs.
    ///
    /// ```
    /// use moso_orm::db::PgBouncerMode;
    ///
    /// assert!(PgBouncerMode::Assume.disables_the_statement_cache());
    /// assert!(!PgBouncerMode::Detect.disables_the_statement_cache());
    /// ```
    #[must_use]
    pub const fn disables_the_statement_cache(self) -> bool {
        matches!(self, Self::Assume)
    }

    /// Whether the boot probe runs.
    ///
    /// ```
    /// use moso_orm::db::PgBouncerMode;
    ///
    /// assert!(PgBouncerMode::Detect.probes());
    /// assert!(!PgBouncerMode::Never.probes());
    /// ```
    #[must_use]
    pub const fn probes(self) -> bool {
        matches!(self, Self::Detect)
    }
}

impl fmt::Display for PgBouncerMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Detect => "detect",
            Self::Assume => "assume",
            Self::Never => "never",
        })
    }
}

/// Whether a URL already says how to negotiate TLS.
///
/// When it does, the URL wins: an operator who wrote `?sslmode=verify-full`
/// into a connection string meant it, and silently overriding it with a
/// configuration default is how a production database ends up talking
/// plaintext.
#[cfg(feature = "postgres")]
pub(crate) fn url_sets_sslmode(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        pair.split_once('=')
            .is_some_and(|(k, _)| k.eq_ignore_ascii_case("sslmode"))
    })
}

/// Whether a URL carries the `pgbouncer=true` marker.
///
/// Several managed PostgreSQL products (and every ORM that has been bitten by
/// this) use that query parameter to mean "there is a transaction-mode pooler
/// in front of me". It is a zero-false-positive signal, so it is honoured even
/// under [`PgBouncerMode::Never`]-adjacent settings — see
/// [`DatabaseConfig::statement_cache_allowed`].
pub(crate) fn url_declares_pgbouncer(url: &str) -> bool {
    let Some((_, query)) = url.split_once('?') else {
        return false;
    };
    query.split('&').any(|pair| {
        pair.split_once('=').is_some_and(|(key, value)| {
            key.eq_ignore_ascii_case("pgbouncer") && value.eq_ignore_ascii_case("true")
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn the_url_is_a_secret_and_the_redaction_keeps_the_rest() {
        let config = DatabaseConfig::from_url("postgres://user:hunter2@h/db");
        let rendered = format!("{:?}", config.url);
        assert!(!rendered.contains("hunter2"), "{rendered}");
        assert_eq!(config.redacted_url(), "postgres://user:***@h/db");
    }

    #[test]
    fn redaction_survives_urls_that_would_break_a_parser() {
        // No credentials at all.
        assert_eq!(redact_password("postgres://h/db"), "postgres://h/db");
        // A user with no password.
        assert_eq!(
            redact_password("postgres://sam@h/db"),
            "postgres://sam@h/db"
        );
        // An `@` in the path must not be read as the credential separator.
        assert_eq!(
            redact_password("postgres://h/db?options=a@b"),
            "postgres://h/db?options=a@b"
        );
        // A password containing `@`, which `split_once` would get wrong.
        assert_eq!(
            redact_password("postgres://sam:p@ss@h/db"),
            "postgres://sam:***@h/db"
        );
        // Not a URL at all.
        assert_eq!(redact_password("nonsense"), "nonsense");
    }

    #[test]
    fn validation_names_the_field_and_the_rule() {
        let error = DatabaseConfig::from_url("postgres://h/db")
            .with_max_connections(2)
            .with_min_connections(8)
            .validate()
            .expect_err("min above max");
        let text = error.to_string();
        assert!(text.contains("min_connections"), "{text}");
        assert!(text.contains("max_connections"), "{text}");
        assert!(text.contains("help:"), "{text}");

        let error = DatabaseConfig::from_url("postgres://h/db")
            .with_max_connections(0)
            .validate()
            .expect_err("a pool of zero can run nothing");
        assert!(error.to_string().contains("help:"));

        let error = DatabaseConfig::from_url("postgres://h/db")
            .with_acquire_timeout(Duration::ZERO)
            .validate()
            .expect_err("zero acquire timeout");
        assert!(error.to_string().contains("acquire_timeout"));

        DatabaseConfig::from_url("postgres://h/db")
            .validate()
            .expect("the defaults are valid");
    }

    #[test]
    fn sqlite_cannot_have_replicas_and_the_error_says_so() {
        let error = DatabaseConfig::from_url("sqlite://x.db")
            .with_replica(ReplicaConfig::from_url("sqlite://y.db"))
            .validate()
            .expect_err("SQLite has no replication");
        assert!(error.to_string().contains("SQLite"), "{error}");
    }

    #[test]
    fn a_zero_weight_replica_is_a_configuration_error_not_a_silent_skip() {
        let error = DatabaseConfig::from_url("postgres://h/db")
            .with_replica(ReplicaConfig::from_url("postgres://r:pw@h/db").with_weight(0))
            .validate()
            .expect_err("weight 0 would never be chosen");
        let text = error.to_string();
        assert!(text.contains("weight 0"), "{text}");
        assert!(!text.contains("pw"), "the replica password leaked: {text}");
    }

    #[test]
    fn dev_holds_no_idle_connections() {
        let config = DatabaseConfig::from_url("postgres://h/db").for_dev();
        assert_eq!(config.min_connections, 0);
        assert_eq!(config.slow_query_ms, 50);
        config.validate().expect("dev defaults are valid");
    }

    #[test]
    fn a_replica_pool_inherits_the_primary_settings_but_not_its_replicas() {
        let primary = DatabaseConfig::from_url("postgres://p/db")
            .with_application_name("shop")
            .with_max_connections(9)
            .with_replica(ReplicaConfig::from_url("postgres://r/db"));
        let replica = primary.replicas[0].pool_config(&primary);

        assert_eq!(replica.url.expose(), "postgres://r/db");
        assert_eq!(replica.max_connections, 9);
        assert_eq!(replica.min_connections, 0);
        assert!(replica.replicas.is_empty());
        assert!(replica.application_name.contains("replica"));
    }

    #[test]
    fn tls_defaults_to_prefer_and_only_verify_full_checks_the_chain() {
        assert_eq!(TlsMode::default(), TlsMode::Prefer);
        assert!(!TlsMode::Prefer.requires_tls());
        assert!(TlsMode::Require.requires_tls());
        assert!(!TlsMode::Require.verifies_the_certificate());
        assert!(TlsMode::VerifyFull.verifies_the_certificate());
        assert_eq!(TlsMode::VerifyFull.to_string(), "verify-full");
    }

    #[test]
    #[cfg(feature = "postgres")]
    fn a_url_that_names_sslmode_is_left_alone() {
        assert!(url_sets_sslmode("postgres://h/db?sslmode=require"));
        assert!(url_sets_sslmode("postgres://h/db?a=b&SSLMode=disable"));
        assert!(!url_sets_sslmode("postgres://h/db"));
        assert!(!url_sets_sslmode("postgres://h/db?application_name=x"));
    }

    #[test]
    fn the_pgbouncer_url_marker_is_honoured() {
        assert!(url_declares_pgbouncer("postgres://h/db?pgbouncer=true"));
        assert!(url_declares_pgbouncer("postgres://h/db?a=b&pgbouncer=TRUE"));
        assert!(!url_declares_pgbouncer("postgres://h/db?pgbouncer=false"));
        assert!(!url_declares_pgbouncer("postgres://h/db"));
    }

    #[test]
    fn the_statement_cache_follows_the_mode_and_the_probe() {
        let detect = DatabaseConfig::from_url("postgres://h/db");
        assert!(detect.statement_cache_allowed(false));
        assert!(!detect.statement_cache_allowed(true));

        let assume = detect.clone().with_pgbouncer(PgBouncerMode::Assume);
        assert!(!assume.statement_cache_allowed(false));

        let never = detect.with_pgbouncer(PgBouncerMode::Never);
        assert!(never.statement_cache_allowed(true));
    }
}
