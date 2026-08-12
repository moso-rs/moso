//! [`Kv`] — the handle handler code holds.
//!
//! Everything above [`KvStore`] lives here: typed reads and writes through a
//! [`Namespace`], the degrade-or-fail policy, the circuit breaker,
//! single-flight caching, stale-while-revalidate, and the counters that say
//! how any of it is going.
//!
//! # The read path, in order
//!
//! 1. **The breaker.** Open? Then this is a failure without a round trip.
//! 2. **The store.** One `get`.
//! 3. **The envelope.** A framed value is unwrapped; its age comes out with it.
//! 4. **The codec.** Bytes become `N::Value`.
//! 5. **The policy.** A transient failure either degrades to a miss or
//!    propagates, per [`Namespace::FAILURE_MODE`].
//!
//! # Two deliberate asymmetries
//!
//! **A decode failure on read is a miss, always — even under `Fail`.** A
//! rolling deploy that changes a cached type's shape *will* read bytes written
//! by the other version, and 500-ing every request until the old pods drain is
//! strictly worse than recomputing. It logs at `warn` with the namespace's
//! name, so it is not silent, and
//! [`Namespace::VERSION`] is the fix.
//!
//! **A decode failure on write propagates.** Serialising the value you are
//! holding cannot fail because of anything in the world; it fails because the
//! value is not serialisable, which is a bug in the program.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use moso_core::HealthStatus;

use crate::breaker::{Breaker, BreakerConfig};
use crate::codec::{Codec, Encodable, Envelope, Framed};
use crate::error::{Chain, Error, Result};
use crate::flight::SingleFlight;
use crate::key::{Key, KeyBuf, KeyPart as _, validate_name};
use crate::namespace::{FailureMode, Namespace};
use crate::store::{Capabilities, KvStore, MessageStream, ScanCursor, SetOpts};

// ---------------------------------------------------------------------------
// CachedValue
// ---------------------------------------------------------------------------

/// A value read out of the cache, with what the envelope said about it.
///
/// ```
/// use moso_kv::CachedValue;
/// use std::time::Duration;
///
/// let entry = CachedValue { value: 7_u32, age: Duration::from_secs(2), negative: false };
/// assert!(entry.is_stale(Duration::from_secs(1)));
/// assert!(!entry.is_stale(Duration::from_secs(5)));
/// assert_eq!(entry.into_value(), 7);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedValue<T> {
    /// What was stored.
    pub value: T,
    /// How long ago it was written. Always [`Duration::ZERO`] for an unframed
    /// codec, which stores no timestamp.
    pub age: Duration,
    /// Whether it is a cached absence.
    pub negative: bool,
}

impl<T> CachedValue<T> {
    /// Whether this value is older than `fresh_for`.
    ///
    /// ```
    /// use moso_kv::CachedValue;
    /// use std::time::Duration;
    ///
    /// let entry = CachedValue { value: (), age: Duration::from_secs(10), negative: false };
    /// assert!(entry.is_stale(Duration::from_secs(5)));
    /// ```
    #[must_use]
    pub fn is_stale(&self, fresh_for: Duration) -> bool {
        self.age > fresh_for
    }

    /// Throw away the metadata.
    ///
    /// ```
    /// use moso_kv::CachedValue;
    /// use std::time::Duration;
    ///
    /// let entry = CachedValue { value: "v", age: Duration::ZERO, negative: false };
    /// assert_eq!(entry.into_value(), "v");
    /// ```
    #[must_use]
    pub fn into_value(self) -> T {
        self.value
    }
}

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

/// The counters, as a snapshot.
///
/// Named after what an operator asks: "is the cache working?" is
/// `hits / (hits + misses)`, and "is it hiding an outage?" is `degraded`.
///
/// ```
/// use moso_kv::KvStats;
///
/// let stats = KvStats { hits: 90, misses: 10, ..KvStats::default() };
/// assert_eq!(stats.hit_ratio(), 0.9);
/// assert_eq!(KvStats::default().hit_ratio(), 0.0);
/// ```
// Deliberately *not* `#[non_exhaustive]`: this is a snapshot of counters, and
// the thing people do with it is write `KvStats { hits: 90, misses: 10,
// ..Default::default() }` in an assertion. Sealing it against construction
// would buy the freedom to add a counter without a minor bump, at the cost of
// every test that wants to say what it expects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KvStats {
    /// Reads that found a value.
    pub hits: u64,
    /// Reads that found nothing.
    pub misses: u64,
    /// Writes that applied.
    pub writes: u64,
    /// Operations that failed and were **not** degraded away.
    pub errors: u64,
    /// Operations that failed and were degraded to a miss or a no-op.
    ///
    /// This is the number that matters: a healthy service with a rising
    /// `degraded` is one whose cache has silently stopped working.
    pub degraded: u64,
    /// Values that were in the store and did not decode.
    pub decode_failures: u64,
    /// Computations a caller waited for rather than running themselves.
    pub flights_shared: u64,
    /// Background revalidations started by [`Kv::get_swr`].
    pub revalidations: u64,
}

impl KvStats {
    /// `hits / (hits + misses)`, or `0.0` when nothing has been read.
    ///
    /// ```
    /// use moso_kv::KvStats;
    ///
    /// assert_eq!(KvStats { hits: 1, misses: 3, ..KvStats::default() }.hit_ratio(), 0.25);
    /// ```
    #[must_use]
    pub fn hit_ratio(&self) -> f64 {
        let total = self.hits + self.misses;
        if total == 0 {
            return 0.0;
        }
        // Both fit in an f64 exactly up to 2^53, which no counter will reach.
        (self.hits as f64) / (total as f64)
    }
}

/// The live counters.
#[derive(Debug, Default)]
struct Counters {
    hits: AtomicU64,
    misses: AtomicU64,
    writes: AtomicU64,
    errors: AtomicU64,
    degraded: AtomicU64,
    decode_failures: AtomicU64,
    revalidations: AtomicU64,
}

impl Counters {
    fn snapshot(&self) -> KvStats {
        KvStats {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            writes: self.writes.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            degraded: self.degraded.load(Ordering::Relaxed),
            decode_failures: self.decode_failures.load(Ordering::Relaxed),
            // Owned by the single-flight map, which is the only thing that can
            // see a caller join a computation somebody else started.
            flights_shared: 0,
            revalidations: self.revalidations.load(Ordering::Relaxed),
        }
    }
}

// ---------------------------------------------------------------------------
// Metric names
// ---------------------------------------------------------------------------

/// The counter incremented once for every KV operation that runs through the
/// failure policy — the reads, writes, deletes and atomics on [`Kv`].
///
/// A `&'static str`, because [`counter`](moso_core::middleware::metrics::counter)
/// takes a bounded name and nothing per-request: this series says *how much* the
/// cache is doing, never *what key*. It is the denominator an operator divides
/// [`KV_ERRORS_METRIC`] by to read an error rate.
///
/// ```
/// assert_eq!(moso_kv::kv::KV_OPERATIONS_METRIC, "moso_kv_operations_total");
/// ```
pub const KV_OPERATIONS_METRIC: &str = "moso_kv_operations_total";

/// The counter incremented when a backend operation fails — an unreachable
/// store or an open circuit, whether the namespace degrades the failure to a
/// miss or propagates it.
///
/// Named by `docs/04-devex/44-observability.md`. It carries no key and no value:
/// a cached value may be a session or a secret, and a key names the subject, so
/// the series records only that the backend failed. A programmer error — a
/// decode failure, an unsupported operation — is **not** counted here, because
/// it is a bug in the caller and not a fault in the store.
///
/// ```
/// assert_eq!(moso_kv::kv::KV_ERRORS_METRIC, "moso_kv_errors_total");
/// ```
pub const KV_ERRORS_METRIC: &str = "moso_kv_errors_total";

// ---------------------------------------------------------------------------
// Kv
// ---------------------------------------------------------------------------

/// The typed handle over a [`KvStore`].
///
/// Cheap to clone — it is one `Arc` — and registered once in the composition
/// root, then reached from a handler with `Inject<Kv>` or `Depends<Kv>`.
///
/// ```
/// use moso_kv::{minutes, Kv, Result};
///
/// moso_kv::namespace! {
///     /// Cached display names.
///     pub Names: u64 => Option<String>, ttl = minutes(5);
/// }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
/// let kv = Kv::in_memory("shop")?;
///
/// assert_eq!(kv.get::<Names>(&1).await?, None);
/// kv.set::<Names>(&1, &Some("alice".to_owned())).await?;
/// assert_eq!(kv.get::<Names>(&1).await?, Some(Some("alice".to_owned())));
///
/// // The key is namespaced, versioned and escaped.
/// assert_eq!(kv.key::<Names>(&1)?.as_str(), "moso:v1:shop:names:1:1");
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct Kv {
    inner: Arc<Inner>,
}

struct Inner {
    store: Arc<dyn KvStore>,
    app: String,
    breaker: Breaker,
    flight: SingleFlight,
    counters: Counters,
}

impl std::fmt::Debug for Kv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kv")
            .field("app", &self.inner.app)
            .field("backend", &self.inner.store.name())
            .field("breaker", &self.inner.breaker.state())
            .finish_non_exhaustive()
    }
}

impl Kv {
    /// Start building a handle for the application named `app`.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::builder("shop").store(MemoryStore::new()).build().expect("built");
    /// assert_eq!(kv.app(), "shop");
    /// ```
    #[must_use]
    pub fn builder(app: impl Into<String>) -> KvBuilder {
        KvBuilder {
            app: app.into(),
            store: None,
            breaker: BreakerConfig::default(),
        }
    }

    /// A handle over an in-process store — the one tests use.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when `app` is not a legal name.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// assert!(Kv::in_memory("shop").is_ok());
    /// assert!(Kv::in_memory("Shop!").is_err());
    /// ```
    #[cfg(feature = "memory")]
    #[cfg_attr(docsrs, doc(cfg(feature = "memory")))]
    pub fn in_memory(app: impl Into<String>) -> Result<Self> {
        Self::builder(app)
            .store(crate::backend::MemoryStore::new())
            // Nothing to break: the store is a map in this process.
            .breaker(BreakerConfig::never())
            .build()
    }

    /// The application name every key carries.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// assert_eq!(Kv::in_memory("shop").expect("built").app(), "shop");
    /// ```
    #[must_use]
    pub fn app(&self) -> &str {
        &self.inner.app
    }

    /// The backend, for the operations [`Kv`] does not wrap.
    ///
    /// The documented escape hatch: pubsub over a raw channel name, a `SCAN`
    /// over somebody else's prefix, a structure operation a battery needs. It
    /// bypasses the failure policy and the circuit breaker, and that is the
    /// trade.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.store().name(), "memory");
    /// ```
    #[must_use]
    pub fn store(&self) -> &Arc<dyn KvStore> {
        &self.inner.store
    }

    /// What the backend can do.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert!(kv.capabilities().atomic_cas);
    /// assert!(!kv.capabilities().pubsub_cross_process);
    /// ```
    #[must_use]
    pub fn capabilities(&self) -> Capabilities {
        self.inner.store.capabilities()
    }

    /// The circuit breaker, for a test or an operator endpoint.
    ///
    /// ```
    /// use moso_kv::breaker::BreakerState;
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.breaker().state(), BreakerState::Closed);
    /// ```
    #[must_use]
    pub fn breaker(&self) -> &Breaker {
        &self.inner.breaker
    }

    /// The counters.
    ///
    /// ```
    /// use moso_kv::{minutes, Kv};
    ///
    /// moso_kv::namespace! {
    ///     /// A tiny namespace for the example.
    ///     pub Demo: u8 => u8, ttl = minutes(1);
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let kv = Kv::in_memory("shop").expect("built");
    /// let _ = kv.get::<Demo>(&1).await;
    /// assert_eq!(kv.stats().misses, 1);
    /// # }
    /// ```
    #[must_use]
    pub fn stats(&self) -> KvStats {
        KvStats {
            flights_shared: self.inner.flight.shared_total(),
            ..self.inner.counters.snapshot()
        }
    }

    /// A readiness probe over this handle's backend.
    ///
    /// ```
    /// use moso_core::HealthCheck as _;
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// // Not critical by default: a cache that degrades is one the instance
    /// // survives without.
    /// assert!(!kv.health_check().critical());
    /// ```
    #[must_use]
    pub fn health_check(&self) -> crate::health::KvHealthCheck {
        crate::health::KvHealthCheck::new(self.clone())
    }

    // ── keys ──────────────────────────────────────────────────────────────

    /// The key `N` gives `key`.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when the finished key is over
    /// [`MAX_KEY_LEN`](crate::key::MAX_KEY_LEN).
    ///
    /// ```
    /// use moso_kv::{Kv, Namespace};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String, version = 2;
    /// }
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.key::<Profile>(&7).expect("short").as_str(), "moso:v1:shop:profile:2:7");
    /// ```
    pub fn key<N: Namespace>(&self, key: &N::Key) -> Result<Key> {
        let mut buf = KeyBuf::new(&self.inner.app, N::PREFIX, N::VERSION)?;
        key.write_key_part(&mut buf);
        Ok(buf.finish()?)
    }

    /// The prefix every key of `N` starts with.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when the names are not usable, which
    /// [`namespace!`](crate::namespace!) already checks at compile time.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(
    ///     kv.namespace_prefix::<Profile>().expect("short").as_str(),
    ///     "moso:v1:shop:profile:1:",
    /// );
    /// ```
    pub fn namespace_prefix<N: Namespace>(&self) -> Result<Key> {
        Ok(KeyBuf::new(&self.inner.app, N::PREFIX, N::VERSION)?.finish_prefix()?)
    }

    // ── typed reads and writes ────────────────────────────────────────────

    /// Read a value.
    ///
    /// # Errors
    ///
    /// Under [`FailureMode::Fail`], a transient backend failure. Under
    /// `Degrade` — the default — that becomes `Ok(None)`.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// assert_eq!(kv.get::<Profile>(&7).await?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get<N: Namespace>(&self, key: &N::Key) -> Result<Option<N::Value>> {
        Ok(self.entry::<N>(key).await?.map(CachedValue::into_value))
    }

    /// Read a value together with its age and whether it is a cached absence.
    ///
    /// The age is always [`Duration::ZERO`] for an unframed codec.
    ///
    /// # Errors
    ///
    /// As [`get`](Self::get).
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    /// use std::time::Duration;
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// kv.set::<Profile>(&7, &"alice".to_owned()).await?;
    ///
    /// let entry = kv.entry::<Profile>(&7).await?.expect("just written");
    /// assert_eq!(entry.value, "alice");
    /// assert!(entry.age < Duration::from_secs(1));
    /// assert!(!entry.negative);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn entry<N: Namespace>(&self, key: &N::Key) -> Result<Option<CachedValue<N::Value>>> {
        let key = self.key::<N>(key)?;
        let raw = self
            .guarded(
                N::NAME,
                N::FAILURE_MODE,
                "get",
                || None,
                || self.inner.store.get(&key),
            )
            .await?;

        let Some(raw) = raw else {
            self.inner.counters.misses.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };

        match self.decode::<N>(&raw) {
            Ok(entry) => {
                self.inner.counters.hits.fetch_add(1, Ordering::Relaxed);
                Ok(Some(entry))
            }
            Err(error) => {
                // See the module documentation: bytes written by another
                // version of this program are a miss, never a 500.
                self.inner
                    .counters
                    .decode_failures
                    .fetch_add(1, Ordering::Relaxed);
                self.inner.counters.misses.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(
                    namespace = N::NAME,
                    version = N::VERSION,
                    error = %Chain(&error),
                    "a cached value did not decode and was treated as a miss; bump the \
                     namespace's `version` if its shape changed"
                );
                Ok(None)
            }
        }
    }

    /// Write a value under the namespace's own TTL.
    ///
    /// # Errors
    ///
    /// [`Error::Codec`] when the value does not serialise, and — under
    /// [`FailureMode::Fail`] — a transient backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// kv.set::<Profile>(&7, &"alice".to_owned()).await?;
    /// assert_eq!(kv.get::<Profile>(&7).await?.as_deref(), Some("alice"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set<N: Namespace>(&self, key: &N::Key, value: &N::Value) -> Result<()> {
        let ttl = N::ttl_for(value);
        self.write::<N>(key, value, SetOpts::new().maybe_ttl(ttl))
            .await
            .map(|_| ())
    }

    /// Write a value with an explicit TTL, overriding the namespace's.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set).
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    /// use std::time::Duration;
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// kv.set_ttl::<Profile>(&7, &"alice".to_owned(), Duration::from_secs(60)).await?;
    /// assert!(kv.ttl::<Profile>(&7).await?.is_some());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_ttl<N: Namespace>(
        &self,
        key: &N::Key,
        value: &N::Value,
        ttl: Duration,
    ) -> Result<()> {
        self.write::<N>(key, value, SetOpts::new().ttl(ttl))
            .await
            .map(|_| ())
    }

    /// Write only if the key is absent, returning whether it applied.
    ///
    /// The primitive behind one-time codes and idempotency keys.
    ///
    /// # Errors
    ///
    /// As [`set`](Self::set).
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// One-time login codes.
    ///     pub LoginCode: String => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let key = "a@b.test".to_owned();
    ///
    /// assert!(kv.set_if_absent::<LoginCode>(&key, &"123456".to_owned()).await?);
    /// assert!(!kv.set_if_absent::<LoginCode>(&key, &"999999".to_owned()).await?);
    /// assert_eq!(kv.get::<LoginCode>(&key).await?.as_deref(), Some("123456"));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn set_if_absent<N: Namespace>(
        &self,
        key: &N::Key,
        value: &N::Value,
    ) -> Result<bool> {
        let ttl = N::ttl_for(value);
        self.write::<N>(key, value, SetOpts::new().maybe_ttl(ttl).if_absent())
            .await
    }

    /// Remove a key, returning whether it was there.
    ///
    /// # Errors
    ///
    /// Under [`FailureMode::Fail`], a transient backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// kv.set::<Profile>(&7, &"alice".to_owned()).await?;
    /// assert!(kv.delete::<Profile>(&7).await?);
    /// assert!(!kv.delete::<Profile>(&7).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn delete<N: Namespace>(&self, key: &N::Key) -> Result<bool> {
        let key = self.key::<N>(key)?;
        self.guarded(
            N::NAME,
            N::FAILURE_MODE,
            "delete",
            || false,
            || self.inner.store.delete(&key),
        )
        .await
    }

    /// Whether a key is there.
    ///
    /// # Errors
    ///
    /// Under [`FailureMode::Fail`], a transient backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// assert!(!kv.exists::<Profile>(&7).await?);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn exists<N: Namespace>(&self, key: &N::Key) -> Result<bool> {
        let key = self.key::<N>(key)?;
        self.guarded(
            N::NAME,
            N::FAILURE_MODE,
            "exists",
            || false,
            || self.inner.store.exists(&key),
        )
        .await
    }

    /// How long until a key expires.
    ///
    /// # Errors
    ///
    /// Under [`FailureMode::Fail`], a transient backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// assert_eq!(kv.ttl::<Profile>(&7).await?, None);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn ttl<N: Namespace>(&self, key: &N::Key) -> Result<Option<Duration>> {
        let key = self.key::<N>(key)?;
        self.guarded(
            N::NAME,
            N::FAILURE_MODE,
            "ttl",
            || None,
            || self.inner.store.ttl(&key),
        )
        .await
    }

    /// Remove every key in a namespace, returning how many went.
    ///
    /// Needs [`Capabilities::scan`]. Bumping [`Namespace::VERSION`] is usually
    /// better: it is instant, and it does not walk a production keyspace.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] without `scan`, and — under `Fail` — a transient
    /// backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// kv.set::<Profile>(&1, &"a".to_owned()).await?;
    /// kv.set::<Profile>(&2, &"b".to_owned()).await?;
    /// assert_eq!(kv.clear_namespace::<Profile>().await?, 2);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn clear_namespace<N: Namespace>(&self) -> Result<u64> {
        let prefix = self.namespace_prefix::<N>()?;
        self.guarded(
            N::NAME,
            N::FAILURE_MODE,
            "delete_prefix",
            || 0,
            || self.inner.store.delete_prefix(&prefix),
        )
        .await
    }

    /// Every key currently in a namespace.
    ///
    /// Walks the whole namespace, one `scan` page at a time. Needs
    /// [`Capabilities::scan`].
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] without `scan`, and — under `Fail` — a transient
    /// backend failure.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Keyed by user id.
    ///     pub Profile: u64 => String;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// kv.set::<Profile>(&1, &"a".to_owned()).await?;
    /// let keys = kv.keys::<Profile>().await?;
    /// assert_eq!(keys.len(), 1);
    /// assert_eq!(keys[0].parts(), "1");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn keys<N: Namespace>(&self) -> Result<Vec<Key>> {
        let prefix = self.namespace_prefix::<N>()?;
        let mut cursor = ScanCursor::start();
        let mut out = Vec::new();
        loop {
            let (page, next) = self
                .guarded(
                    N::NAME,
                    N::FAILURE_MODE,
                    "scan",
                    || (Vec::new(), ScanCursor::end()),
                    || self.inner.store.scan(&prefix, cursor.clone(), 512),
                )
                .await?;
            out.extend(page);
            if next.is_end() {
                return Ok(out);
            }
            cursor = next;
        }
    }

    /// Add to a counter and return the new value.
    ///
    /// Only defined for a namespace whose codec is
    /// [`Raw`](crate::codec::Raw): a framed value is not something the
    /// backend's own `INCR` can read, and pretending otherwise would produce a
    /// counter that works on the memory backend and corrupts on Redis.
    ///
    /// # Errors
    ///
    /// Under [`FailureMode::Fail`], a transient backend failure. A degraded
    /// `incr` returns `0`, which is the honest answer: nothing was counted.
    ///
    /// ```
    /// use moso_kv::{minutes, Kv, Result};
    ///
    /// moso_kv::namespace! {
    ///     /// Per-IP request counter.
    ///     pub IpRate: std::net::IpAddr => u64, ttl = minutes(1), codec = Raw;
    /// }
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let ip = std::net::IpAddr::from([127, 0, 0, 1]);
    ///
    /// assert_eq!(kv.incr::<IpRate>(&ip, 1).await?, 1);
    /// assert_eq!(kv.incr::<IpRate>(&ip, 2).await?, 3);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn incr<N>(&self, key: &N::Key, by: i64) -> Result<i64>
    where
        N: Namespace<Codec = crate::codec::Raw>,
    {
        let full = self.key::<N>(key)?;
        self.guarded(
            N::NAME,
            N::FAILURE_MODE,
            "incr",
            || 0,
            || self.inner.store.incr(&full, by, N::TTL),
        )
        .await
    }

    // ── caching patterns ──────────────────────────────────────────────────

    /// Read, or compute and store, with single-flight de-duplication.
    ///
    /// A hundred concurrent callers that all miss produce **one** call to
    /// `compute`; the other ninety-nine wait for it. A `None` result of an
    /// `Option`-valued namespace is stored under
    /// [`NEGATIVE_TTL`](crate::Namespace::NEGATIVE_TTL), which is what stops a
    /// stampede against a value that is not there.
    ///
    /// # Errors
    ///
    /// Whatever `compute` returns. A store failure never fails this call under
    /// the default `Degrade`: it becomes a computation.
    ///
    /// ```
    /// use moso_kv::{minutes, Kv, Result};
    /// use std::sync::Arc;
    /// use std::sync::atomic::{AtomicUsize, Ordering};
    ///
    /// moso_kv::namespace! {
    ///     /// Expensive user profile.
    ///     pub Profile: u64 => Option<String>, ttl = minutes(5);
    /// }
    ///
    /// # #[tokio::main(flavor = "multi_thread", worker_threads = 4)]
    /// # async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let calls = Arc::new(AtomicUsize::new(0));
    ///
    /// for _ in 0..2 {
    ///     let calls = Arc::clone(&calls);
    ///     let value = kv
    ///         .get_or_insert_with::<Profile, _, _>(&7, || async move {
    ///             calls.fetch_add(1, Ordering::SeqCst);
    ///             Ok(Some("alice".to_owned()))
    ///         })
    ///         .await?;
    ///     assert_eq!(value.as_deref(), Some("alice"));
    /// }
    ///
    /// // The second call was a cache hit.
    /// assert_eq!(calls.load(Ordering::SeqCst), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_or_insert_with<N, F, Fut>(&self, key: &N::Key, compute: F) -> Result<N::Value>
    where
        N: Namespace,
        N::Value: Clone,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<N::Value>>,
    {
        if let Some(entry) = self.entry::<N>(key).await? {
            return Ok(entry.value);
        }

        let full = self.key::<N>(key)?;
        let value = self
            .inner
            .flight
            .run(full.as_str(), || async {
                // Re-check inside the flight: the leader of a previous flight
                // for this key may have finished between the read above and
                // here, and recomputing then would defeat the whole point.
                if let Some(entry) = self.entry::<N>(key).await? {
                    return Ok(entry.value);
                }
                let value = compute().await?;
                let ttl = N::ttl_for(&value);
                let _ = self
                    .write::<N>(key, &value, SetOpts::new().maybe_ttl(ttl))
                    .await?;
                Ok(value)
            })
            .await?;

        Ok(N::Value::clone(&value))
    }

    /// Serve a stale value immediately and refresh it in the background.
    ///
    /// The right default for an expensive read on a hot path: nobody waits for
    /// the recomputation, and the value is at most `fresh_for` plus one
    /// recomputation old.
    ///
    /// Needs a [`Framed`] codec, because "how old is this" is a property of the
    /// value.
    ///
    /// # Errors
    ///
    /// Whatever `compute` returns, on the path where there is no value to
    /// serve. A failing *background* refresh logs and leaves the stale value
    /// in place.
    ///
    /// ```
    /// use moso_kv::{minutes, Kv, Result};
    /// use std::time::Duration;
    ///
    /// moso_kv::namespace! {
    ///     /// Expensive dashboard numbers.
    ///     pub Dashboard: u64 => Option<u64>, ttl = minutes(10);
    /// }
    ///
    /// # #[tokio::main(flavor = "multi_thread", worker_threads = 2)]
    /// # async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    ///
    /// // Nothing cached: this one computes and waits.
    /// let first = kv
    ///     .get_swr::<Dashboard, _, _>(&1, Duration::from_millis(20), || async { Ok(Some(1)) })
    ///     .await?;
    /// assert_eq!(first, Some(1));
    ///
    /// // Fresh: served from the cache.
    /// let second = kv
    ///     .get_swr::<Dashboard, _, _>(&1, Duration::from_secs(60), || async { Ok(Some(2)) })
    ///     .await?;
    /// assert_eq!(second, Some(1));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_swr<N, F, Fut>(
        &self,
        key: &N::Key,
        fresh_for: Duration,
        compute: F,
    ) -> Result<N::Value>
    where
        N: Namespace,
        N::Codec: Framed,
        N::Value: Clone,
        N::Key: Clone + Send + Sync + Sized,
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = Result<N::Value>> + Send + 'static,
    {
        if let Some(entry) = self.entry::<N>(key).await? {
            if !entry.is_stale(fresh_for) {
                return Ok(entry.value);
            }

            // Stale: hand back what we have and refresh behind the request.
            self.inner
                .counters
                .revalidations
                .fetch_add(1, Ordering::Relaxed);
            // Built here, where a failure can still be reported, rather than
            // inside the task where it would have to be papered over. `entry`
            // already succeeded for this key, so it cannot fail — and if the
            // limits ever change, this is an error and not a shared bucket that
            // silently collapses every revalidation into one.
            //
            // The `swr:` prefix keeps a revalidation from joining the flight of
            // a *reader* that is waiting for the same key's first value: they
            // are two different computations with two different callers.
            let flight_key = format!("swr:{}", self.key::<N>(key)?.into_string());
            let kv = self.clone();
            let key = key.clone();
            tokio::spawn(async move {
                let outcome = kv
                    .inner
                    .flight
                    .run(&flight_key, || async {
                        let value = compute().await?;
                        let ttl = N::ttl_for(&value);
                        kv.write::<N>(&key, &value, SetOpts::new().maybe_ttl(ttl))
                            .await?;
                        Ok(())
                    })
                    .await;
                if let Err(error) = outcome {
                    tracing::warn!(
                        namespace = N::NAME,
                        error = %Chain(&error),
                        "a background revalidation failed; the stale value stays in place"
                    );
                }
            });

            return Ok(entry.value);
        }

        // Nothing to serve, so this caller waits — with de-duplication, since
        // a cold hot key is exactly when a stampede happens.
        self.get_or_insert_with::<N, _, _>(key, compute).await
    }

    // ── raw pubsub ────────────────────────────────────────────────────────

    /// Publish to a channel, returning how many subscribers were reached.
    ///
    /// Channels are *not* namespaced: they are a coordination surface shared
    /// with whatever else is listening, and silently rewriting the name would
    /// make that impossible. Prefix your own.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] without [`Capabilities::pubsub`], and a transient
    /// backend failure — publishes are never degraded, because a dropped
    /// notification is not a cache miss.
    ///
    /// ```
    /// use moso_kv::{Kv, Result};
    /// use bytes::Bytes;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// assert_eq!(kv.publish("orders", Bytes::from_static(b"{}")).await?, 0);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn publish(&self, channel: &str, payload: Bytes) -> Result<u64> {
        self.inner.store.publish(channel, payload).await
    }

    /// Subscribe to a channel.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] without [`Capabilities::pubsub`].
    ///
    /// ```
    /// use futures_util::StreamExt as _;
    /// use moso_kv::{Kv, Result};
    /// use bytes::Bytes;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
    /// let kv = Kv::in_memory("shop")?;
    /// let mut stream = kv.subscribe("orders").await?;
    /// kv.publish("orders", Bytes::from_static(b"paid")).await?;
    /// assert_eq!(stream.next().await, Some(Bytes::from_static(b"paid")));
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe(&self, channel: &str) -> Result<MessageStream> {
        self.inner.store.subscribe(channel).await
    }

    // ── internals ─────────────────────────────────────────────────────────

    /// Encode and write, returning whether the write applied.
    pub(crate) async fn write<N: Namespace>(
        &self,
        key: &N::Key,
        value: &N::Value,
        opts: SetOpts,
    ) -> Result<bool> {
        let full = self.key::<N>(key)?;
        let bytes = self.encode::<N>(value)?;
        let applied = self
            .guarded(
                N::NAME,
                N::FAILURE_MODE,
                "set",
                || false,
                || self.inner.store.set(&full, bytes.clone(), opts),
            )
            .await?;
        if applied {
            self.inner.counters.writes.fetch_add(1, Ordering::Relaxed);
        }
        Ok(applied)
    }

    /// A value as the store holds it: the codec's bytes, framed if the codec
    /// is framed.
    fn encode<N: Namespace>(&self, value: &N::Value) -> Result<Bytes> {
        let payload = value
            .encode_value()
            .map_err(|source| Error::codec(N::NAME, source))?;
        Ok(if <N::Codec as Codec>::FRAMED {
            Envelope::wrap(payload, N::is_negative(value))
        } else {
            payload
        })
    }

    /// The inverse of [`Self::encode`].
    fn decode<N: Namespace>(&self, raw: &[u8]) -> Result<CachedValue<N::Value>> {
        if <N::Codec as Codec>::FRAMED {
            let envelope = Envelope::open(raw).map_err(|source| Error::codec(N::NAME, source))?;
            let value = N::Value::decode_value(envelope.payload)
                .map_err(|source| Error::codec(N::NAME, source))?;
            Ok(CachedValue {
                value,
                age: envelope.age(),
                negative: envelope.negative,
            })
        } else {
            let value =
                N::Value::decode_value(raw).map_err(|source| Error::codec(N::NAME, source))?;
            Ok(CachedValue {
                value,
                age: Duration::ZERO,
                negative: false,
            })
        }
    }

    /// The span every backend operation runs inside.
    ///
    /// Cheap and bounded: it carries the operation name and the backend name,
    /// both `&'static str`, and never a key or a value — a cached value may be a
    /// session or a secret, and a key names the subject. The same posture as the
    /// ORM's per-statement span, so KV work shows up in a request trace beside
    /// the SQL it stands in for. `pub(crate)` because the lock and rate-limit
    /// operations live in sibling modules and reach the store without passing
    /// through [`guarded`](Self::guarded).
    pub(crate) fn op_span(&self, operation: &'static str) -> tracing::Span {
        tracing::debug_span!(
            target: "moso::kv",
            "kv.op",
            op = operation,
            backend = self.inner.store.name(),
        )
    }

    /// Run one store operation behind the circuit breaker and the failure
    /// policy.
    async fn guarded<T, F, Fut>(
        &self,
        namespace: &'static str,
        mode: FailureMode,
        operation: &'static str,
        fallback: impl FnOnce() -> T,
        call: F,
    ) -> Result<T>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        use tracing::Instrument as _;

        moso_core::middleware::metrics::counter(KV_OPERATIONS_METRIC).increment(1);

        if let Err(remaining) = self.inner.breaker.allow() {
            return self.on_failure(
                namespace,
                mode,
                operation,
                Error::CircuitOpen {
                    backend: self.inner.store.name(),
                    retry_after: remaining,
                },
                fallback,
            );
        }

        match call().instrument(self.op_span(operation)).await {
            Ok(value) => {
                self.inner.breaker.record_success();
                Ok(value)
            }
            Err(error) => self.on_failure(namespace, mode, operation, error, fallback),
        }
    }

    /// Degrade or propagate, and keep the breaker and the counters honest.
    fn on_failure<T>(
        &self,
        namespace: &'static str,
        mode: FailureMode,
        operation: &'static str,
        error: Error,
        fallback: impl FnOnce() -> T,
    ) -> Result<T> {
        if !error.retryable() {
            // A bug does not open a circuit, and it is never degraded away.
            self.inner.counters.errors.fetch_add(1, Ordering::Relaxed);
            return Err(error);
        }

        // Past the guard above this is a genuine backend failure — an
        // unreachable store or an already-open circuit — so it moves the
        // process-wide `moso_kv_errors_total` whether the namespace goes on to
        // degrade or to fail. The counter carries no key: only that the backend
        // failed, which is all an alert needs.
        moso_core::middleware::metrics::counter(KV_ERRORS_METRIC).increment(1);

        // `CircuitOpen` is produced *by* the breaker, so counting it as a
        // failure again would ratchet the cooldown on traffic the store never
        // saw.
        if !matches!(error, Error::CircuitOpen { .. }) {
            self.inner.breaker.record_failure();
        }

        if mode.degrades() {
            self.inner.counters.degraded.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                namespace,
                operation,
                backend = self.inner.store.name(),
                error = %Chain(&error),
                "the kv backend failed; degrading to a miss"
            );
            return Ok(fallback());
        }

        self.inner.counters.errors.fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            namespace,
            operation,
            backend = self.inner.store.name(),
            error = %Chain(&error),
            "the kv backend failed and this namespace is `on_failure = fail`"
        );
        Err(error)
    }
}

// ---------------------------------------------------------------------------
// KvBuilder
// ---------------------------------------------------------------------------

/// Builds a [`Kv`].
///
/// ```
/// use moso_kv::backend::MemoryStore;
/// use moso_kv::breaker::BreakerConfig;
/// use moso_kv::Kv;
/// use std::time::Duration;
///
/// let kv = Kv::builder("shop")
///     .store(MemoryStore::with_capacity(1_000))
///     .breaker(BreakerConfig::default().failure_threshold(3))
///     .build()
///     .expect("built");
///
/// assert_eq!(kv.app(), "shop");
/// assert_eq!(kv.breaker().config().failure_threshold, 3);
/// ```
pub struct KvBuilder {
    app: String,
    store: Option<Arc<dyn KvStore>>,
    breaker: BreakerConfig,
}

impl std::fmt::Debug for KvBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KvBuilder")
            .field("app", &self.app)
            .field("store", &self.store.as_ref().map(|store| store.name()))
            .field("breaker", &self.breaker)
            .finish()
    }
}

impl KvBuilder {
    /// Use `store` as the backend.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    /// use moso_kv::Kv;
    ///
    /// assert!(Kv::builder("shop").store(MemoryStore::new()).build().is_ok());
    /// ```
    #[must_use]
    pub fn store(mut self, store: impl KvStore) -> Self {
        self.store = Some(Arc::new(store));
        self
    }

    /// Use an already-shared backend.
    ///
    /// Two `Kv` handles over one store is how two applications share a Redis
    /// while keeping separate keyspaces.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    /// use moso_kv::{Kv, KvStore};
    /// use std::sync::Arc;
    ///
    /// let store: Arc<dyn KvStore> = Arc::new(MemoryStore::new());
    /// let shop = Kv::builder("shop").shared_store(Arc::clone(&store)).build().expect("built");
    /// let blog = Kv::builder("blog").shared_store(store).build().expect("built");
    ///
    /// assert_eq!(shop.app(), "shop");
    /// assert_eq!(blog.app(), "blog");
    /// ```
    #[must_use]
    pub fn shared_store(mut self, store: Arc<dyn KvStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Configure the circuit breaker.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    /// use moso_kv::breaker::BreakerConfig;
    /// use moso_kv::Kv;
    ///
    /// let kv = Kv::builder("shop")
    ///     .store(MemoryStore::new())
    ///     .breaker(BreakerConfig::never())
    ///     .build()
    ///     .expect("built");
    /// assert_eq!(kv.breaker().config().failure_threshold, u32::MAX);
    /// ```
    #[must_use]
    pub fn breaker(mut self, config: BreakerConfig) -> Self {
        self.breaker = config;
        self
    }

    /// Finish.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when the application name is not usable, and
    /// [`Error::Config`] when no store was given — a `Kv` with no backend is a
    /// misconfiguration that would otherwise fail on the first request.
    ///
    /// ```
    /// use moso_kv::Kv;
    ///
    /// let error = Kv::builder("shop").build().expect_err("no store");
    /// assert!(error.to_string().contains("backend"));
    /// ```
    pub fn build(self) -> Result<Kv> {
        validate_name("application", &self.app)?;
        let store = self.store.ok_or_else(|| Error::Config {
            detail: String::from(
                "no backend was given. help: Kv::builder(app).store(MemoryStore::new()).build()",
            ),
        })?;

        Ok(Kv {
            inner: Arc::new(Inner {
                store,
                app: self.app,
                breaker: Breaker::new(self.breaker),
                flight: SingleFlight::new(),
                counters: Counters::default(),
            }),
        })
    }
}

// ---------------------------------------------------------------------------
// Dependency
// ---------------------------------------------------------------------------

/// `Kv` in a handler signature, through `Depends<Kv>`.
///
/// Resolved from the provider map, which is where the composition root put it,
/// so the boot-time graph check catches a missing `Kv` before the first request
/// rather than in it.
///
/// ```
/// use moso_core::Dependency;
/// use moso_kv::Kv;
///
/// // One requirement: the application must provide a `Kv`.
/// assert_eq!(<Kv as Dependency>::PROVIDER_REQ.len(), 1);
/// ```
impl moso_core::Dependency for Kv {
    const PROVIDER_REQ: &'static [moso_core::ProviderReq] = &[moso_core::ProviderReq::of::<Kv>()];

    fn resolve(
        ctx: &moso_core::RequestCtx,
    ) -> impl Future<Output = moso_core::Result<Self>> + Send {
        let resolved = ctx.provider::<Kv>().map(|kv| Kv::clone(&kv));
        async move { resolved }
    }
}

// ---------------------------------------------------------------------------
// HealthStatus bridging
// ---------------------------------------------------------------------------

impl Kv {
    /// Ask the backend whether it is reachable.
    ///
    /// ```
    /// use moso_core::HealthStatus;
    /// use moso_kv::Kv;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let kv = Kv::in_memory("shop").expect("built");
    /// assert_eq!(kv.health().await, HealthStatus::Up);
    /// # }
    /// ```
    pub async fn health(&self) -> HealthStatus {
        self.inner.store.health().await
    }
}

#[cfg(all(test, feature = "memory"))]
mod tests {
    use super::*;
    use crate::backend::MemoryStore;
    use crate::namespace::{minutes, seconds};
    use std::sync::atomic::AtomicUsize;

    crate::namespace! {
        /// A profile, cached.
        pub Profile: u64 => Option<String>, ttl = minutes(5), negative_ttl = seconds(1);

        /// A namespace that fails rather than degrading.
        pub Session: String => String, ttl = minutes(60), on_failure = fail;

        /// A raw counter.
        pub Counter: String => u64, codec = Raw, ttl = minutes(1);
    }

    fn kv() -> Kv {
        Kv::in_memory("shop").expect("built")
    }

    #[tokio::test]
    async fn keys_are_namespaced_versioned_and_escaped() {
        let kv = kv();
        assert_eq!(
            kv.key::<Profile>(&7).expect("short").as_str(),
            "moso:v1:shop:profile:1:7"
        );
        assert_eq!(
            kv.key::<Session>(&"a:b".to_owned()).expect("short").parts(),
            "a\\cb"
        );
        assert_eq!(
            kv.namespace_prefix::<Profile>().expect("short").as_str(),
            "moso:v1:shop:profile:1:"
        );
    }

    #[tokio::test]
    async fn a_value_round_trips_through_the_codec() {
        let kv = kv();
        assert_eq!(kv.get::<Profile>(&7).await.expect("get"), None);

        kv.set::<Profile>(&7, &Some("alice".to_owned()))
            .await
            .expect("set");
        assert_eq!(
            kv.get::<Profile>(&7).await.expect("get"),
            Some(Some("alice".to_owned()))
        );
        assert_eq!(kv.stats().writes, 1);
        assert_eq!(kv.stats().hits, 1);
        assert_eq!(kv.stats().misses, 1);
    }

    #[tokio::test]
    async fn a_cached_none_is_distinguishable_from_an_absent_key() {
        let kv = kv();
        kv.set::<Profile>(&7, &None).await.expect("set");

        let entry = kv
            .entry::<Profile>(&7)
            .await
            .expect("get")
            .expect("present");
        assert_eq!(entry.value, None);
        assert!(entry.negative);

        // ... and it went in under the *negative* ttl.
        let ttl = kv.ttl::<Profile>(&7).await.expect("ttl").expect("a ttl");
        assert!(ttl <= seconds(1), "{ttl:?}");
    }

    #[tokio::test]
    async fn set_if_absent_is_a_real_condition() {
        let kv = kv();
        let key = "a@b.test".to_owned();
        assert!(
            kv.set_if_absent::<Session>(&key, &"first".to_owned())
                .await
                .expect("set")
        );
        assert!(
            !kv.set_if_absent::<Session>(&key, &"second".to_owned())
                .await
                .expect("set")
        );
        assert_eq!(
            kv.get::<Session>(&key).await.expect("get").as_deref(),
            Some("first")
        );
    }

    #[tokio::test]
    async fn delete_exists_and_ttl_agree() {
        let kv = kv();
        assert!(!kv.exists::<Profile>(&1).await.expect("exists"));
        kv.set::<Profile>(&1, &Some("x".to_owned()))
            .await
            .expect("set");
        assert!(kv.exists::<Profile>(&1).await.expect("exists"));
        assert!(kv.ttl::<Profile>(&1).await.expect("ttl").is_some());
        assert!(kv.delete::<Profile>(&1).await.expect("delete"));
        assert!(!kv.delete::<Profile>(&1).await.expect("delete"));
    }

    #[tokio::test]
    async fn a_namespace_can_be_cleared_and_listed() {
        let kv = kv();
        for id in 0..5_u64 {
            kv.set::<Profile>(&id, &Some(id.to_string()))
                .await
                .expect("set");
        }
        kv.set::<Session>(&"s".to_owned(), &"v".to_owned())
            .await
            .expect("set");

        assert_eq!(kv.keys::<Profile>().await.expect("keys").len(), 5);
        assert_eq!(kv.clear_namespace::<Profile>().await.expect("clear"), 5);
        assert!(kv.keys::<Profile>().await.expect("keys").is_empty());
        // The other namespace is untouched.
        assert!(kv.exists::<Session>(&"s".to_owned()).await.expect("exists"));
    }

    #[tokio::test]
    async fn a_raw_counter_uses_the_backends_own_representation() {
        let kv = kv();
        let key = "ip".to_owned();
        assert_eq!(kv.incr::<Counter>(&key, 1).await.expect("incr"), 1);
        assert_eq!(kv.incr::<Counter>(&key, 4).await.expect("incr"), 5);

        // ... and reading it back through the namespace gives the same number.
        assert_eq!(kv.get::<Counter>(&key).await.expect("get"), Some(5));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn get_or_insert_with_deduplicates_and_then_caches() {
        let kv = kv();
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..32 {
            let kv = kv.clone();
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                kv.get_or_insert_with::<Profile, _, _>(&99, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    Ok(Some("built".to_owned()))
                })
                .await
            }));
        }
        for handle in handles {
            assert_eq!(
                handle.await.expect("joined").expect("value"),
                Some("built".to_owned())
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        // And afterwards it is a plain hit.
        kv.get_or_insert_with::<Profile, _, _>(&99, || async {
            panic!("the value is cached");
        })
        .await
        .expect("hit");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_negative_result_is_cached_too() {
        let kv = kv();
        let calls = AtomicUsize::new(0);

        for _ in 0..3 {
            let value = kv
                .get_or_insert_with::<Profile, _, _>(&5, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
                .await
                .expect("value");
            assert_eq!(value, None);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1, "the `None` was cached");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_swr_serves_stale_and_refreshes_behind_the_request() {
        let kv = kv();

        kv.set::<Profile>(&3, &Some("old".to_owned()))
            .await
            .expect("set");
        tokio::time::sleep(Duration::from_millis(30)).await;

        let served = kv
            .get_swr::<Profile, _, _>(&3, Duration::from_millis(10), || async {
                Ok(Some("new".to_owned()))
            })
            .await
            .expect("swr");
        assert_eq!(served, Some("old".to_owned()), "the stale value is served");
        assert_eq!(kv.stats().revalidations, 1);

        // The refresh lands shortly afterwards.
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            if kv.get::<Profile>(&3).await.expect("get") == Some(Some("new".to_owned())) {
                return;
            }
        }
        panic!("the background revalidation never landed");
    }

    #[tokio::test]
    async fn get_swr_waits_when_there_is_nothing_to_serve() {
        let kv = kv();
        let value = kv
            .get_swr::<Profile, _, _>(&4, Duration::from_secs(60), || async {
                Ok(Some("computed".to_owned()))
            })
            .await
            .expect("swr");
        assert_eq!(value, Some("computed".to_owned()));
        assert_eq!(kv.stats().revalidations, 0);
    }

    #[tokio::test]
    async fn bytes_that_do_not_decode_are_a_miss_and_not_a_500() {
        let kv = kv();
        let key = kv.key::<Profile>(&8).expect("short");

        // Bytes written by a different version of this program.
        kv.store()
            .set(&key, Bytes::from_static(b"not a frame"), SetOpts::new())
            .await
            .expect("set");

        assert_eq!(kv.get::<Profile>(&8).await.expect("get"), None);
        assert_eq!(kv.stats().decode_failures, 1);
    }

    #[tokio::test]
    async fn the_builder_refuses_a_bad_application_name_and_a_missing_store() {
        assert!(
            Kv::builder("Shop")
                .store(MemoryStore::new())
                .build()
                .is_err()
        );
        assert!(Kv::builder("shop").build().is_err());
        assert!(Kv::in_memory("sh:op").is_err());
    }

    #[tokio::test]
    async fn two_applications_over_one_store_do_not_collide() {
        let store: Arc<dyn KvStore> = Arc::new(MemoryStore::new());
        let shop = Kv::builder("shop")
            .shared_store(Arc::clone(&store))
            .breaker(BreakerConfig::never())
            .build()
            .expect("built");
        let blog = Kv::builder("blog")
            .shared_store(store)
            .breaker(BreakerConfig::never())
            .build()
            .expect("built");

        shop.set::<Profile>(&1, &Some("shop".to_owned()))
            .await
            .expect("set");
        assert_eq!(blog.get::<Profile>(&1).await.expect("get"), None);
    }

    #[tokio::test]
    async fn pubsub_goes_through_untouched() {
        use futures_util::StreamExt as _;

        let kv = kv();
        let mut stream = kv.subscribe("orders").await.expect("subscribe");
        assert_eq!(
            kv.publish("orders", Bytes::from_static(b"paid"))
                .await
                .expect("publish"),
            1
        );
        assert_eq!(stream.next().await, Some(Bytes::from_static(b"paid")));
    }

    #[test]
    fn a_handle_describes_itself() {
        let kv = kv();
        let rendered = format!("{kv:?}");
        assert!(rendered.contains("shop"), "{rendered}");
        assert!(rendered.contains("memory"), "{rendered}");
    }

    #[test]
    fn the_stats_ratio_is_defined_at_zero() {
        assert_eq!(KvStats::default().hit_ratio(), 0.0);
        let stats = KvStats {
            hits: 3,
            misses: 1,
            ..KvStats::default()
        };
        assert!((stats.hit_ratio() - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn a_cached_value_knows_whether_it_is_stale() {
        let entry = CachedValue {
            value: 1_u8,
            age: Duration::from_secs(5),
            negative: false,
        };
        assert!(entry.is_stale(Duration::from_secs(1)));
        assert!(!entry.is_stale(Duration::from_secs(10)));
        assert_eq!(entry.clone().into_value(), 1);
    }
}
