//! Choosing a backend from configuration rather than from code.
//!
//! ```toml
//! [kv]
//! backend = "redis"
//! url = "redis://localhost:6379"
//! pool_size = 8
//! ```
//!
//! The point of this module is one sentence in
//! `docs/02-data/25-kv-cache.md`: **backend choice is config, not code.** A
//! handler that reads a cache is written once and runs against an in-process
//! map in tests and Redis in production, and the only thing that changes is a
//! string.
//!
//! # Why this is not a `#[derive(Config)]`
//!
//! `#[derive(Config)]` lives in `moso-macros` and generates code that resolves
//! against `::moso::__private::*`, so it only works in a crate that depends on
//! the `moso` facade — which `moso-kv` deliberately does not. An application
//! declares its own `#[derive(Config)]` struct and converts it into a
//! [`KvConfig`]; the fields line up one for one and the conversion is a
//! constructor call.
//!
//! ```
//! use moso_kv::{KvBackend, KvConfig};
//!
//! // What an application's `#[derive(Config)]` struct hands over.
//! let config = KvConfig::new("shop", KvBackend::Memory).capacity(50_000);
//!
//! assert_eq!(config.backend, KvBackend::Memory);
//! assert_eq!(config.app, "shop");
//! ```

use std::time::Duration;

use moso_core::SecretString;

use crate::breaker::BreakerConfig;
use crate::error::{Error, Result};
use crate::key::validate_name;
use crate::kv::Kv;

/// Which backend to open.
///
/// ```
/// use moso_kv::KvBackend;
///
/// assert_eq!("redis".parse::<KvBackend>().expect("known"), KvBackend::Redis);
/// assert_eq!(KvBackend::Postgres.as_str(), "postgres");
/// assert!("dynamo".parse::<KvBackend>().is_err());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum KvBackend {
    /// In-process, `moka`-backed. The default, so a test suite needs nothing.
    #[default]
    Memory,
    /// Redis or Valkey, through `fred`.
    Redis,
    /// A PostgreSQL table, with a TTL sweeper and `LISTEN`/`NOTIFY`.
    Postgres,
}

impl KvBackend {
    /// The name used in configuration and in logs.
    ///
    /// ```
    /// use moso_kv::KvBackend;
    ///
    /// assert_eq!(KvBackend::Memory.as_str(), "memory");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            KvBackend::Memory => "memory",
            KvBackend::Redis => "redis",
            KvBackend::Postgres => "postgres",
        }
    }

    /// Whether this backend needs a `url`.
    ///
    /// ```
    /// use moso_kv::KvBackend;
    ///
    /// assert!(!KvBackend::Memory.needs_url());
    /// assert!(KvBackend::Redis.needs_url());
    /// ```
    #[must_use]
    pub const fn needs_url(self) -> bool {
        !matches!(self, KvBackend::Memory)
    }

    /// The cargo feature that has to be on for this backend to exist.
    ///
    /// ```
    /// use moso_kv::KvBackend;
    ///
    /// assert_eq!(KvBackend::Postgres.feature(), "pg-kv");
    /// ```
    #[must_use]
    pub const fn feature(self) -> &'static str {
        match self {
            KvBackend::Memory => "memory",
            KvBackend::Redis => "redis",
            KvBackend::Postgres => "pg-kv",
        }
    }
}

impl std::fmt::Display for KvBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for KvBackend {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "memory" | "in-memory" | "inmemory" => Ok(KvBackend::Memory),
            "redis" | "valkey" => Ok(KvBackend::Redis),
            "postgres" | "postgresql" | "pg" => Ok(KvBackend::Postgres),
            other => Err(Error::Config {
                detail: format!(
                    "`{other}` is not a kv backend. help: one of `memory`, `redis`, `postgres`"
                ),
            }),
        }
    }
}

/// Everything needed to open a [`Kv`].
///
/// ```
/// use moso_kv::{KvBackend, KvConfig};
/// use std::time::Duration;
///
/// let config = KvConfig::new("shop", KvBackend::Redis)
///     .url("redis://localhost:6379")
///     .pool_size(8)
///     .connect_timeout(Duration::from_secs(3));
///
/// assert!(config.validate().is_ok());
/// assert_eq!(config.pool_size, 8);
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct KvConfig {
    /// The application name, which becomes the third segment of every key.
    ///
    /// Two applications sharing one Redis differ here and nowhere else.
    pub app: String,

    /// Which backend.
    pub backend: KvBackend,

    /// Where it is. Secret, because it usually carries a password.
    ///
    /// Required for every backend except [`KvBackend::Memory`].
    pub url: Option<SecretString>,

    /// How many connections to hold. Ignored by the memory backend.
    pub pool_size: u32,

    /// How long to wait for a connection before failing.
    pub connect_timeout: Duration,

    /// How many entries the memory backend holds. Ignored by the others.
    pub capacity: u64,

    /// The table the PostgreSQL backend uses. Ignored by the others.
    pub table: String,

    /// How the circuit breaker opens and recovers.
    pub breaker: BreakerConfig,

    /// How often the PostgreSQL backend deletes expired rows.
    ///
    /// [`Duration::ZERO`] turns the sweeper off, for a deployment that runs it
    /// from `cron` instead.
    pub sweep_interval: Duration,
}

impl KvConfig {
    /// A configuration with the documented defaults.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// let config = KvConfig::new("shop", KvBackend::Memory);
    /// assert_eq!(config.pool_size, 8);
    /// assert_eq!(config.table, "moso_kv");
    /// ```
    #[must_use]
    pub fn new(app: impl Into<String>, backend: KvBackend) -> Self {
        Self {
            app: app.into(),
            backend,
            url: None,
            pool_size: 8,
            connect_timeout: Duration::from_secs(5),
            capacity: 10_000,
            table: String::from("moso_kv"),
            breaker: BreakerConfig::default(),
            sweep_interval: Duration::from_secs(30),
        }
    }

    /// Set the URL.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// let config = KvConfig::new("shop", KvBackend::Redis).url("redis://localhost:6379");
    /// assert!(config.url.is_some());
    /// ```
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(SecretString::new(url.into()));
        self
    }

    /// Set the pool size.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// assert_eq!(KvConfig::new("a", KvBackend::Redis).pool_size(4).pool_size, 4);
    /// ```
    #[must_use]
    pub fn pool_size(mut self, size: u32) -> Self {
        self.pool_size = size.max(1);
        self
    }

    /// Set the connect timeout.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    /// use std::time::Duration;
    ///
    /// let config = KvConfig::new("a", KvBackend::Redis).connect_timeout(Duration::from_secs(2));
    /// assert_eq!(config.connect_timeout, Duration::from_secs(2));
    /// ```
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the memory backend's capacity.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// assert_eq!(KvConfig::new("a", KvBackend::Memory).capacity(99).capacity, 99);
    /// ```
    #[must_use]
    pub fn capacity(mut self, capacity: u64) -> Self {
        self.capacity = capacity;
        self
    }

    /// Set the PostgreSQL backend's table name.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// assert_eq!(KvConfig::new("a", KvBackend::Postgres).table("cache").table, "cache");
    /// ```
    #[must_use]
    pub fn table(mut self, table: impl Into<String>) -> Self {
        self.table = table.into();
        self
    }

    /// Set the circuit breaker's configuration.
    ///
    /// ```
    /// use moso_kv::breaker::BreakerConfig;
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// let config = KvConfig::new("a", KvBackend::Redis).breaker(BreakerConfig::never());
    /// assert_eq!(config.breaker.failure_threshold, u32::MAX);
    /// ```
    #[must_use]
    pub fn breaker(mut self, breaker: BreakerConfig) -> Self {
        self.breaker = breaker;
        self
    }

    /// Set how often the PostgreSQL sweeper runs.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    /// use std::time::Duration;
    ///
    /// let config = KvConfig::new("a", KvBackend::Postgres).sweep_interval(Duration::ZERO);
    /// assert_eq!(config.sweep_interval, Duration::ZERO);
    /// ```
    #[must_use]
    pub fn sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    /// Read a configuration from the environment.
    ///
    /// | Variable | Default |
    /// | --- | --- |
    /// | `KV_BACKEND` | `memory` |
    /// | `KV_URL` | none |
    /// | `KV_POOL_SIZE` | `8` |
    /// | `KV_CAPACITY` | `10000` |
    /// | `KV_TABLE` | `moso_kv` |
    ///
    /// The twelve-factor path, for a binary that has no configuration layer of
    /// its own. An application that does have one should build a [`KvConfig`]
    /// from it instead, so that `moso config` can see the settings.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when a variable is set to something unparseable.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// // With nothing set, the memory backend.
    /// let config = KvConfig::from_env("shop").expect("defaults");
    /// assert_eq!(config.app, "shop");
    /// ```
    pub fn from_env(app: impl Into<String>) -> Result<Self> {
        let backend = match std::env::var("KV_BACKEND") {
            Ok(value) if !value.trim().is_empty() => value.parse()?,
            _ => KvBackend::Memory,
        };

        let mut config = Self::new(app, backend);

        if let Ok(url) = std::env::var("KV_URL")
            && !url.trim().is_empty()
        {
            config = config.url(url);
        }
        if let Some(size) = parse_env::<u32>("KV_POOL_SIZE")? {
            config = config.pool_size(size);
        }
        if let Some(capacity) = parse_env::<u64>("KV_CAPACITY")? {
            config = config.capacity(capacity);
        }
        if let Ok(table) = std::env::var("KV_TABLE")
            && !table.trim().is_empty()
        {
            config = config.table(table);
        }

        Ok(config)
    }

    /// Check the configuration without opening anything.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] for an unusable application name, and [`Error::Config`]
    /// for a backend with no URL.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig};
    ///
    /// assert!(KvConfig::new("shop", KvBackend::Memory).validate().is_ok());
    /// // Redis with no URL is a boot failure, not a first-request failure.
    /// assert!(KvConfig::new("shop", KvBackend::Redis).validate().is_err());
    /// assert!(KvConfig::new("Shop", KvBackend::Memory).validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        validate_name("application", &self.app)?;
        if self.backend.needs_url() && self.url.is_none() {
            return Err(Error::Config {
                detail: format!(
                    "the `{}` backend needs a url. help: set `kv.url`, or `KV_URL`",
                    self.backend
                ),
            });
        }
        if self.backend == KvBackend::Postgres {
            validate_name("table", &self.table)?;
        }
        Ok(())
    }

    /// Open the store and build the handle.
    ///
    /// # Errors
    ///
    /// Whatever [`validate`](Self::validate) rejects, plus a connection
    /// failure, plus [`Error::Config`] when the backend's cargo feature is off
    /// — which is a build-time mistake reported at boot, with the feature named.
    ///
    /// ```
    /// use moso_kv::{KvBackend, KvConfig, Result};
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = KvConfig::new("shop", KvBackend::Memory).build().await?;
    /// assert_eq!(kv.store().name(), "memory");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn build(&self) -> Result<Kv> {
        self.validate()?;

        let builder = Kv::builder(self.app.clone()).breaker(self.breaker);

        match self.backend {
            #[cfg(feature = "memory")]
            KvBackend::Memory => builder
                .store(crate::backend::MemoryStore::with_capacity(self.capacity))
                .build(),

            #[cfg(feature = "redis")]
            KvBackend::Redis => {
                let redis = crate::backend::RedisStore::connect(
                    crate::backend::RedisConfig::new(self.expect_url()?)
                        .pool_size(self.pool_size)
                        .connect_timeout(self.connect_timeout),
                )
                .await?;
                builder.store(redis).build()
            }

            #[cfg(feature = "pg-kv")]
            KvBackend::Postgres => {
                let postgres = crate::backend::PostgresStore::connect(
                    self.expect_url()?,
                    &self.table,
                    self.pool_size,
                    self.connect_timeout,
                )
                .await?;
                if self.sweep_interval > Duration::ZERO {
                    // Owned by the store, so it lives as long as the store and
                    // stops when the last handle is dropped.
                    postgres.start_sweeper(self.sweep_interval);
                }
                builder.store(postgres).build()
            }

            #[allow(unreachable_patterns)]
            other => Err(Error::Config {
                detail: format!(
                    "the `{other}` backend is configured but its cargo feature is off. \
                     help: moso-kv = {{ features = [\"{}\"] }}",
                    other.feature()
                ),
            }),
        }
    }

    /// The URL, or the error that names what to set.
    #[cfg_attr(
        not(any(feature = "redis", feature = "pg-kv")),
        expect(dead_code, reason = "only the networked backends read the url")
    )]
    fn expect_url(&self) -> Result<&str> {
        self.url
            .as_ref()
            .map(SecretString::expose)
            .ok_or_else(|| Error::Config {
                detail: format!(
                    "the `{}` backend needs a url. help: set `kv.url`, or `KV_URL`",
                    self.backend
                ),
            })
    }
}

/// Parse an environment variable, or say which one was wrong.
fn parse_env<T: std::str::FromStr>(name: &'static str) -> Result<Option<T>> {
    match std::env::var(name) {
        Err(_) => Ok(None),
        Ok(value) if value.trim().is_empty() => Ok(None),
        Ok(value) => value
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|_| Error::Config {
                detail: format!("`{name}` is set to `{value}`, which is not a number"),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backend_names_round_trip() {
        for backend in [KvBackend::Memory, KvBackend::Redis, KvBackend::Postgres] {
            assert_eq!(
                backend.as_str().parse::<KvBackend>().expect("known"),
                backend
            );
            assert_eq!(backend.to_string(), backend.as_str());
        }
        assert_eq!(KvBackend::default(), KvBackend::Memory);
    }

    #[test]
    fn the_aliases_people_actually_write_are_accepted() {
        assert_eq!(
            "Redis".parse::<KvBackend>().expect("known"),
            KvBackend::Redis
        );
        assert_eq!(
            "valkey".parse::<KvBackend>().expect("known"),
            KvBackend::Redis
        );
        assert_eq!(
            " postgresql ".parse::<KvBackend>().expect("known"),
            KvBackend::Postgres
        );
        assert_eq!(
            "pg".parse::<KvBackend>().expect("known"),
            KvBackend::Postgres
        );
    }

    #[test]
    fn an_unknown_backend_names_the_ones_that_exist() {
        let error = "dynamo".parse::<KvBackend>().expect_err("unknown");
        let message = error.to_string();
        assert!(message.contains("memory"), "{message}");
        assert!(message.contains("redis"), "{message}");
        assert!(message.contains("postgres"), "{message}");
    }

    #[test]
    fn the_defaults_are_the_documented_ones() {
        let config = KvConfig::new("shop", KvBackend::Memory);
        assert_eq!(config.pool_size, 8);
        assert_eq!(config.capacity, 10_000);
        assert_eq!(config.table, "moso_kv");
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert_eq!(config.sweep_interval, Duration::from_secs(30));
        assert!(config.url.is_none());
    }

    #[test]
    fn validation_catches_what_would_otherwise_fail_at_the_first_request() {
        assert!(KvConfig::new("shop", KvBackend::Memory).validate().is_ok());
        assert!(KvConfig::new("Shop", KvBackend::Memory).validate().is_err());
        assert!(KvConfig::new("shop", KvBackend::Redis).validate().is_err());
        assert!(
            KvConfig::new("shop", KvBackend::Redis)
                .url("redis://x")
                .validate()
                .is_ok()
        );
        assert!(
            KvConfig::new("shop", KvBackend::Postgres)
                .url("postgres://x")
                .table("Bad Table")
                .validate()
                .is_err()
        );
    }

    #[test]
    fn a_pool_size_of_zero_is_a_pool_of_one() {
        assert_eq!(
            KvConfig::new("a", KvBackend::Redis).pool_size(0).pool_size,
            1
        );
    }

    #[test]
    fn the_url_is_a_secret_and_does_not_print() {
        let config = KvConfig::new("a", KvBackend::Redis).url("redis://user:hunter2@host");
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("hunter2"), "{rendered}");
    }

    #[cfg(feature = "memory")]
    #[tokio::test]
    async fn building_the_memory_backend_needs_nothing() {
        let kv = KvConfig::new("shop", KvBackend::Memory)
            .capacity(64)
            .build()
            .await
            .expect("built");
        assert_eq!(kv.store().name(), "memory");
        assert_eq!(kv.app(), "shop");
    }

    #[test]
    fn a_bad_number_in_the_environment_names_the_variable() {
        // Deliberately not touching the real environment: `parse_env` is the
        // whole of the behaviour and it is tested through its own contract.
        assert!(
            parse_env::<u32>("MOSO_KV_DEFINITELY_NOT_SET")
                .expect("absent")
                .is_none()
        );
    }
}
