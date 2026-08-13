//! The in-process backend, on `moka`.
//!
//! What makes a test suite runnable with no external dependency, which
//! `docs/02-data/25-kv-cache.md` calls a Loop-1 requirement. It is not a toy:
//! it implements the *same semantics* as Redis, including per-key TTL,
//! compare-and-swap, lists, sets, sorted sets and pubsub, so a test that passes
//! here passes there.
//!
//! # The two things it genuinely cannot do
//!
//! 1. **Cross-process pubsub.** A publish reaches subscribers in this process
//!    and nowhere else. [`Capabilities::pubsub_cross_process`] is `false`, and
//!    a test that needs two processes to talk gates on it.
//! 2. **Persistence.** Everything is gone when the process is.
//!
//! Both are reported rather than papered over, which is the whole reason
//! [`Capabilities`] exists.
//!
//! # How TTL works here
//!
//! Every entry carries its own `expires_at`. Two things read it:
//!
//! * `moka`'s [`Expiry`](moka::Expiry) hook, so an expired entry is *evicted*
//!   and stops costing memory;
//! * every read, so an entry that `moka` has not got round to evicting is
//!   still invisible.
//!
//! The second is what makes "TTLs are honoured within 50 ms" true rather than
//! approximately true: eviction is a background concern, visibility is not.
//!
//! # Atomicity
//!
//! Reads are lock-free. Every read-modify-write — `incr`, the three
//! compare-and-swap operations, and all of the structure operations — takes one
//! `Mutex` for the duration of the operation. That is coarser than a per-key
//! lock and it is the right trade for an in-process cache: the critical
//! sections are a few hundred nanoseconds of `HashMap` work, and a per-key lock
//! table would be more machinery than the thing it protects.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use moka::future::Cache;
use moso_core::{BoxFuture, HealthStatus};
use tokio::sync::{Mutex, broadcast};

use crate::error::{Error, Result};
use crate::key::Key;
use crate::store::{Capabilities, KvStore, MessageStream, ScanCursor, SetOpts, Side};

/// The name this backend reports.
pub(crate) const BACKEND: &str = "memory";

/// How many entries a [`MemoryStore::new`] holds before it starts evicting.
///
/// ```
/// use moso_kv::backend::memory::DEFAULT_CAPACITY;
///
/// assert_eq!(DEFAULT_CAPACITY, 10_000);
/// ```
pub const DEFAULT_CAPACITY: u64 = 10_000;

/// How many subscribers' worth of buffered messages a channel keeps.
///
/// A subscriber that falls this far behind loses the messages in between, which
/// is what Redis pubsub does too — it is at-most-once delivery either way.
///
/// ```
/// use moso_kv::backend::memory::CHANNEL_DEPTH;
///
/// assert_eq!(CHANNEL_DEPTH, 256);
/// ```
pub const CHANNEL_DEPTH: usize = 256;

// ---------------------------------------------------------------------------
// The stored value
// ---------------------------------------------------------------------------

/// What one key holds. Mirrors the Redis type system, minus the parts
/// [`KvStore`] does not expose.
#[derive(Debug, Clone)]
enum Value {
    /// A plain byte string.
    Bytes(Bytes),
    /// A list, head at the front.
    List(Vec<Bytes>),
    /// A set, kept in insertion order because the trait promises no order.
    Set(Vec<Bytes>),
    /// A sorted set, kept sorted by score then member.
    ZSet(Vec<(f64, Bytes)>),
}

impl Value {
    /// The name Redis uses in a `WRONGTYPE` error.
    const fn type_name(&self) -> &'static str {
        match self {
            Value::Bytes(_) => "string",
            Value::List(_) => "list",
            Value::Set(_) => "set",
            Value::ZSet(_) => "zset",
        }
    }
}

/// A value and when it stops being visible.
#[derive(Debug, Clone)]
struct Stored {
    value: Value,
    expires_at: Option<Instant>,
}

impl Stored {
    /// How long until this entry expires, or `None` when it never does.
    fn remaining(&self) -> Option<Duration> {
        self.expires_at
            .map(|at| at.saturating_duration_since(Instant::now()))
    }

    /// Whether this entry is already invisible.
    fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|at| at <= Instant::now())
    }
}

/// Tells `moka` when to evict, from the entry's own `expires_at`.
struct StoredExpiry;

impl moka::Expiry<String, Stored> for StoredExpiry {
    fn expire_after_create(
        &self,
        _key: &String,
        value: &Stored,
        _created_at: std::time::Instant,
    ) -> Option<Duration> {
        value.remaining()
    }

    fn expire_after_update(
        &self,
        _key: &String,
        value: &Stored,
        _updated_at: std::time::Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        value.remaining()
    }
}

// ---------------------------------------------------------------------------
// MemoryStore
// ---------------------------------------------------------------------------

/// An in-process [`KvStore`].
///
/// ```
/// use moso_kv::backend::MemoryStore;
/// use moso_kv::{Key, KvStore, SetOpts};
/// use bytes::Bytes;
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() {
/// let store = MemoryStore::new();
/// let key = Key::from_raw("moso:v1:app:demo:1:a").expect("valid");
///
/// store.set(&key, Bytes::from_static(b"1"), SetOpts::new()).await.expect("set");
/// assert_eq!(store.get(&key).await.expect("get").as_deref(), Some(&b"1"[..]));
/// assert!(store.delete(&key).await.expect("delete"));
/// # }
/// ```
#[derive(Clone)]
pub struct MemoryStore {
    inner: Arc<Inner>,
}

struct Inner {
    cache: Cache<String, Stored>,
    /// Serialises every read-modify-write. See the module documentation.
    write: Mutex<()>,
    channels: std::sync::Mutex<HashMap<String, broadcast::Sender<Bytes>>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("entries", &self.inner.cache.entry_count())
            .finish_non_exhaustive()
    }
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    /// A store holding up to [`DEFAULT_CAPACITY`] entries.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    ///
    /// let store = MemoryStore::new();
    /// assert_eq!(store.len(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    /// A store holding up to `capacity` entries before evicting the least
    /// useful ones.
    ///
    /// The bound is what stops an in-process cache from being an in-process
    /// memory leak. `moka` decides *which* entry goes using TinyLFU, which
    /// keeps the entries that are actually being read.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    ///
    /// let store = MemoryStore::with_capacity(128);
    /// assert_eq!(store.capacity(), 128);
    /// ```
    #[must_use]
    pub fn with_capacity(capacity: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                cache: Cache::builder()
                    .max_capacity(capacity)
                    .expire_after(StoredExpiry)
                    .build(),
                write: Mutex::new(()),
                channels: std::sync::Mutex::new(HashMap::new()),
            }),
        }
    }

    /// The configured capacity.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    ///
    /// assert_eq!(MemoryStore::with_capacity(7).capacity(), 7);
    /// ```
    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.inner.cache.policy().max_capacity().unwrap_or(u64::MAX)
    }

    /// Roughly how many entries are held.
    ///
    /// `moka` counts asynchronously, so this lags a burst of writes; it is a
    /// diagnostic, not an invariant.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    ///
    /// assert_eq!(MemoryStore::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> u64 {
        self.inner.cache.entry_count()
    }

    /// Whether the store is (roughly) empty.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    ///
    /// assert!(MemoryStore::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Forget everything.
    ///
    /// ```
    /// use moso_kv::backend::MemoryStore;
    /// use moso_kv::{Key, KvStore, SetOpts};
    /// use bytes::Bytes;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let store = MemoryStore::new();
    /// let key = Key::from_raw("k").expect("valid");
    /// store.set(&key, Bytes::from_static(b"v"), SetOpts::new()).await.expect("set");
    /// store.clear().await;
    /// assert!(store.get(&key).await.expect("get").is_none());
    /// # }
    /// ```
    pub async fn clear(&self) {
        self.inner.cache.invalidate_all();
        self.inner.cache.run_pending_tasks().await;
    }

    /// The live entry for `key`, or `None` when it is absent or expired.
    async fn live(&self, key: &Key) -> Option<Stored> {
        match self.inner.cache.get(key.as_str()).await {
            Some(stored) if stored.is_expired() => {
                self.inner.cache.invalidate(key.as_str()).await;
                None
            }
            other => other,
        }
    }

    /// The byte string under `key`, or a `WRONGTYPE`-shaped error.
    async fn live_bytes(&self, key: &Key, operation: &'static str) -> Result<Option<Bytes>> {
        match self.live(key).await {
            None => Ok(None),
            Some(Stored {
                value: Value::Bytes(bytes),
                ..
            }) => Ok(Some(bytes)),
            Some(stored) => Err(wrong_type(operation, "string", stored.value.type_name())),
        }
    }

    /// Write `value`, keeping `expires_at`.
    async fn put(&self, key: &Key, value: Value, expires_at: Option<Instant>) {
        self.inner
            .cache
            .insert(key.as_str().to_owned(), Stored { value, expires_at })
            .await;
    }

    /// The list under `key` — an absent key is an empty list, as in Redis.
    async fn live_list(
        &self,
        key: &Key,
        operation: &'static str,
    ) -> Result<(Vec<Bytes>, Option<Instant>)> {
        match self.live(key).await {
            None => Ok((Vec::new(), None)),
            Some(Stored {
                value: Value::List(items),
                expires_at,
            }) => Ok((items, expires_at)),
            Some(stored) => Err(wrong_type(operation, "list", stored.value.type_name())),
        }
    }

    /// The set under `key`.
    async fn live_set(
        &self,
        key: &Key,
        operation: &'static str,
    ) -> Result<(Vec<Bytes>, Option<Instant>)> {
        match self.live(key).await {
            None => Ok((Vec::new(), None)),
            Some(Stored {
                value: Value::Set(items),
                expires_at,
            }) => Ok((items, expires_at)),
            Some(stored) => Err(wrong_type(operation, "set", stored.value.type_name())),
        }
    }

    /// The sorted set under `key`.
    async fn live_zset(
        &self,
        key: &Key,
        operation: &'static str,
    ) -> Result<(Vec<(f64, Bytes)>, Option<Instant>)> {
        match self.live(key).await {
            None => Ok((Vec::new(), None)),
            Some(Stored {
                value: Value::ZSet(items),
                expires_at,
            }) => Ok((items, expires_at)),
            Some(stored) => Err(wrong_type(operation, "zset", stored.value.type_name())),
        }
    }

    /// The sender for `channel`, creating it on first use.
    fn channel(&self, channel: &str) -> broadcast::Sender<Bytes> {
        let mut channels = self
            .inner
            .channels
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        channels
            .entry(channel.to_owned())
            .or_insert_with(|| broadcast::Sender::new(CHANNEL_DEPTH))
            .clone()
    }
}

/// The error a type mismatch produces, shaped like Redis' `WRONGTYPE`.
fn wrong_type(operation: &'static str, wanted: &'static str, found: &'static str) -> Error {
    Error::backend(
        BACKEND,
        operation,
        format!("WRONGTYPE: this key holds a {found}, and `{operation}` needs a {wanted}"),
    )
}

/// `Instant` for a TTL, if there is one.
fn deadline(ttl: Option<Duration>) -> Option<Instant> {
    ttl.map(|ttl| Instant::now() + ttl)
}

impl KvStore for MemoryStore {
    fn name(&self) -> &'static str {
        BACKEND
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::memory()
    }

    fn health(&self) -> BoxFuture<'_, HealthStatus> {
        // Nothing to reach: if this process is running, this store is up.
        Box::pin(async { HealthStatus::Up })
    }

    fn get<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move { self.live_bytes(key, "get").await })
    }

    fn set<'a>(&'a self, key: &'a Key, value: Bytes, opts: SetOpts) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            opts.validate()?;
            let _guard = self.inner.write.lock().await;

            let existing = self.live(key).await;
            if opts.if_absent && existing.is_some() {
                return Ok(false);
            }
            if opts.if_present && existing.is_none() {
                return Ok(false);
            }

            let expires_at = if opts.keep_ttl {
                existing.and_then(|stored| stored.expires_at)
            } else {
                deadline(opts.ttl)
            };
            self.put(key, Value::Bytes(value), expires_at).await;
            Ok(true)
        })
    }

    fn delete<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let existed = self.live(key).await.is_some();
            self.inner.cache.invalidate(key.as_str()).await;
            Ok(existed)
        })
    }

    fn exists<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move { Ok(self.live(key).await.is_some()) })
    }

    fn expire<'a>(&'a self, key: &'a Key, ttl: Duration) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let Some(stored) = self.live(key).await else {
                return Ok(false);
            };
            self.put(key, stored.value, deadline(Some(ttl))).await;
            Ok(true)
        })
    }

    fn ttl<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move { Ok(self.live(key).await.and_then(|stored| stored.remaining())) })
    }

    fn incr<'a>(
        &'a self,
        key: &'a Key,
        by: i64,
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;

            let existing = self.live(key).await;
            let (current, expires_at, is_new) = match existing {
                None => (0_i64, deadline(ttl), true),
                Some(Stored {
                    value: Value::Bytes(bytes),
                    expires_at,
                }) => {
                    let text = std::str::from_utf8(&bytes).map_err(|_| not_an_integer("incr"))?;
                    let parsed: i64 = text.trim().parse().map_err(|_| not_an_integer("incr"))?;
                    (parsed, expires_at, false)
                }
                Some(stored) => {
                    return Err(wrong_type("incr", "string", stored.value.type_name()));
                }
            };
            let _ = is_new;

            let next = current
                .checked_add(by)
                .ok_or_else(|| Error::backend(BACKEND, "incr", "counter overflowed an i64"))?;
            self.put(
                key,
                Value::Bytes(Bytes::from(next.to_string().into_bytes())),
                expires_at,
            )
            .await;
            Ok(next)
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
            let _guard = self.inner.write.lock().await;

            let existing = self.live(key).await;
            let current = match &existing {
                None => None,
                Some(Stored {
                    value: Value::Bytes(bytes),
                    ..
                }) => Some(bytes.as_ref()),
                Some(stored) => {
                    return Err(wrong_type(
                        "compare_and_swap",
                        "string",
                        stored.value.type_name(),
                    ));
                }
            };

            if current != old {
                return Ok(false);
            }

            let expires_at = if opts.keep_ttl {
                existing.as_ref().and_then(|stored| stored.expires_at)
            } else {
                deadline(opts.ttl)
            };
            self.put(key, Value::Bytes(new), expires_at).await;
            Ok(true)
        })
    }

    fn compare_and_delete<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            match self.live(key).await {
                Some(Stored {
                    value: Value::Bytes(bytes),
                    ..
                }) if bytes.as_ref() == expected => {
                    self.inner.cache.invalidate(key.as_str()).await;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
    }

    fn compare_and_expire<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
        ttl: Duration,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            match self.live(key).await {
                Some(Stored {
                    value: Value::Bytes(bytes),
                    ..
                }) if bytes.as_ref() == expected => {
                    self.put(key, Value::Bytes(bytes), deadline(Some(ttl)))
                        .await;
                    Ok(true)
                }
                _ => Ok(false),
            }
        })
    }

    fn scan<'a>(
        &'a self,
        prefix: &'a Key,
        cursor: ScanCursor,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Key>, ScanCursor)>> {
        Box::pin(async move {
            // A snapshot, sorted, so that paging is stable even though `moka`'s
            // iteration order is not.
            let mut matched: Vec<String> = self
                .inner
                .cache
                .iter()
                .filter(|(key, stored)| key.starts_with(prefix.as_str()) && !stored.is_expired())
                .map(|(key, _)| String::clone(&key))
                .collect();
            matched.sort_unstable();

            let after = cursor.bookmark().map(str::to_owned);
            let start = match &after {
                None => 0,
                Some(bookmark) => matched.partition_point(|key| key <= bookmark),
            };

            let take = usize::try_from(limit.max(1)).unwrap_or(usize::MAX);
            let end = start.saturating_add(take).min(matched.len());

            let page: Vec<Key> = matched[start..end]
                .iter()
                .map(|text| Key::from_raw(text.clone()))
                .collect::<std::result::Result<_, _>>()?;

            let next = match page.last() {
                Some(last) if end < matched.len() => ScanCursor::at(last.as_str()),
                _ => ScanCursor::end(),
            };
            Ok((page, next))
        })
    }

    fn list_push<'a>(
        &'a self,
        key: &'a Key,
        values: &'a [Bytes],
        side: Side,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let (mut items, expires_at) = self.live_list(key, "list_push").await?;
            match side {
                Side::Right => items.extend(values.iter().cloned()),
                // Redis' `LPUSH a b c` leaves `c b a` at the head.
                Side::Left => {
                    for value in values {
                        items.insert(0, value.clone());
                    }
                }
            }
            let len = items.len() as u64;
            self.put(key, Value::List(items), expires_at).await;
            Ok(len)
        })
    }

    fn list_pop<'a>(
        &'a self,
        key: &'a Key,
        side: Side,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            let deadline = timeout.map(|timeout| Instant::now() + timeout);
            loop {
                {
                    let _guard = self.inner.write.lock().await;
                    let (mut items, expires_at) = self.live_list(key, "list_pop").await?;
                    if !items.is_empty() {
                        let popped = match side {
                            Side::Left => items.remove(0),
                            Side::Right => items.pop().unwrap_or_default(),
                        };
                        if items.is_empty() {
                            self.inner.cache.invalidate(key.as_str()).await;
                        } else {
                            self.put(key, Value::List(items), expires_at).await;
                        }
                        return Ok(Some(popped));
                    }
                }

                match deadline {
                    Some(deadline) if Instant::now() < deadline => {
                        // No blocking pop in an in-process map, so poll. The
                        // interval bounds the latency a `list_pop` adds and is
                        // far below any timeout worth setting.
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    _ => return Ok(None),
                }
            }
        })
    }

    fn list_len<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let (items, _) = self.live_list(key, "list_len").await?;
            Ok(items.len() as u64)
        })
    }

    fn set_add<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let (mut items, expires_at) = self.live_set(key, "set_add").await?;
            let mut added = 0_u64;
            for member in members {
                if !items.iter().any(|held| held == member) {
                    items.push(member.clone());
                    added += 1;
                }
            }
            self.put(key, Value::Set(items), expires_at).await;
            Ok(added)
        })
    }

    fn set_remove<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let (mut items, expires_at) = self.live_set(key, "set_remove").await?;
            let before = items.len();
            items.retain(|held| !members.iter().any(|member| member == held));
            let removed = (before - items.len()) as u64;
            if items.is_empty() {
                self.inner.cache.invalidate(key.as_str()).await;
            } else {
                self.put(key, Value::Set(items), expires_at).await;
            }
            Ok(removed)
        })
    }

    fn set_members<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async move {
            let (items, _) = self.live_set(key, "set_members").await?;
            Ok(items)
        })
    }

    fn zadd<'a>(&'a self, key: &'a Key, scored: &'a [(f64, Bytes)]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let (mut items, expires_at) = self.live_zset(key, "zadd").await?;
            let mut added = 0_u64;
            for (score, member) in scored {
                match items.iter_mut().find(|(_, held)| held == member) {
                    // Redis replaces the score of a member that is already in.
                    Some(entry) => entry.0 = *score,
                    None => {
                        items.push((*score, member.clone()));
                        added += 1;
                    }
                }
            }
            sort_zset(&mut items);
            self.put(key, Value::ZSet(items), expires_at).await;
            Ok(added)
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
            let (items, _) = self.live_zset(key, "zrange_by_score").await?;
            Ok(items
                .into_iter()
                .filter(|(score, _)| *score >= lo && *score <= hi)
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .map(|(_, member)| member)
                .collect())
        })
    }

    fn zrem<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let _guard = self.inner.write.lock().await;
            let (mut items, expires_at) = self.live_zset(key, "zrem").await?;
            let before = items.len();
            items.retain(|(_, held)| !members.iter().any(|member| member == held));
            let removed = (before - items.len()) as u64;
            if items.is_empty() {
                self.inner.cache.invalidate(key.as_str()).await;
            } else {
                self.put(key, Value::ZSet(items), expires_at).await;
            }
            Ok(removed)
        })
    }

    fn publish<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let sender = self.channel(channel);
            // `send` fails only when nobody is listening, which is not an error
            // anywhere pubsub exists.
            Ok(sender.send(payload).unwrap_or(0) as u64)
        })
    }

    fn subscribe<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        Box::pin(async move {
            let receiver = self.channel(channel).subscribe();
            let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
                loop {
                    match receiver.recv().await {
                        Ok(payload) => return Some((payload, receiver)),
                        // A slow subscriber loses messages rather than the
                        // subscription; Redis behaves the same way.
                        Err(broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(broadcast::error::RecvError::Closed) => return None,
                    }
                }
            });
            Ok(Box::pin(stream) as MessageStream)
        })
    }
}

/// Sort by score, then by member, so iteration order is deterministic.
fn sort_zset(items: &mut [(f64, Bytes)]) {
    items.sort_by(|(left_score, left), (right_score, right)| {
        left_score
            .partial_cmp(right_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.cmp(right))
    });
}

/// The error Redis returns for `INCR` on something that is not a number.
fn not_an_integer(operation: &'static str) -> Error {
    Error::backend(
        BACKEND,
        operation,
        "the value under this key is not an integer",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str) -> Key {
        Key::from_raw(format!("moso:v1:test:demo:1:{name}")).expect("valid")
    }

    #[tokio::test]
    async fn a_value_round_trips() {
        let store = MemoryStore::new();
        let key = key("a");
        assert!(store.get(&key).await.expect("get").is_none());
        assert!(
            store
                .set(&key, Bytes::from_static(b"v"), SetOpts::new())
                .await
                .expect("set")
        );
        assert_eq!(
            store.get(&key).await.expect("get").as_deref(),
            Some(&b"v"[..])
        );
        assert!(store.exists(&key).await.expect("exists"));
        assert!(store.delete(&key).await.expect("delete"));
        assert!(!store.delete(&key).await.expect("delete"));
    }

    #[tokio::test]
    async fn a_wrong_type_is_reported_rather_than_silently_missing() {
        let store = MemoryStore::new();
        let key = key("list");
        store
            .list_push(&key, &[Bytes::from_static(b"x")], Side::Right)
            .await
            .expect("push");
        let error = store.get(&key).await.expect_err("wrong type");
        assert!(error.chain().contains("WRONGTYPE"), "{error}");
    }

    #[tokio::test]
    async fn ttl_is_visible_immediately_even_before_eviction() {
        let store = MemoryStore::new();
        let key = key("ttl");
        store
            .set(
                &key,
                Bytes::from_static(b"v"),
                SetOpts::new().ttl(Duration::from_millis(30)),
            )
            .await
            .expect("set");
        assert!(store.ttl(&key).await.expect("ttl").is_some());

        tokio::time::sleep(Duration::from_millis(45)).await;
        assert!(store.get(&key).await.expect("get").is_none());
        assert!(!store.exists(&key).await.expect("exists"));
        assert_eq!(store.ttl(&key).await.expect("ttl"), None);
    }

    #[tokio::test]
    async fn conditional_writes_do_what_they_say() {
        let store = MemoryStore::new();
        let key = key("cond");

        assert!(
            !store
                .set(&key, Bytes::from_static(b"a"), SetOpts::new().if_present())
                .await
                .expect("set")
        );
        assert!(
            store
                .set(&key, Bytes::from_static(b"a"), SetOpts::new().if_absent())
                .await
                .expect("set")
        );
        assert!(
            !store
                .set(&key, Bytes::from_static(b"b"), SetOpts::new().if_absent())
                .await
                .expect("set")
        );
        assert_eq!(
            store.get(&key).await.expect("get").as_deref(),
            Some(&b"a"[..])
        );
    }

    #[tokio::test]
    async fn keep_ttl_keeps_the_expiry() {
        let store = MemoryStore::new();
        let key = key("keep");
        store
            .set(
                &key,
                Bytes::from_static(b"a"),
                SetOpts::new().ttl(Duration::from_secs(60)),
            )
            .await
            .expect("set");
        store
            .set(&key, Bytes::from_static(b"b"), SetOpts::new().keep_ttl())
            .await
            .expect("set");
        assert!(store.ttl(&key).await.expect("ttl").is_some());

        // ... and without `keep_ttl`, the expiry goes.
        store
            .set(&key, Bytes::from_static(b"c"), SetOpts::new())
            .await
            .expect("set");
        assert_eq!(store.ttl(&key).await.expect("ttl"), None);
    }

    #[tokio::test]
    async fn incr_creates_at_zero_and_only_sets_a_ttl_then() {
        let store = MemoryStore::new();
        let key = key("count");
        assert_eq!(
            store
                .incr(&key, 1, Some(Duration::from_secs(60)))
                .await
                .expect("incr"),
            1
        );
        let first = store.ttl(&key).await.expect("ttl").expect("a ttl");

        assert_eq!(store.incr(&key, 4, None).await.expect("incr"), 5);
        let second = store.ttl(&key).await.expect("ttl").expect("still a ttl");
        assert!(second <= first, "the window did not slide");

        assert_eq!(store.incr(&key, -5, None).await.expect("incr"), 0);
    }

    #[tokio::test]
    async fn incr_on_a_non_number_is_an_error() {
        let store = MemoryStore::new();
        let key = key("text");
        store
            .set(&key, Bytes::from_static(b"hello"), SetOpts::new())
            .await
            .expect("set");
        assert!(store.incr(&key, 1, None).await.is_err());
    }

    #[tokio::test]
    async fn compare_and_swap_is_a_real_comparison() {
        let store = MemoryStore::new();
        let key = key("cas");

        // Absent -> present.
        assert!(
            store
                .compare_and_swap(&key, None, Bytes::from_static(b"1"), SetOpts::new())
                .await
                .expect("cas")
        );
        // The same swap again fails: it is no longer absent.
        assert!(
            !store
                .compare_and_swap(&key, None, Bytes::from_static(b"2"), SetOpts::new())
                .await
                .expect("cas")
        );
        // The right old value succeeds.
        assert!(
            store
                .compare_and_swap(&key, Some(b"1"), Bytes::from_static(b"2"), SetOpts::new())
                .await
                .expect("cas")
        );
        assert_eq!(
            store.get(&key).await.expect("get").as_deref(),
            Some(&b"2"[..])
        );
    }

    #[tokio::test]
    async fn compare_and_delete_cannot_remove_somebody_elses_value() {
        let store = MemoryStore::new();
        let key = key("lock");
        store
            .set(&key, Bytes::from_static(b"mine"), SetOpts::new())
            .await
            .expect("set");
        assert!(
            !store
                .compare_and_delete(&key, b"theirs")
                .await
                .expect("cad")
        );
        assert!(store.compare_and_delete(&key, b"mine").await.expect("cad"));
        assert!(!store.exists(&key).await.expect("exists"));
    }

    #[tokio::test]
    async fn compare_and_expire_only_renews_our_own_lease() {
        let store = MemoryStore::new();
        let key = key("lease");
        store
            .set(
                &key,
                Bytes::from_static(b"mine"),
                SetOpts::new().ttl(Duration::from_millis(50)),
            )
            .await
            .expect("set");
        assert!(
            !store
                .compare_and_expire(&key, b"theirs", Duration::from_secs(60))
                .await
                .expect("cae")
        );
        assert!(
            store
                .compare_and_expire(&key, b"mine", Duration::from_secs(60))
                .await
                .expect("cae")
        );
        assert!(store.ttl(&key).await.expect("ttl").expect("a ttl") > Duration::from_secs(30));
    }

    #[tokio::test]
    async fn scan_pages_deterministically_and_ends() {
        let store = MemoryStore::new();
        for index in 0..10_u32 {
            store
                .set(&key(&format!("{index:02}")), Bytes::new(), SetOpts::new())
                .await
                .expect("set");
        }
        let prefix = Key::from_raw("moso:v1:test:demo:1:").expect("valid");

        let mut seen = Vec::new();
        let mut cursor = ScanCursor::start();
        let mut rounds = 0;
        loop {
            let (page, next) = store.scan(&prefix, cursor, 3).await.expect("scan");
            seen.extend(page.iter().map(|key| key.parts().to_owned()));
            rounds += 1;
            if next.is_end() {
                break;
            }
            cursor = next;
            assert!(rounds < 20, "the scan did not terminate");
        }

        assert_eq!(seen.len(), 10);
        let mut sorted = seen.clone();
        sorted.sort();
        assert_eq!(seen, sorted, "pages come out in order");
    }

    #[tokio::test]
    async fn delete_prefix_only_removes_its_own_prefix() {
        let store = MemoryStore::new();
        store
            .set(&key("a"), Bytes::new(), SetOpts::new())
            .await
            .expect("set");
        store
            .set(&key("b"), Bytes::new(), SetOpts::new())
            .await
            .expect("set");
        let other = Key::from_raw("moso:v1:test:other:1:a").expect("valid");
        store
            .set(&other, Bytes::new(), SetOpts::new())
            .await
            .expect("set");

        let prefix = Key::from_raw("moso:v1:test:demo:1:").expect("valid");
        assert_eq!(store.delete_prefix(&prefix).await.expect("prefix"), 2);
        assert!(store.exists(&other).await.expect("exists"));
    }

    #[tokio::test]
    async fn lists_behave_like_redis_lists() {
        let store = MemoryStore::new();
        let key = key("q");

        assert_eq!(
            store
                .list_push(
                    &key,
                    &[Bytes::from_static(b"b"), Bytes::from_static(b"c")],
                    Side::Right
                )
                .await
                .expect("push"),
            2
        );
        assert_eq!(
            store
                .list_push(&key, &[Bytes::from_static(b"a")], Side::Left)
                .await
                .expect("push"),
            3
        );
        assert_eq!(store.list_len(&key).await.expect("len"), 3);

        assert_eq!(
            store.list_pop(&key, Side::Left, None).await.expect("pop"),
            Some(Bytes::from_static(b"a"))
        );
        assert_eq!(
            store.list_pop(&key, Side::Right, None).await.expect("pop"),
            Some(Bytes::from_static(b"c"))
        );
        assert_eq!(
            store.list_pop(&key, Side::Left, None).await.expect("pop"),
            Some(Bytes::from_static(b"b"))
        );
        assert_eq!(
            store.list_pop(&key, Side::Left, None).await.expect("pop"),
            None
        );
        assert!(!store.exists(&key).await.expect("exists"));
    }

    #[tokio::test]
    async fn a_blocking_pop_waits_and_then_gives_up() {
        let store = MemoryStore::new();
        let key = key("blocking");
        let started = Instant::now();
        assert_eq!(
            store
                .list_pop(&key, Side::Left, Some(Duration::from_millis(40)))
                .await
                .expect("pop"),
            None
        );
        assert!(started.elapsed() >= Duration::from_millis(35));
    }

    #[tokio::test]
    async fn sets_deduplicate() {
        let store = MemoryStore::new();
        let key = key("s");
        let members = [Bytes::from_static(b"a"), Bytes::from_static(b"b")];
        assert_eq!(store.set_add(&key, &members).await.expect("add"), 2);
        assert_eq!(store.set_add(&key, &members).await.expect("add"), 0);
        assert_eq!(store.set_members(&key).await.expect("members").len(), 2);
        assert_eq!(
            store
                .set_remove(&key, &[Bytes::from_static(b"a")])
                .await
                .expect("remove"),
            1
        );
        assert_eq!(store.set_members(&key).await.expect("members").len(), 1);
    }

    #[tokio::test]
    async fn sorted_sets_range_by_score_and_replace_scores() {
        let store = MemoryStore::new();
        let key = key("z");
        assert_eq!(
            store
                .zadd(
                    &key,
                    &[
                        (2.0, Bytes::from_static(b"b")),
                        (1.0, Bytes::from_static(b"a")),
                        (3.0, Bytes::from_static(b"c")),
                    ]
                )
                .await
                .expect("zadd"),
            3
        );
        assert_eq!(
            store
                .zrange_by_score(&key, 1.0, 2.5, 10)
                .await
                .expect("zrange"),
            vec![Bytes::from_static(b"a"), Bytes::from_static(b"b")]
        );

        // Re-adding a member replaces its score and adds nothing.
        assert_eq!(
            store
                .zadd(&key, &[(0.0, Bytes::from_static(b"c"))])
                .await
                .expect("zadd"),
            0
        );
        assert_eq!(
            store
                .zrange_by_score(&key, 0.0, 0.5, 10)
                .await
                .expect("zrange"),
            vec![Bytes::from_static(b"c")]
        );
        assert_eq!(
            store
                .zrem(&key, &[Bytes::from_static(b"c")])
                .await
                .expect("zrem"),
            1
        );
    }

    #[tokio::test]
    async fn pubsub_reaches_a_subscriber_in_this_process() {
        use futures_util::StreamExt as _;

        let store = MemoryStore::new();
        let mut stream = store.subscribe("events").await.expect("subscribe");

        // The subscription exists before the publish, which is the only
        // ordering pubsub guarantees anywhere.
        assert_eq!(
            store
                .publish("events", Bytes::from_static(b"hello"))
                .await
                .expect("publish"),
            1
        );
        assert_eq!(stream.next().await, Some(Bytes::from_static(b"hello")));
    }

    #[tokio::test]
    async fn publishing_to_nobody_is_not_an_error() {
        let store = MemoryStore::new();
        assert_eq!(
            store.publish("quiet", Bytes::new()).await.expect("publish"),
            0
        );
    }

    #[tokio::test]
    async fn the_capacity_is_a_bound_and_not_a_suggestion() {
        let store = MemoryStore::with_capacity(4);
        assert_eq!(store.capacity(), 4);
        for index in 0..64_u32 {
            store
                .set(&key(&format!("{index}")), Bytes::new(), SetOpts::new())
                .await
                .expect("set");
        }
        store.inner.cache.run_pending_tasks().await;
        assert!(store.len() <= 4, "held {} entries", store.len());
    }

    #[tokio::test]
    async fn clear_forgets_everything() {
        let store = MemoryStore::new();
        store
            .set(&key("a"), Bytes::new(), SetOpts::new())
            .await
            .expect("set");
        store.clear().await;
        assert!(store.is_empty());
        assert!(!store.exists(&key("a")).await.expect("exists"));
    }

    #[test]
    fn it_says_what_it_is() {
        let store = MemoryStore::new();
        assert_eq!(store.name(), "memory");
        assert_eq!(store.capabilities(), Capabilities::memory());
        assert!(format!("{store:?}").starts_with("MemoryStore"));
    }
}
