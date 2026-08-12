//! [`KvStore`] — the backend trait, and the vocabulary it speaks.
//!
//! One trait, three implementations, and a [`Capabilities`] struct that says
//! which of the optional halves an implementation has. That last part is what
//! makes the trait honest: rather than every backend pretending to be Redis,
//! a backend says what it can do and the layer above degrades on purpose.
//!
//! # Dyn-compatible, so boxed futures
//!
//! `Kv` holds an `Arc<dyn KvStore>` — the backend is chosen by configuration,
//! not by a type parameter, which is the whole point of "the same code runs
//! against an in-process map in tests and Redis in production". A dyn-compatible
//! trait cannot have `async fn`, so every method returns a
//! [`BoxFuture`]. Decision D4 in the build contract, and the same shape as
//! [`HealthCheck`](moso_core::HealthCheck).
//!
//! # The optional half
//!
//! Ten of the twenty-six operations are required. Sixteen have a default body
//! that returns [`Error::Unsupported`], and three more — `get_many`, `set_many`
//! and `delete_prefix` — have a default that *works*, built out of the required
//! ones, so a backend overrides them only to be faster. A backend implements
//! what it has and reports it in [`capabilities`](KvStore::capabilities);
//! calling one that is absent is a programmer error rather than a silent wrong
//! answer.
//!
//! | Capability | Methods |
//! | --- | --- |
//! | `structures` | `list_push`, `list_pop`, `set_add`, `set_members`, `zadd`, `zrange_by_score` |
//! | `pubsub` | `publish`, `subscribe` |
//! | `scripting` | `eval` |
//! | `atomic_cas` | `compare_and_swap`, `compare_and_delete`, `compare_and_expire` |
//! | `scan` | `scan`, `delete_prefix` |

use std::fmt;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::Stream;
use moso_core::BoxFuture;
use moso_core::HealthStatus;

use crate::error::{Error, Result};
use crate::key::Key;

/// A stream of messages from [`KvStore::subscribe`].
///
/// `'static` because a subscription outlives the call that created it: the
/// stream owns its connection or its channel receiver.
///
/// ```
/// use moso_kv::MessageStream;
/// use futures_util::StreamExt as _;
///
/// # async fn example(mut stream: MessageStream) {
/// while let Some(payload) = stream.next().await {
///     println!("{} bytes", payload.len());
/// }
/// # }
/// ```
pub type MessageStream = Pin<Box<dyn Stream<Item = Bytes> + Send + 'static>>;

// ---------------------------------------------------------------------------
// SetOpts
// ---------------------------------------------------------------------------

/// How a [`set`](KvStore::set) should behave.
///
/// The default is the unconditional, no-expiry write. Everything else is one
/// builder call, and the combinations that make no sense are rejected by
/// [`SetOpts::validate`] rather than being silently resolved one way.
///
/// ```
/// use moso_kv::SetOpts;
/// use std::time::Duration;
///
/// // The write a cache does.
/// let opts = SetOpts::new().ttl(Duration::from_secs(300));
/// assert_eq!(opts.ttl, Some(Duration::from_secs(300)));
///
/// // The write a lock does: `SET key value NX PX 30000`.
/// let lock = SetOpts::new().if_absent().ttl(Duration::from_secs(30));
/// assert!(lock.if_absent);
/// assert!(lock.validate().is_ok());
///
/// // Both conditions at once can never apply.
/// assert!(SetOpts::new().if_absent().if_present().validate().is_err());
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SetOpts {
    /// Expire the key after this long. `None` means "no expiry".
    pub ttl: Option<Duration>,
    /// Only write when the key does not exist (Redis `NX`).
    pub if_absent: bool,
    /// Only write when the key already exists (Redis `XX`).
    pub if_present: bool,
    /// Leave the existing expiry alone (Redis `KEEPTTL`).
    pub keep_ttl: bool,
}

impl SetOpts {
    /// An unconditional write with no expiry.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    ///
    /// assert_eq!(SetOpts::new(), SetOpts::default());
    /// ```
    #[must_use]
    pub const fn new() -> Self {
        Self {
            ttl: None,
            if_absent: false,
            if_present: false,
            keep_ttl: false,
        }
    }

    /// Expire after `ttl`.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    /// use std::time::Duration;
    ///
    /// assert_eq!(SetOpts::new().ttl(Duration::from_secs(60)).ttl, Some(Duration::from_secs(60)));
    /// ```
    #[must_use]
    pub const fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Expire after `ttl` when there is one.
    ///
    /// The form a namespace uses, since [`Namespace::TTL`](crate::Namespace::TTL)
    /// is an `Option`.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    ///
    /// assert_eq!(SetOpts::new().maybe_ttl(None).ttl, None);
    /// ```
    #[must_use]
    pub const fn maybe_ttl(mut self, ttl: Option<Duration>) -> Self {
        self.ttl = ttl;
        self
    }

    /// Only write when the key is absent.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    ///
    /// assert!(SetOpts::new().if_absent().if_absent);
    /// ```
    #[must_use]
    pub const fn if_absent(mut self) -> Self {
        self.if_absent = true;
        self
    }

    /// Only write when the key is present.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    ///
    /// assert!(SetOpts::new().if_present().if_present);
    /// ```
    #[must_use]
    pub const fn if_present(mut self) -> Self {
        self.if_present = true;
        self
    }

    /// Keep whatever expiry the key already had.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    ///
    /// assert!(SetOpts::new().keep_ttl().keep_ttl);
    /// ```
    #[must_use]
    pub const fn keep_ttl(mut self) -> Self {
        self.keep_ttl = true;
        self
    }

    /// Reject combinations that cannot both hold.
    ///
    /// # Errors
    ///
    /// [`Error::Config`] when `if_absent` and `if_present` are both set, or
    /// when `keep_ttl` and `ttl` are, since Redis rejects `KEEPTTL` alongside
    /// `PX` and silently preferring one would make the same code behave
    /// differently on two backends.
    ///
    /// ```
    /// use moso_kv::SetOpts;
    /// use std::time::Duration;
    ///
    /// assert!(SetOpts::new().validate().is_ok());
    /// assert!(SetOpts::new().keep_ttl().ttl(Duration::from_secs(1)).validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.if_absent && self.if_present {
            return Err(Error::Config {
                detail: "a set cannot be both `if_absent` and `if_present`".to_owned(),
            });
        }
        if self.keep_ttl && self.ttl.is_some() {
            return Err(Error::Config {
                detail: "a set cannot both `keep_ttl` and set a new `ttl`".to_owned(),
            });
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Side
// ---------------------------------------------------------------------------

/// Which end of a list.
///
/// ```
/// use moso_kv::Side;
///
/// // A queue: push right, pop left.
/// assert_eq!(Side::Right.opposite(), Side::Left);
/// assert_eq!(Side::Left.as_str(), "left");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// The head — Redis' `LPUSH`/`LPOP`.
    Left,
    /// The tail — Redis' `RPUSH`/`RPOP`.
    Right,
}

impl Side {
    /// The other end.
    ///
    /// ```
    /// use moso_kv::Side;
    ///
    /// assert_eq!(Side::Left.opposite(), Side::Right);
    /// ```
    #[must_use]
    pub const fn opposite(self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }

    /// The name used in logs and errors.
    ///
    /// ```
    /// use moso_kv::Side;
    ///
    /// assert_eq!(Side::Right.as_str(), "right");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Side::Left => "left",
            Side::Right => "right",
        }
    }
}

impl fmt::Display for Side {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// ScanCursor
// ---------------------------------------------------------------------------

/// Where a [`scan`](KvStore::scan) left off.
///
/// Opaque, because the three backends mean three different things by it: a
/// Redis `SCAN` cursor, a PostgreSQL `WHERE key > $1` bookmark, and an index
/// into a snapshot for the memory backend. Named `ScanCursor` rather than
/// `Cursor` so it cannot be confused with
/// [`moso_schema::types::Cursor`], which is the HTTP pagination one.
///
/// ```
/// use moso_kv::ScanCursor;
///
/// // A scan starts at the beginning and ends when the cursor is exhausted.
/// let start = ScanCursor::start();
/// assert!(start.is_start());
/// assert!(!start.is_end());
///
/// assert!(ScanCursor::end().is_end());
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScanCursor(CursorRepr);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
enum CursorRepr {
    /// Nothing scanned yet.
    #[default]
    Start,
    /// The backend's own bookmark.
    At(String),
    /// Nothing left.
    End,
}

impl ScanCursor {
    /// The cursor a scan begins with.
    ///
    /// ```
    /// use moso_kv::ScanCursor;
    ///
    /// assert_eq!(ScanCursor::start(), ScanCursor::default());
    /// ```
    #[must_use]
    pub fn start() -> Self {
        Self(CursorRepr::Start)
    }

    /// The cursor a finished scan returns.
    ///
    /// ```
    /// use moso_kv::ScanCursor;
    ///
    /// assert!(ScanCursor::end().is_end());
    /// ```
    #[must_use]
    pub fn end() -> Self {
        Self(CursorRepr::End)
    }

    /// A backend's own bookmark.
    ///
    /// ```
    /// use moso_kv::ScanCursor;
    ///
    /// let cursor = ScanCursor::at("4096");
    /// assert_eq!(cursor.bookmark(), Some("4096"));
    /// ```
    #[must_use]
    pub fn at(bookmark: impl Into<String>) -> Self {
        Self(CursorRepr::At(bookmark.into()))
    }

    /// Whether nothing has been scanned yet.
    ///
    /// ```
    /// use moso_kv::ScanCursor;
    ///
    /// assert!(ScanCursor::start().is_start());
    /// assert!(!ScanCursor::at("1").is_start());
    /// ```
    #[must_use]
    pub fn is_start(&self) -> bool {
        matches!(self.0, CursorRepr::Start)
    }

    /// Whether the scan is complete.
    ///
    /// The loop condition: keep calling `scan` until this is `true`.
    ///
    /// ```
    /// use moso_kv::ScanCursor;
    ///
    /// assert!(ScanCursor::end().is_end());
    /// assert!(!ScanCursor::start().is_end());
    /// ```
    #[must_use]
    pub fn is_end(&self) -> bool {
        matches!(self.0, CursorRepr::End)
    }

    /// The backend's bookmark, when there is one.
    ///
    /// ```
    /// use moso_kv::ScanCursor;
    ///
    /// assert_eq!(ScanCursor::start().bookmark(), None);
    /// assert_eq!(ScanCursor::at("k").bookmark(), Some("k"));
    /// ```
    #[must_use]
    pub fn bookmark(&self) -> Option<&str> {
        match &self.0 {
            CursorRepr::At(value) => Some(value),
            CursorRepr::Start | CursorRepr::End => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// What a backend can do.
///
/// This is how a battery degrades on purpose: `moso-jobs` uses list operations
/// when `structures` is set and falls back to a PostgreSQL queue when it is
/// not; the rate limiter uses a script when `scripting` is set and a
/// compare-and-swap loop when it is not.
///
/// ```
/// use moso_kv::Capabilities;
///
/// let memory = Capabilities::memory();
/// assert!(memory.structures);
/// assert!(memory.pubsub);
/// // ... but only within this process, which is the one semantic difference
/// // the memory backend cannot paper over.
/// assert!(!memory.pubsub_cross_process);
/// assert!(!memory.persistence);
///
/// let redis = Capabilities::redis();
/// assert!(redis.scripting);
/// assert!(redis.pubsub_cross_process);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct Capabilities {
    /// [`publish`](KvStore::publish) and [`subscribe`](KvStore::subscribe) work.
    pub pubsub: bool,
    /// A subscriber in *another process* sees a publish from this one.
    ///
    /// Not in the design document's list, and added because the memory backend
    /// needs to say something true: it has working pubsub, and it is
    /// process-local. A test that needs two processes to talk gates on this
    /// field rather than on [`pubsub`](Self::pubsub).
    pub pubsub_cross_process: bool,
    /// Lists, sets and sorted sets work.
    pub structures: bool,
    /// [`scan`](KvStore::scan) and [`delete_prefix`](KvStore::delete_prefix)
    /// work.
    pub scan: bool,
    /// The three compare-and-swap operations work.
    pub atomic_cas: bool,
    /// [`eval`](KvStore::eval) works.
    pub scripting: bool,
    /// Data survives a restart of the store.
    pub persistence: bool,
}

impl Capabilities {
    /// A backend that can do nothing optional — the starting point for a custom
    /// implementation.
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// let caps = Capabilities::none().with_scan(true);
    /// assert!(caps.scan);
    /// assert!(!caps.pubsub);
    /// ```
    #[must_use]
    pub const fn none() -> Self {
        Self {
            pubsub: false,
            pubsub_cross_process: false,
            structures: false,
            scan: false,
            atomic_cas: false,
            scripting: false,
            persistence: false,
        }
    }

    /// What the in-process backend can do.
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::memory().atomic_cas);
    /// ```
    #[must_use]
    pub const fn memory() -> Self {
        Self {
            pubsub: true,
            pubsub_cross_process: false,
            structures: true,
            scan: true,
            atomic_cas: true,
            scripting: false,
            persistence: false,
        }
    }

    /// What the Redis backend can do.
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::redis().scripting);
    /// ```
    #[must_use]
    pub const fn redis() -> Self {
        Self {
            pubsub: true,
            pubsub_cross_process: true,
            structures: true,
            scan: true,
            atomic_cas: true,
            scripting: true,
            persistence: true,
        }
    }

    /// What the PostgreSQL backend can do.
    ///
    /// No scripting: the equivalent would be a `PL/pgSQL` function, which means
    /// a migration, which means the cache layer would own a schema object it
    /// cannot create without permissions it should not need.
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::postgres().persistence);
    /// assert!(!Capabilities::postgres().scripting);
    /// ```
    #[must_use]
    pub const fn postgres() -> Self {
        Self {
            pubsub: true,
            pubsub_cross_process: true,
            structures: true,
            scan: true,
            atomic_cas: true,
            scripting: false,
            persistence: true,
        }
    }

    /// Set [`pubsub`](Self::pubsub).
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::none().with_pubsub(true, false).pubsub);
    /// ```
    #[must_use]
    pub const fn with_pubsub(mut self, enabled: bool, cross_process: bool) -> Self {
        self.pubsub = enabled;
        self.pubsub_cross_process = cross_process;
        self
    }

    /// Set [`structures`](Self::structures).
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::none().with_structures(true).structures);
    /// ```
    #[must_use]
    pub const fn with_structures(mut self, enabled: bool) -> Self {
        self.structures = enabled;
        self
    }

    /// Set [`scan`](Self::scan).
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::none().with_scan(true).scan);
    /// ```
    #[must_use]
    pub const fn with_scan(mut self, enabled: bool) -> Self {
        self.scan = enabled;
        self
    }

    /// Set [`atomic_cas`](Self::atomic_cas).
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::none().with_atomic_cas(true).atomic_cas);
    /// ```
    #[must_use]
    pub const fn with_atomic_cas(mut self, enabled: bool) -> Self {
        self.atomic_cas = enabled;
        self
    }

    /// Set [`scripting`](Self::scripting).
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::none().with_scripting(true).scripting);
    /// ```
    #[must_use]
    pub const fn with_scripting(mut self, enabled: bool) -> Self {
        self.scripting = enabled;
        self
    }

    /// Set [`persistence`](Self::persistence).
    ///
    /// ```
    /// use moso_kv::Capabilities;
    ///
    /// assert!(Capabilities::none().with_persistence(true).persistence);
    /// ```
    #[must_use]
    pub const fn with_persistence(mut self, enabled: bool) -> Self {
        self.persistence = enabled;
        self
    }
}

// ---------------------------------------------------------------------------
// KvStore
// ---------------------------------------------------------------------------

/// A key-value backend.
///
/// Implement it to add a backend — DynamoDB and Cloudflare KV are natural fits.
/// The optional methods have default bodies that return
/// [`Error::Unsupported`], so a minimal implementation is the seven core
/// operations plus [`capabilities`](Self::capabilities),
/// [`health`](Self::health) and [`name`](Self::name) — ten in total, and the
/// `NullStore` below is all ten.
///
/// ```
/// use bytes::Bytes;
/// use moso_core::{BoxFuture, HealthStatus};
/// use moso_kv::{Capabilities, Key, KvStore, Result, ScanCursor, SetOpts};
/// use std::time::Duration;
///
/// /// A store that forgets everything — the null object, useful in tests
/// /// that must prove a code path never reads the cache.
/// #[derive(Debug, Default)]
/// pub struct NullStore;
///
/// impl KvStore for NullStore {
///     fn name(&self) -> &'static str { "null" }
///     fn capabilities(&self) -> Capabilities { Capabilities::none() }
///
///     fn health(&self) -> BoxFuture<'_, HealthStatus> {
///         Box::pin(async { HealthStatus::Up })
///     }
///
///     fn get<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
///         Box::pin(async { Ok(None) })
///     }
///     fn set<'a>(&'a self, _k: &'a Key, _v: Bytes, _o: SetOpts) -> BoxFuture<'a, Result<bool>> {
///         Box::pin(async { Ok(true) })
///     }
///     fn delete<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
///         Box::pin(async { Ok(false) })
///     }
///     fn exists<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
///         Box::pin(async { Ok(false) })
///     }
///     fn expire<'a>(&'a self, _key: &'a Key, _ttl: Duration) -> BoxFuture<'a, Result<bool>> {
///         Box::pin(async { Ok(false) })
///     }
///     fn ttl<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
///         Box::pin(async { Ok(None) })
///     }
///     fn incr<'a>(
///         &'a self,
///         _key: &'a Key,
///         by: i64,
///         _ttl: Option<Duration>,
///     ) -> BoxFuture<'a, Result<i64>> {
///         Box::pin(async move { Ok(by) })
///     }
/// }
///
/// // And it is usable as the trait object `Kv` holds.
/// let store: std::sync::Arc<dyn KvStore> = std::sync::Arc::new(NullStore);
/// assert_eq!(store.name(), "null");
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a key-value backend",
    label = "this type has no `KvStore` impl",
    note = "implement the ten required methods: `name`, `capabilities`, `health`, `get`, \
            `set`, `delete`, `exists`, `expire`, `ttl` and `incr`",
    note = "the other sixteen are optional and default to `Error::Unsupported`; say which ones \
            you have in `capabilities()`",
    note = "help: let kv = Kv::builder(\"my-app\").store({Self}::new()).build();"
)]
pub trait KvStore: Send + Sync + 'static {
    // ── identity ──────────────────────────────────────────────────────────

    /// The backend's name, as it appears in errors, logs and metric labels.
    ///
    /// A `&'static str` so a log field and a metric label cannot disagree.
    fn name(&self) -> &'static str;

    /// What this backend can do.
    fn capabilities(&self) -> Capabilities;

    /// Whether the backend is reachable right now.
    ///
    /// Called by [`KvHealthCheck`](crate::KvHealthCheck) on every `/readyz`
    /// probe, so it must be one cheap round trip and nothing more.
    fn health(&self) -> BoxFuture<'_, HealthStatus>;

    // ── core ──────────────────────────────────────────────────────────────

    /// Read one value.
    fn get<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>>;

    /// Write one value, subject to `opts`.
    ///
    /// Returns whether the write applied: `false` means a conditional set was
    /// declined, not that anything failed.
    fn set<'a>(&'a self, key: &'a Key, value: Bytes, opts: SetOpts) -> BoxFuture<'a, Result<bool>>;

    /// Remove one key. Returns whether it was there.
    fn delete<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>>;

    /// Whether the key exists and has not expired.
    fn exists<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>>;

    /// Give an existing key a new expiry. Returns whether it was there.
    fn expire<'a>(&'a self, key: &'a Key, ttl: Duration) -> BoxFuture<'a, Result<bool>>;

    /// How long until the key expires. `Ok(None)` for an absent key *or* one
    /// with no expiry — callers that need to distinguish those use
    /// [`exists`](Self::exists).
    fn ttl<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>>;

    // ── atomics ───────────────────────────────────────────────────────────

    /// Add `by` to a counter, creating it at zero first, and return the new
    /// value.
    ///
    /// `ttl` is applied only when the key is created, which is what makes a
    /// fixed-window counter possible in one round trip.
    fn incr<'a>(
        &'a self,
        key: &'a Key,
        by: i64,
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>>;

    /// Write `new` only if the current value is exactly `old`.
    ///
    /// `old = None` means "only if the key is absent". Returns whether the swap
    /// happened. Part of the `atomic_cas` capability.
    fn compare_and_swap<'a>(
        &'a self,
        key: &'a Key,
        old: Option<&'a [u8]>,
        new: Bytes,
        opts: SetOpts,
    ) -> BoxFuture<'a, Result<bool>> {
        let _ = (key, old, new, opts);
        unsupported(self.name(), "compare_and_swap", "atomic_cas")
    }

    /// Delete `key` only if its value is exactly `expected`.
    ///
    /// What releasing a lock is: a delete that cannot remove somebody else's
    /// lock after our own lease expired. Part of the `atomic_cas` capability.
    fn compare_and_delete<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
    ) -> BoxFuture<'a, Result<bool>> {
        let _ = (key, expected);
        unsupported(self.name(), "compare_and_delete", "atomic_cas")
    }

    /// Extend `key`'s expiry only if its value is exactly `expected`.
    ///
    /// What renewing a lock lease is. Part of the `atomic_cas` capability.
    fn compare_and_expire<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
        ttl: Duration,
    ) -> BoxFuture<'a, Result<bool>> {
        let _ = (key, expected, ttl);
        unsupported(self.name(), "compare_and_expire", "atomic_cas")
    }

    // ── bulk ──────────────────────────────────────────────────────────────

    /// Read many values, in the order asked for.
    ///
    /// The default is a loop, which is correct everywhere and optimal nowhere;
    /// a backend with a pipeline overrides it.
    fn get_many<'a>(&'a self, keys: &'a [Key]) -> BoxFuture<'a, Result<Vec<Option<Bytes>>>> {
        Box::pin(async move {
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                out.push(self.get(key).await?);
            }
            Ok(out)
        })
    }

    /// Write many values with the same options.
    fn set_many<'a>(
        &'a self,
        items: &'a [(Key, Bytes)],
        opts: SetOpts,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            for (key, value) in items {
                self.set(key, value.clone(), opts).await?;
            }
            Ok(())
        })
    }

    /// Remove every key under `prefix`, returning how many went.
    ///
    /// Part of the `scan` capability: the default implementation is a `scan`
    /// loop, so a backend that has `scan` gets this one for free.
    fn delete_prefix<'a>(&'a self, prefix: &'a Key) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if !self.capabilities().scan {
                return Err(Error::unsupported(self.name(), "delete_prefix", "scan"));
            }
            let mut cursor = ScanCursor::start();
            let mut removed = 0_u64;
            loop {
                let (keys, next) = self.scan(prefix, cursor, 512).await?;
                for key in &keys {
                    if self.delete(key).await? {
                        removed += 1;
                    }
                }
                if next.is_end() {
                    return Ok(removed);
                }
                cursor = next;
            }
        })
    }

    /// One page of the keys under `prefix`.
    ///
    /// `limit` is a hint: a backend may return fewer, including zero, and the
    /// scan is only over when the returned cursor
    /// [`is_end`](ScanCursor::is_end). That is Redis' `SCAN` contract, and
    /// pretending otherwise would make correct code on the memory backend
    /// break in production.
    fn scan<'a>(
        &'a self,
        prefix: &'a Key,
        cursor: ScanCursor,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Key>, ScanCursor)>> {
        let _ = (prefix, cursor, limit);
        unsupported(self.name(), "scan", "scan")
    }

    // ── structures ────────────────────────────────────────────────────────

    /// Push values onto one end of a list, returning the new length.
    fn list_push<'a>(
        &'a self,
        key: &'a Key,
        values: &'a [Bytes],
        side: Side,
    ) -> BoxFuture<'a, Result<u64>> {
        let _ = (key, values, side);
        unsupported(self.name(), "list_push", "structures")
    }

    /// Pop one value off one end of a list.
    ///
    /// `timeout` asks the backend to block for up to that long rather than
    /// returning `None` immediately; a backend without a blocking pop polls.
    fn list_pop<'a>(
        &'a self,
        key: &'a Key,
        side: Side,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<Option<Bytes>>> {
        let _ = (key, side, timeout);
        unsupported(self.name(), "list_pop", "structures")
    }

    /// How long a list is. `Ok(0)` for an absent key.
    fn list_len<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<u64>> {
        let _ = key;
        unsupported(self.name(), "list_len", "structures")
    }

    /// Add members to a set, returning how many were new.
    fn set_add<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        let _ = (key, members);
        unsupported(self.name(), "set_add", "structures")
    }

    /// Remove members from a set, returning how many were there.
    fn set_remove<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        let _ = (key, members);
        unsupported(self.name(), "set_remove", "structures")
    }

    /// Every member of a set, in an unspecified order.
    fn set_members<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        let _ = key;
        unsupported(self.name(), "set_members", "structures")
    }

    /// Add scored members to a sorted set, returning how many were new.
    ///
    /// A member that is already present has its score replaced, which is
    /// Redis' `ZADD` behaviour and the one a scheduled-job queue relies on.
    fn zadd<'a>(&'a self, key: &'a Key, scored: &'a [(f64, Bytes)]) -> BoxFuture<'a, Result<u64>> {
        let _ = (key, scored);
        unsupported(self.name(), "zadd", "structures")
    }

    /// Members whose score is in `[lo, hi]`, lowest first, at most `limit`.
    fn zrange_by_score<'a>(
        &'a self,
        key: &'a Key,
        lo: f64,
        hi: f64,
        limit: u32,
    ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        let _ = (key, lo, hi, limit);
        unsupported(self.name(), "zrange_by_score", "structures")
    }

    /// Remove members from a sorted set, returning how many were there.
    fn zrem<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        let _ = (key, members);
        unsupported(self.name(), "zrem", "structures")
    }

    // ── pubsub ────────────────────────────────────────────────────────────

    /// Publish `payload` to `channel`, returning how many subscribers were
    /// reached — `0` when the backend cannot tell.
    fn publish<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>> {
        let _ = (channel, payload);
        unsupported(self.name(), "publish", "pubsub")
    }

    /// Subscribe to `channel`.
    ///
    /// The stream ends when the subscription is dropped or the backend
    /// disconnects for good; a reconnecting backend keeps it open across a
    /// reconnect and may drop the messages that happened in between, because
    /// pubsub is at-most-once everywhere it exists.
    fn subscribe<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        let _ = channel;
        unsupported(self.name(), "subscribe", "pubsub")
    }

    // ── scripting ─────────────────────────────────────────────────────────

    /// Run a server-side script over `keys` and `args`, returning its result as
    /// an integer.
    ///
    /// Deliberately narrow: the only thing Moso runs server-side is the rate
    /// limiter, whose result is three integers, and a general-purpose scripting
    /// API would be an unsealed Redis dependency in the public surface.
    fn eval<'a>(
        &'a self,
        script: &'a str,
        keys: &'a [Key],
        args: &'a [Bytes],
    ) -> BoxFuture<'a, Result<Vec<i64>>> {
        let _ = (script, keys, args);
        unsupported(self.name(), "eval", "scripting")
    }
}

/// The body every optional method's default shares.
fn unsupported<'a, T: 'a>(
    backend: &'static str,
    operation: &'static str,
    capability: &'static str,
) -> BoxFuture<'a, Result<T>> {
    Box::pin(async move { Err(Error::unsupported(backend, operation, capability)) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// The minimum a backend has to write.
    #[derive(Debug)]
    struct Minimal;

    impl KvStore for Minimal {
        fn name(&self) -> &'static str {
            "minimal"
        }
        fn capabilities(&self) -> Capabilities {
            Capabilities::none()
        }
        fn health(&self) -> BoxFuture<'_, HealthStatus> {
            Box::pin(async { HealthStatus::Up })
        }
        fn get<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
            Box::pin(async { Ok(None) })
        }
        fn set<'a>(
            &'a self,
            _key: &'a Key,
            _value: Bytes,
            _opts: SetOpts,
        ) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(true) })
        }
        fn delete<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }
        fn exists<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }
        fn expire<'a>(&'a self, _key: &'a Key, _ttl: Duration) -> BoxFuture<'a, Result<bool>> {
            Box::pin(async { Ok(false) })
        }
        fn ttl<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
            Box::pin(async { Ok(None) })
        }
        fn incr<'a>(
            &'a self,
            _key: &'a Key,
            by: i64,
            _ttl: Option<Duration>,
        ) -> BoxFuture<'a, Result<i64>> {
            Box::pin(async move { Ok(by) })
        }
    }

    fn key() -> Key {
        Key::from_raw("moso:v1:a:b:1:c").expect("valid")
    }

    #[test]
    fn the_trait_is_dyn_compatible() {
        let store: Arc<dyn KvStore> = Arc::new(Minimal);
        assert_eq!(store.name(), "minimal");
    }

    #[tokio::test]
    async fn every_optional_method_defaults_to_unsupported() {
        let store = Minimal;
        let key = key();

        macro_rules! check {
            ($call:expr, $operation:literal, $capability:literal) => {{
                let error = $call.await.expect_err(concat!($operation, " is optional"));
                match error {
                    Error::Unsupported {
                        backend,
                        operation,
                        capability,
                    } => {
                        assert_eq!(backend, "minimal");
                        assert_eq!(operation, $operation);
                        assert_eq!(capability, $capability);
                    }
                    other => panic!("{other}"),
                }
            }};
        }

        check!(
            store.compare_and_swap(&key, None, Bytes::new(), SetOpts::new()),
            "compare_and_swap",
            "atomic_cas"
        );
        check!(
            store.compare_and_delete(&key, b""),
            "compare_and_delete",
            "atomic_cas"
        );
        check!(
            store.compare_and_expire(&key, b"", Duration::from_secs(1)),
            "compare_and_expire",
            "atomic_cas"
        );
        check!(store.scan(&key, ScanCursor::start(), 10), "scan", "scan");
        check!(
            store.list_push(&key, &[], Side::Right),
            "list_push",
            "structures"
        );
        check!(
            store.list_pop(&key, Side::Left, None),
            "list_pop",
            "structures"
        );
        check!(store.list_len(&key), "list_len", "structures");
        check!(store.set_add(&key, &[]), "set_add", "structures");
        check!(store.set_remove(&key, &[]), "set_remove", "structures");
        check!(store.set_members(&key), "set_members", "structures");
        check!(store.zadd(&key, &[]), "zadd", "structures");
        check!(
            store.zrange_by_score(&key, 0.0, 1.0, 10),
            "zrange_by_score",
            "structures"
        );
        check!(store.zrem(&key, &[]), "zrem", "structures");
        check!(store.publish("c", Bytes::new()), "publish", "pubsub");
        check!(store.eval("return 1", &[], &[]), "eval", "scripting");

        // `MessageStream` is not `Debug`, so `expect_err` cannot be used here.
        match store.subscribe("c").await {
            Err(Error::Unsupported {
                operation: "subscribe",
                capability: "pubsub",
                ..
            }) => {}
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("subscribe is optional and this backend has no pubsub"),
        }
    }

    #[tokio::test]
    async fn delete_prefix_says_which_capability_it_needs() {
        let error = Minimal
            .delete_prefix(&key())
            .await
            .expect_err("scan is optional");
        assert!(matches!(
            error,
            Error::Unsupported {
                operation: "delete_prefix",
                capability: "scan",
                ..
            }
        ));
    }

    #[tokio::test]
    async fn the_bulk_defaults_are_the_single_operations_in_a_loop() {
        let keys = vec![key(), key()];
        assert_eq!(
            Minimal.get_many(&keys).await.expect("get_many"),
            [None, None]
        );

        let items = vec![(key(), Bytes::from_static(b"v"))];
        Minimal
            .set_many(&items, SetOpts::new())
            .await
            .expect("set_many");
    }

    #[test]
    fn set_opts_rejects_the_impossible_combinations() {
        assert!(SetOpts::new().validate().is_ok());
        assert!(SetOpts::new().if_absent().validate().is_ok());
        assert!(SetOpts::new().if_absent().if_present().validate().is_err());
        assert!(
            SetOpts::new()
                .keep_ttl()
                .ttl(Duration::from_secs(1))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn the_capability_presets_say_the_true_thing_about_each_backend() {
        assert!(!Capabilities::memory().pubsub_cross_process);
        assert!(!Capabilities::memory().persistence);
        assert!(!Capabilities::memory().scripting);
        assert!(Capabilities::redis().scripting);
        assert!(!Capabilities::postgres().scripting);
        assert!(Capabilities::postgres().persistence);
        assert_eq!(Capabilities::none(), Capabilities::default());
    }

    #[test]
    fn a_scan_cursor_reports_where_it_is() {
        assert!(ScanCursor::start().is_start());
        assert!(ScanCursor::end().is_end());
        assert_eq!(ScanCursor::at("7").bookmark(), Some("7"));
        assert_eq!(ScanCursor::start().bookmark(), None);
        assert_eq!(ScanCursor::end().bookmark(), None);
    }

    #[test]
    fn a_side_names_itself() {
        assert_eq!(Side::Left.to_string(), "left");
        assert_eq!(Side::Right.opposite(), Side::Left);
    }
}
