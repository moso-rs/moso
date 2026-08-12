//! The Redis backend, on `fred`.
//!
//! The production standard. `fred` brings pooling, pipelining, cluster and
//! sentinel support, TLS and automatic reconnection; this module maps
//! [`KvStore`] onto it and adds the four things the mapping is not obvious for.
//!
//! # 1. Compare-and-swap is a script, not a `WATCH`
//!
//! `WATCH`/`MULTI`/`EXEC` needs a dedicated connection for the whole
//! transaction, which a pool cannot give without pinning — and an optimistic
//! transaction that aborts has to be retried by the caller. The three
//! compare-and-swap operations are single `EVAL`s instead: one round trip, no
//! pinning, no retry loop, and atomic by construction because Redis runs a
//! script to completion.
//!
//! # 2. `incr` only sets a TTL when there is not one
//!
//! `INCRBY` followed by `EXPIRE` is the usual idiom and it is wrong: two
//! requests arriving together both see a fresh key and both push the window
//! forward, so a "one minute" counter never expires under load. The script
//! checks `PTTL` and only sets an expiry when the key has none, which is what
//! the memory backend does and therefore what a test that passes on one will
//! see on the other.
//!
//! # 3. `SCAN` patterns are glob-escaped
//!
//! A key part may legitimately contain `*`, `?` or `[`. Splicing one into a
//! `MATCH` pattern unescaped turns `delete_prefix` into a wildcard delete of
//! somebody else's keys. [`glob_escape`] is not an optimisation.
//!
//! # 4. A cluster reports `scan: false`
//!
//! `SCAN` on a clustered client walks the keyspace of *one* node, so a
//! `delete_prefix` over a cluster would silently delete some of the keys and
//! report a number. Rather than do that, a clustered [`RedisStore`] reports
//! [`Capabilities::scan`] as `false` and the two operations return
//! [`Error::Unsupported`] naming the reason. Bumping
//! [`Namespace::VERSION`](crate::Namespace::VERSION) is the cluster-safe way to
//! invalidate a namespace, and it is instant.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use fred::clients::SubscriberClient;
use fred::interfaces::EventInterface;
use fred::prelude::{
    Builder, ClientLike, Config, ConnectionConfig, Expiration, KeysInterface, ListInterface,
    LuaInterface, PerformanceConfig, Pool, PubsubInterface, ReconnectPolicy, SetOptions,
    SetsInterface, SortedSetsInterface, Value,
};
use fred::types::scan::ScanType;
use moso_core::{BoxFuture, HealthStatus};

use crate::error::{Error, Result};
use crate::key::Key;
use crate::store::{Capabilities, KvStore, MessageStream, ScanCursor, SetOpts, Side};

/// The name this backend reports.
pub(crate) const BACKEND: &str = "redis";

// ---------------------------------------------------------------------------
// The scripts
// ---------------------------------------------------------------------------

/// `INCRBY`, setting an expiry only when the key does not already have one.
///
/// `ARGV[1]` is the delta, `ARGV[2]` the TTL in milliseconds or `0` for none.
pub const INCR_SCRIPT: &str = r"
local value = redis.call('INCRBY', KEYS[1], ARGV[1])
if ARGV[2] ~= '0' and redis.call('PTTL', KEYS[1]) < 0 then
  redis.call('PEXPIRE', KEYS[1], ARGV[2])
end
return value
";

/// Compare-and-swap.
///
/// `ARGV[1]` is `0` when the key must be absent and `1` when it must equal
/// `ARGV[2]`. `ARGV[3]` is `none`, `keep`, or a TTL in milliseconds.
/// `ARGV[4]` is the new value. Returns `1` when the swap happened.
pub const CAS_SCRIPT: &str = r"
local current = redis.call('GET', KEYS[1])
if ARGV[1] == '0' then
  if current then return 0 end
else
  if not current or current ~= ARGV[2] then return 0 end
end
if ARGV[3] == 'keep' then
  redis.call('SET', KEYS[1], ARGV[4], 'KEEPTTL')
elseif ARGV[3] == 'none' then
  redis.call('SET', KEYS[1], ARGV[4])
else
  redis.call('SET', KEYS[1], ARGV[4], 'PX', ARGV[3])
end
return 1
";

/// Delete only if the value is exactly `ARGV[1]`. Releasing a lock.
pub const CAD_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('DEL', KEYS[1])
end
return 0
";

/// Extend the expiry only if the value is exactly `ARGV[1]`. Renewing a lease.
pub const CAE_SCRIPT: &str = r"
if redis.call('GET', KEYS[1]) == ARGV[1] then
  return redis.call('PEXPIRE', KEYS[1], ARGV[2])
end
return 0
";

// ---------------------------------------------------------------------------
// RedisConfig
// ---------------------------------------------------------------------------

/// How to open a [`RedisStore`].
///
/// ```
/// use moso_kv::backend::RedisConfig;
/// use std::time::Duration;
///
/// let config = RedisConfig::new("redis://localhost:6379")
///     .pool_size(8)
///     .connect_timeout(Duration::from_secs(3));
///
/// assert_eq!(config.pool_size, 8);
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RedisConfig {
    /// The connection URL. `redis://`, `rediss://`, `redis-cluster://` or
    /// `redis-sentinel://`; `fred` decides the topology from the scheme.
    pub url: String,
    /// How many connections to hold.
    pub pool_size: u32,
    /// How long to wait for the first connection.
    pub connect_timeout: Duration,
    /// How long a command may take before it is failed.
    pub command_timeout: Duration,
    /// How many times to retry a command after a reconnection.
    pub max_command_attempts: u32,
}

impl RedisConfig {
    /// A configuration for `url` with the documented defaults.
    ///
    /// ```
    /// use moso_kv::backend::RedisConfig;
    ///
    /// let config = RedisConfig::new("redis://localhost:6379");
    /// assert_eq!(config.pool_size, 8);
    /// assert_eq!(config.max_command_attempts, 3);
    /// ```
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            pool_size: 8,
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(5),
            max_command_attempts: 3,
        }
    }

    /// Set the pool size, which is never zero.
    ///
    /// ```
    /// use moso_kv::backend::RedisConfig;
    ///
    /// assert_eq!(RedisConfig::new("redis://x").pool_size(0).pool_size, 1);
    /// ```
    #[must_use]
    pub fn pool_size(mut self, size: u32) -> Self {
        self.pool_size = size.max(1);
        self
    }

    /// Set the connect timeout.
    ///
    /// ```
    /// use moso_kv::backend::RedisConfig;
    /// use std::time::Duration;
    ///
    /// let config = RedisConfig::new("redis://x").connect_timeout(Duration::from_secs(1));
    /// assert_eq!(config.connect_timeout, Duration::from_secs(1));
    /// ```
    #[must_use]
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = timeout;
        self
    }

    /// Set the per-command timeout.
    ///
    /// A cache read that takes longer than this is worse than a cache miss, so
    /// the default is deliberately short.
    ///
    /// ```
    /// use moso_kv::backend::RedisConfig;
    /// use std::time::Duration;
    ///
    /// let config = RedisConfig::new("redis://x").command_timeout(Duration::from_millis(200));
    /// assert_eq!(config.command_timeout, Duration::from_millis(200));
    /// ```
    #[must_use]
    pub fn command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }

    /// Set how many times a command is retried across a reconnection.
    ///
    /// ```
    /// use moso_kv::backend::RedisConfig;
    ///
    /// assert_eq!(RedisConfig::new("redis://x").max_command_attempts(1).max_command_attempts, 1);
    /// ```
    #[must_use]
    pub fn max_command_attempts(mut self, attempts: u32) -> Self {
        self.max_command_attempts = attempts.max(1);
        self
    }

    /// The `fred` configuration this becomes.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when the URL is not one `fred` understands.
    ///
    /// ```
    /// use moso_kv::backend::RedisConfig;
    ///
    /// assert!(RedisConfig::new("redis://localhost:6379").fred_config().is_ok());
    /// assert!(RedisConfig::new("not a url").fred_config().is_err());
    /// ```
    pub fn fred_config(&self) -> Result<Config> {
        Config::from_url(&self.url).map_err(|error| Error::Config {
            detail: format!(
                "`{}` is not a usable redis url ({error}). help: redis://host:port, \
                 rediss://host:port for TLS, redis-cluster://host:port, \
                 redis-sentinel://host:port",
                redact(&self.url)
            ),
        })
    }
}

/// A URL with its password replaced, for an error message.
///
/// A connection URL in a log is the single most common way a Redis password
/// ends up in a log aggregator.
fn redact(url: &str) -> String {
    match (url.find("://"), url.find('@')) {
        (Some(scheme_end), Some(at)) if at > scheme_end + 3 => {
            format!("{}://***{}", &url[..scheme_end], &url[at..])
        }
        _ => url.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// RedisStore
// ---------------------------------------------------------------------------

/// A [`KvStore`] over Redis or Valkey.
///
/// ```no_run
/// use moso_kv::backend::{RedisConfig, RedisStore};
/// use moso_kv::{Kv, Result};
///
/// # #[tokio::main] async fn main() -> Result<()> {
/// let store = RedisStore::connect(RedisConfig::new("redis://localhost:6379")).await?;
/// let kv = Kv::builder("shop").store(store).build()?;
///
/// assert_eq!(kv.store().name(), "redis");
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct RedisStore {
    inner: Arc<RedisInner>,
}

struct RedisInner {
    pool: Pool,
    config: Config,
    clustered: bool,
    settings: RedisConfig,
}

impl std::fmt::Debug for RedisStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RedisStore")
            .field("url", &redact(&self.inner.settings.url))
            .field("pool_size", &self.inner.settings.pool_size)
            .field("clustered", &self.inner.clustered)
            .finish_non_exhaustive()
    }
}

impl RedisStore {
    /// Connect, and wait for the first connection to come up.
    ///
    /// Connecting eagerly is deliberate: a bad URL or an unreachable server is
    /// a boot failure with a message, not a 503 on the first request that
    /// happens to touch the cache.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] for an unusable URL, and [`Error::Backend`] when the
    /// server cannot be reached within
    /// [`connect_timeout`](RedisConfig::connect_timeout).
    ///
    /// ```no_run
    /// use moso_kv::backend::{RedisConfig, RedisStore};
    ///
    /// # #[tokio::main] async fn main() {
    /// let store = RedisStore::connect(RedisConfig::new("redis://localhost:6379"))
    ///     .await
    ///     .expect("connected");
    /// # let _ = store;
    /// # }
    /// ```
    pub async fn connect(settings: RedisConfig) -> Result<Self> {
        let config = settings.fred_config()?;
        let clustered = config.server.is_clustered();

        let pool = Builder::from_config(config.clone())
            .set_connection_config(ConnectionConfig {
                connection_timeout: settings.connect_timeout,
                internal_command_timeout: settings.command_timeout,
                max_command_attempts: settings.max_command_attempts,
                ..ConnectionConfig::default()
            })
            .set_performance_config(PerformanceConfig {
                default_command_timeout: settings.command_timeout,
                ..PerformanceConfig::default()
            })
            .set_policy(ReconnectPolicy::new_exponential(0, 100, 30_000, 2))
            .build_pool(usize::try_from(settings.pool_size).unwrap_or(1))
            .map_err(|error| Error::Config {
                detail: format!("the redis pool could not be built: {error}"),
            })?;

        tokio::time::timeout(settings.connect_timeout, pool.init())
            .await
            .map_err(|_| {
                Error::backend(
                    BACKEND,
                    "connect",
                    format!(
                        "no connection within {}",
                        humantime::format_duration(settings.connect_timeout)
                    ),
                )
            })?
            .map_err(|error| Error::backend(BACKEND, "connect", error))?;

        Ok(Self {
            inner: Arc::new(RedisInner {
                pool,
                config,
                clustered,
                settings,
            }),
        })
    }

    /// Whether this store is talking to a Redis Cluster.
    ///
    /// ```no_run
    /// # use moso_kv::backend::{RedisConfig, RedisStore};
    /// # #[tokio::main] async fn main() {
    /// let store = RedisStore::connect(RedisConfig::new("redis://localhost:6379"))
    ///     .await
    ///     .expect("connected");
    /// assert!(!store.is_clustered());
    /// # }
    /// ```
    #[must_use]
    pub fn is_clustered(&self) -> bool {
        self.inner.clustered
    }

    /// The `fred` pool, for the commands [`KvStore`] does not expose.
    ///
    /// The documented escape hatch. Using it makes `fred`'s major version part
    /// of your application's contract, which is the trade.
    ///
    /// ```no_run
    /// # use moso_kv::backend::{RedisConfig, RedisStore};
    /// # #[tokio::main] async fn main() {
    /// let store = RedisStore::connect(RedisConfig::new("redis://localhost:6379"))
    ///     .await
    ///     .expect("connected");
    /// let pool = store.pool();
    /// # let _ = pool;
    /// # }
    /// ```
    #[must_use]
    pub fn pool(&self) -> &Pool {
        &self.inner.pool
    }

    /// Close every connection.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when the server refuses the `QUIT`.
    ///
    /// ```no_run
    /// # use moso_kv::backend::{RedisConfig, RedisStore};
    /// # #[tokio::main] async fn main() {
    /// let store = RedisStore::connect(RedisConfig::new("redis://localhost:6379"))
    ///     .await
    ///     .expect("connected");
    /// store.close().await.expect("closed");
    /// # }
    /// ```
    pub async fn close(&self) -> Result<()> {
        self.inner
            .pool
            .quit()
            .await
            .map_err(|error| Error::backend(BACKEND, "close", error))
    }

    /// One `EVAL`, returning an integer.
    async fn eval_int(&self, script: &str, key: &Key, args: Vec<Value>) -> Result<i64> {
        self.inner
            .pool
            .eval::<i64, _, _, _>(script, vec![key.as_str()], args)
            .await
            .map_err(|error| Error::backend(BACKEND, "eval", error))
    }
}

/// The TTL argument the scripts take: `none`, `keep`, or milliseconds.
fn ttl_argument(opts: SetOpts) -> Value {
    if opts.keep_ttl {
        return Value::from_static_str("keep");
    }
    match opts.ttl {
        None => Value::from_static_str("none"),
        Some(ttl) => Value::String(millis(ttl).to_string().into()),
    }
}

/// A `Duration` in milliseconds, never zero — Redis rejects a zero `PX`.
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis())
        .unwrap_or(i64::MAX)
        .max(1)
}

/// Escape the glob metacharacters Redis' `MATCH` understands.
///
/// A key part may contain `*`, `?` or `[`, and splicing one into a pattern
/// unescaped turns a prefix scan into a wildcard scan over other namespaces.
///
/// ```
/// use moso_kv::backend::redis::glob_escape;
///
/// assert_eq!(glob_escape("moso:v1:a:b:1:"), "moso:v1:a:b:1:");
/// assert_eq!(glob_escape("a*b"), "a\\*b");
/// assert_eq!(glob_escape("a?b[c]"), "a\\?b\\[c\\]");
/// assert_eq!(glob_escape("a\\b"), "a\\\\b");
/// ```
#[must_use]
pub fn glob_escape(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len() + 8);
    for ch in pattern.chars() {
        if matches!(ch, '\\' | '*' | '?' | '[' | ']' | '^') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// The error a command produces.
fn failed(operation: &'static str) -> impl FnOnce(fred::error::Error) -> Error {
    move |error| Error::backend(BACKEND, operation, error)
}

impl KvStore for RedisStore {
    fn name(&self) -> &'static str {
        BACKEND
    }

    fn capabilities(&self) -> Capabilities {
        // `SCAN` on a cluster walks one node, so claiming `scan` there would
        // make `delete_prefix` silently partial. See the module documentation.
        Capabilities::redis().with_scan(!self.inner.clustered)
    }

    fn health(&self) -> BoxFuture<'_, HealthStatus> {
        Box::pin(async move {
            match self.inner.pool.ping::<String>(None).await {
                Ok(_) => HealthStatus::Up,
                Err(error) => HealthStatus::Down(error.to_string()),
            }
        })
    }

    fn get<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            self.inner
                .pool
                .get::<Option<Bytes>, _>(key.as_str())
                .await
                .map_err(failed("get"))
        })
    }

    fn set<'a>(&'a self, key: &'a Key, value: Bytes, opts: SetOpts) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            opts.validate()?;

            let expiration = if opts.keep_ttl {
                Some(Expiration::KEEPTTL)
            } else {
                opts.ttl.map(|ttl| Expiration::PX(millis(ttl)))
            };
            let condition = match (opts.if_absent, opts.if_present) {
                (true, _) => Some(SetOptions::NX),
                (_, true) => Some(SetOptions::XX),
                _ => None,
            };

            let reply: Value = self
                .inner
                .pool
                .set(
                    key.as_str(),
                    Value::Bytes(value),
                    expiration,
                    condition,
                    false,
                )
                .await
                .map_err(failed("set"))?;

            // A declined `NX`/`XX` is a nil reply, not an error.
            Ok(!reply.is_null())
        })
    }

    fn delete<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let removed: i64 = self
                .inner
                .pool
                .del(key.as_str())
                .await
                .map_err(failed("delete"))?;
            Ok(removed > 0)
        })
    }

    fn exists<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let found: i64 = self
                .inner
                .pool
                .exists(key.as_str())
                .await
                .map_err(failed("exists"))?;
            Ok(found > 0)
        })
    }

    fn expire<'a>(&'a self, key: &'a Key, ttl: Duration) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let applied: i64 = self
                .inner
                .pool
                .pexpire(key.as_str(), millis(ttl), None)
                .await
                .map_err(failed("expire"))?;
            Ok(applied == 1)
        })
    }

    fn ttl<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move {
            // `-2` is "no such key", `-1` is "no expiry". Neither is a
            // duration, and both are `None` by the trait's contract.
            let remaining: i64 = self
                .inner
                .pool
                .pttl(key.as_str())
                .await
                .map_err(failed("ttl"))?;
            Ok(if remaining < 0 {
                None
            } else {
                Some(Duration::from_millis(remaining.unsigned_abs()))
            })
        })
    }

    fn incr<'a>(
        &'a self,
        key: &'a Key,
        by: i64,
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let ttl_ms = ttl.map_or(0, millis);
            self.eval_int(
                INCR_SCRIPT,
                key,
                vec![
                    Value::String(by.to_string().into()),
                    Value::String(ttl_ms.to_string().into()),
                ],
            )
            .await
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: &'a Key,
        old: Option<&'a [u8]>,
        new: Bytes,
        opts: SetOpts,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            opts.validate()?;
            let (flag, expected) = match old {
                None => (Value::from_static_str("0"), Value::Bytes(Bytes::new())),
                Some(bytes) => (
                    Value::from_static_str("1"),
                    Value::Bytes(Bytes::copy_from_slice(bytes)),
                ),
            };
            let swapped = self
                .eval_int(
                    CAS_SCRIPT,
                    key,
                    vec![flag, expected, ttl_argument(opts), Value::Bytes(new)],
                )
                .await?;
            Ok(swapped == 1)
        })
    }

    fn compare_and_delete<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let removed = self
                .eval_int(
                    CAD_SCRIPT,
                    key,
                    vec![Value::Bytes(Bytes::copy_from_slice(expected))],
                )
                .await?;
            Ok(removed == 1)
        })
    }

    fn compare_and_expire<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
        ttl: Duration,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let renewed = self
                .eval_int(
                    CAE_SCRIPT,
                    key,
                    vec![
                        Value::Bytes(Bytes::copy_from_slice(expected)),
                        Value::String(millis(ttl).to_string().into()),
                    ],
                )
                .await?;
            Ok(renewed == 1)
        })
    }

    fn get_many<'a>(&'a self, keys: &'a [Key]) -> BoxFuture<'a, Result<Vec<Option<Bytes>>>> {
        Box::pin(async move {
            // Concurrent single `GET`s rather than one `MGET`: `MGET` is a
            // `CROSSSLOT` error the moment the keys land on two cluster nodes,
            // and these keys deliberately have no hash tag.
            futures_util::future::try_join_all(keys.iter().map(|key| self.get(key))).await
        })
    }

    fn set_many<'a>(
        &'a self,
        items: &'a [(Key, Bytes)],
        opts: SetOpts,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            futures_util::future::try_join_all(
                items
                    .iter()
                    .map(|(key, value)| self.set(key, value.clone(), opts)),
            )
            .await?;
            Ok(())
        })
    }

    fn scan<'a>(
        &'a self,
        prefix: &'a Key,
        cursor: ScanCursor,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Key>, ScanCursor)>> {
        Box::pin(async move {
            if self.inner.clustered {
                return Err(Error::unsupported(BACKEND, "scan", "scan"));
            }

            let pattern = format!("{}*", glob_escape(prefix.as_str()));
            let start = cursor.bookmark().unwrap_or("0");

            let reply: Value = self
                .inner
                .pool
                .scan_page(start, pattern, Some(limit.max(1)), None::<ScanType>)
                .await
                .map_err(failed("scan"))?;

            let Value::Array(page) = reply else {
                return Err(Error::backend(
                    BACKEND,
                    "scan",
                    "SCAN did not reply with a cursor and a page",
                ));
            };
            let [next, found] = <[Value; 2]>::try_from(page).map_err(|page| {
                Error::backend(
                    BACKEND,
                    "scan",
                    format!("SCAN replied with {} elements, not 2", page.len()),
                )
            })?;

            let next = next
                .as_str()
                .map_or_else(|| String::from("0"), |text| text.into_owned());
            let Value::Array(found) = found else {
                return Err(Error::backend(
                    BACKEND,
                    "scan",
                    "SCAN did not reply with keys",
                ));
            };

            let mut keys = Vec::with_capacity(found.len());
            for value in found {
                let text = value.as_str().ok_or_else(|| {
                    Error::backend(BACKEND, "scan", "SCAN replied with a non-string key")
                })?;
                keys.push(Key::from_raw(text.into_owned())?);
            }

            // Redis signals the end of a scan with a `0` cursor, and may return
            // an empty page before then — which is why the trait's contract
            // says the cursor, not the page, decides when a scan is over.
            let cursor = if next == "0" {
                ScanCursor::end()
            } else {
                ScanCursor::at(next)
            };
            Ok((keys, cursor))
        })
    }

    fn list_push<'a>(
        &'a self,
        key: &'a Key,
        values: &'a [Bytes],
        side: Side,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if values.is_empty() {
                return self.list_len(key).await;
            }
            let elements: Vec<Value> = values.iter().cloned().map(Value::Bytes).collect();
            let length: i64 = match side {
                Side::Left => self.inner.pool.lpush(key.as_str(), elements).await,
                Side::Right => self.inner.pool.rpush(key.as_str(), elements).await,
            }
            .map_err(failed("list_push"))?;
            Ok(length.unsigned_abs())
        })
    }

    fn list_pop<'a>(
        &'a self,
        key: &'a Key,
        side: Side,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            match timeout {
                None => {
                    let popped: Option<Bytes> = match side {
                        Side::Left => self.inner.pool.lpop(key.as_str(), None).await,
                        Side::Right => self.inner.pool.rpop(key.as_str(), None).await,
                    }
                    .map_err(failed("list_pop"))?;
                    Ok(popped)
                }
                Some(timeout) => {
                    // `BLPOP` replies with `[key, value]`, or nil when the
                    // blocking timeout elapses — and `fred` turns that nil into
                    // an `ErrorKind::Timeout`. "Nothing arrived in time" is the
                    // documented `Ok(None)` of this method, not a failure, so
                    // that one error kind is translated back.
                    let seconds = timeout.as_secs_f64();
                    let reply: Value = match side {
                        Side::Left => self.inner.pool.blpop(key.as_str(), seconds).await,
                        Side::Right => self.inner.pool.brpop(key.as_str(), seconds).await,
                    }
                    .or_else(|error| {
                        if *error.kind() == fred::error::ErrorKind::Timeout {
                            Ok(Value::Null)
                        } else {
                            Err(error)
                        }
                    })
                    .map_err(failed("list_pop"))?;

                    Ok(match reply {
                        Value::Null => None,
                        Value::Array(pair) => pair.into_iter().nth(1).and_then(Value::into_bytes),
                        other => other.into_bytes(),
                    })
                }
            }
        })
    }

    fn list_len<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let length: i64 = self
                .inner
                .pool
                .llen(key.as_str())
                .await
                .map_err(failed("list_len"))?;
            Ok(length.unsigned_abs())
        })
    }

    fn set_add<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if members.is_empty() {
                return Ok(0);
            }
            let elements: Vec<Value> = members.iter().cloned().map(Value::Bytes).collect();
            let added: i64 = self
                .inner
                .pool
                .sadd(key.as_str(), elements)
                .await
                .map_err(failed("set_add"))?;
            Ok(added.unsigned_abs())
        })
    }

    fn set_remove<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if members.is_empty() {
                return Ok(0);
            }
            let elements: Vec<Value> = members.iter().cloned().map(Value::Bytes).collect();
            let removed: i64 = self
                .inner
                .pool
                .srem(key.as_str(), elements)
                .await
                .map_err(failed("set_remove"))?;
            Ok(removed.unsigned_abs())
        })
    }

    fn set_members<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async move {
            let members: Vec<Bytes> = self
                .inner
                .pool
                .smembers(key.as_str())
                .await
                .map_err(failed("set_members"))?;
            Ok(members)
        })
    }

    fn zadd<'a>(&'a self, key: &'a Key, scored: &'a [(f64, Bytes)]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if scored.is_empty() {
                return Ok(0);
            }
            let values: Vec<(f64, Value)> = scored
                .iter()
                .map(|(score, member)| (*score, Value::Bytes(member.clone())))
                .collect();
            // `changed = false`, so the reply counts *added* members and not
            // members whose score moved — which is what the trait promises.
            let added: i64 = self
                .inner
                .pool
                .zadd(key.as_str(), None, None, false, false, values)
                .await
                .map_err(failed("zadd"))?;
            Ok(added.unsigned_abs())
        })
    }

    fn zrange_by_score<'a>(
        &'a self,
        key: &'a Key,
        lo: f64,
        hi: f64,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async move {
            let members: Vec<Bytes> = self
                .inner
                .pool
                .zrangebyscore(key.as_str(), lo, hi, false, Some((0, i64::from(limit))))
                .await
                .map_err(failed("zrange_by_score"))?;
            Ok(members)
        })
    }

    fn zrem<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if members.is_empty() {
                return Ok(0);
            }
            let elements: Vec<Value> = members.iter().cloned().map(Value::Bytes).collect();
            let removed: i64 = self
                .inner
                .pool
                .zrem(key.as_str(), elements)
                .await
                .map_err(failed("zrem"))?;
            Ok(removed.unsigned_abs())
        })
    }

    fn publish<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            // `PubsubInterface` is implemented for `Client` and not for
            // `Pool`, because a subscription is a property of one connection.
            // `PUBLISH` is not, so it goes to whichever connection is next.
            let reached: i64 = self
                .inner
                .pool
                .next()
                .publish(channel, Value::Bytes(payload))
                .await
                .map_err(failed("publish"))?;
            Ok(reached.unsigned_abs())
        })
    }

    fn subscribe<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        Box::pin(async move {
            // A dedicated connection: a Redis connection in subscriber mode
            // accepts no other commands, so borrowing one from the command pool
            // would take it out of service for as long as the subscription
            // lives. `SubscriberClient` also re-subscribes after a reconnect,
            // which a raw `SUBSCRIBE` does not.
            let subscriber = SubscriberClient::new(self.inner.config.clone(), None, None, None);
            subscriber.init().await.map_err(failed("subscribe"))?;
            subscriber
                .subscribe(channel)
                .await
                .map_err(failed("subscribe"))?;
            let manager = subscriber.manage_subscriptions();

            let receiver = subscriber.message_rx();
            let wanted = channel.to_owned();

            // The client and the reconnection manager are moved into the
            // stream's state, so dropping the stream closes the connection —
            // and holding the stream keeps it alive.
            let stream = futures_util::stream::unfold(
                (receiver, subscriber, manager, wanted),
                |(mut receiver, subscriber, manager, wanted)| async move {
                    loop {
                        match receiver.recv().await {
                            Ok(message) if message.channel == wanted => {
                                let payload = message.value.into_bytes().unwrap_or_default();
                                return Some((payload, (receiver, subscriber, manager, wanted)));
                            }
                            // Another channel on the same client, or a lagging
                            // subscriber: keep waiting rather than ending.
                            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                },
            );

            Ok(Box::pin(stream) as MessageStream)
        })
    }

    fn eval<'a>(
        &'a self,
        script: &'a str,
        keys: &'a [Key],
        args: &'a [Bytes],
    ) -> BoxFuture<'a, Result<Vec<i64>>> {
        Box::pin(async move {
            let key_strings: Vec<&str> = keys.iter().map(Key::as_str).collect();
            let arguments: Vec<Value> = args.iter().cloned().map(Value::Bytes).collect();

            let reply: Value = self
                .inner
                .pool
                .eval(script, key_strings, arguments)
                .await
                .map_err(failed("eval"))?;

            Ok(match reply {
                Value::Integer(value) => vec![value],
                Value::Array(values) => values
                    .into_iter()
                    .map(|value| value.as_i64().unwrap_or(0))
                    .collect(),
                Value::Null => Vec::new(),
                other => {
                    return Err(Error::backend(
                        BACKEND,
                        "eval",
                        format!("the script replied with {other:?}, not integers"),
                    ));
                }
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_url_is_redacted_before_it_reaches_a_message() {
        assert_eq!(
            redact("redis://user:hunter2@example.test:6379"),
            "redis://***@example.test:6379"
        );
        assert_eq!(
            redact("redis://example.test:6379"),
            "redis://example.test:6379"
        );
        assert_eq!(redact("nonsense"), "nonsense");
    }

    #[test]
    fn a_bad_url_says_what_a_good_one_looks_like() {
        let error = RedisConfig::new("not a url")
            .fred_config()
            .expect_err("rejected");
        let message = error.to_string();
        assert!(message.contains("redis://"), "{message}");
        assert!(message.contains("rediss://"), "{message}");
    }

    #[test]
    fn a_password_never_reaches_an_error_message() {
        let error = RedisConfig::new("redis://user:hunter2@")
            .fred_config()
            .expect_err("rejected");
        assert!(!error.to_string().contains("hunter2"), "{error}");
    }

    #[test]
    fn glob_metacharacters_are_escaped() {
        assert_eq!(
            glob_escape("moso:v1:shop:profile:1:"),
            "moso:v1:shop:profile:1:"
        );
        assert_eq!(glob_escape("*"), "\\*");
        assert_eq!(glob_escape("a?b"), "a\\?b");
        assert_eq!(glob_escape("[a-z]"), "\\[a-z\\]");
        assert_eq!(glob_escape("a\\cb"), "a\\\\cb");
        assert_eq!(glob_escape("^"), "\\^");
    }

    #[test]
    fn millis_never_produces_a_zero_px() {
        assert_eq!(millis(Duration::ZERO), 1);
        assert_eq!(millis(Duration::from_millis(1)), 1);
        assert_eq!(millis(Duration::from_secs(2)), 2_000);
        assert_eq!(millis(Duration::MAX), i64::MAX);
    }

    #[test]
    fn the_ttl_argument_says_which_of_the_three_cases_it_is() {
        assert_eq!(
            ttl_argument(SetOpts::new()).as_str().as_deref(),
            Some("none")
        );
        assert_eq!(
            ttl_argument(SetOpts::new().keep_ttl()).as_str().as_deref(),
            Some("keep")
        );
        assert_eq!(
            ttl_argument(SetOpts::new().ttl(Duration::from_secs(2)))
                .as_str()
                .as_deref(),
            Some("2000")
        );
    }

    #[test]
    fn the_scripts_are_the_documented_shape() {
        // The scripts are the atomicity, so their contracts are asserted here
        // and exercised for real in `tests/conformance.rs` when `REDIS_URL` is
        // set.
        assert!(INCR_SCRIPT.contains("INCRBY"));
        assert!(INCR_SCRIPT.contains("PTTL"), "the ttl must be conditional");
        assert!(CAS_SCRIPT.contains("KEEPTTL"));
        assert!(CAS_SCRIPT.contains("ARGV[4]"));
        assert!(CAD_SCRIPT.contains("DEL"));
        assert!(CAE_SCRIPT.contains("PEXPIRE"));
    }

    #[test]
    fn the_configuration_defaults_are_the_documented_ones() {
        let config = RedisConfig::new("redis://localhost:6379");
        assert_eq!(config.pool_size, 8);
        assert_eq!(config.max_command_attempts, 3);
        assert_eq!(config.connect_timeout, Duration::from_secs(5));
        assert_eq!(config.clone().pool_size(0).pool_size, 1);
        assert_eq!(config.max_command_attempts(0).max_command_attempts, 1);
    }
}
