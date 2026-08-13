#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = "Moso's key-value and cache layer."]
//!
//! Every non-trivial web application needs a key-value store: sessions, caches,
//! rate limits, locks, ephemeral state. Today a Rust developer picks between
//! two Redis clients, wires a pool, invents a key scheme and hand-rolls
//! serialisation. This crate is that layer, with typed namespaces on top and a
//! backend trait underneath, so the **same code** runs against an in-process
//! map in tests and Redis in production.
//!
//! ```
//! use moso_kv::{minutes, seconds, Kv, Result};
//!
//! moso_kv::namespace! {
//!     /// Cached user profile, refreshed on write.
//!     pub Profile: u64 => Option<String>, ttl = minutes(15), negative_ttl = seconds(30);
//!
//!     /// The session record. Losing one logs somebody out, so it fails loudly.
//!     pub Session: String => String, ttl = minutes(480), on_failure = fail;
//! }
//!
//! # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
//! let kv = Kv::in_memory("shop")?;
//!
//! let profile = kv
//!     .get_or_insert_with::<Profile, _, _>(&7, || async { Ok(Some("alice".to_owned())) })
//!     .await?;
//!
//! assert_eq!(profile.as_deref(), Some("alice"));
//! assert_eq!(kv.key::<Profile>(&7)?.as_str(), "moso:v1:shop:profile:1:7");
//! # Ok(())
//! # }
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@store`] | [`KvStore`], [`Capabilities`], [`SetOpts`], [`Side`], [`ScanCursor`] |
//! | [`mod@backend`] | the memory, Redis and PostgreSQL implementations |
//! | [`mod@key`] | [`Key`], [`KeyPart`], and why a key cannot forge a namespace |
//! | [`mod@codec`] | [`Codec`], [`Json`](codec::Json), [`Raw`](codec::Raw), the envelope |
//! | [`mod@namespace`] | [`Namespace`], [`FailureMode`], [`namespace!`](macro@crate::namespace) |
//! | [`mod@kv`] | [`Kv`] — typed reads and writes, and the failure policy |
//! | [`mod@cached`] | [`cached!`](macro@crate::cached) — single-flight caching |
//! | [`mod@flight`] | [`SingleFlight`](flight::SingleFlight) — one computation per key |
//! | [`mod@lock`] | [`LockGuard`], and an honest warning about failover |
//! | [`mod@rate`] | [`RateLimit`], the GCRA limiter, as a `Guard` |
//! | [`mod@breaker`] | [`Breaker`] — the circuit breaker |
//! | [`mod@bus`] | [`Bus`], [`Topic`], [`Presence`] — cross-instance pub/sub |
//! | [`mod@config`] | [`KvConfig`] — backend choice as configuration |
//! | [`mod@health`] | [`KvHealthCheck`] — the `/readyz` probe |
//! | [`mod@error`] | [`Error`], and what each variant becomes over HTTP |
//!
//! # Three decisions worth knowing before reading the code
//!
//! **A cache is not a database.** [`FailureMode::Degrade`] is the default: an
//! unreachable store turns a `get` into a miss and a `set` into a no-op, logs
//! at `warn`, increments a counter, and lets the request reach the source of
//! truth. Sessions and locks declare `on_failure = fail`, because silently
//! losing one of those is worse than a 503.
//!
//! **`capabilities()` is not decoration.** Sixteen of [`KvStore`]'s twenty-six
//! operations are optional, and a backend says which it has. That is how the memory backend
//! can be a real test double — it implements the same semantics, including TTL
//! granularity and compare-and-swap — while saying honestly that its pubsub
//! does not cross a process boundary.
//!
//! **A key cannot forge a namespace.** The application, namespace and version
//! segments are at fixed positions and every key part has its `:` escaped, so
//! no value a user supplies can move a key into another namespace. This is
//! fuzzed, not asserted by inspection.
//!
//! # Cargo features
//!
//! | Feature | Default | What it adds |
//! | --- | --- | --- |
//! | `memory` | yes | `backend::MemoryStore`, on `moka` |
//! | `redis` | no | `backend::RedisStore`, on `fred` |
//! | `pg-kv` | no | `backend::PostgresStore`, on `sqlx` |
//!
//! Code spans rather than links: a link to a type that only exists under a
//! cargo feature is a broken link in every build that does not turn it on, and
//! `rustdoc::broken_intra_doc_links` is `deny` across this workspace.

pub mod backend;
pub mod breaker;
pub mod bus;
pub mod cached;
pub mod codec;
pub mod config;
pub mod error;
pub mod flight;
pub mod health;
pub mod key;
pub mod kv;
pub mod lock;
pub mod namespace;
pub mod rate;
pub mod store;

pub use crate::breaker::{Breaker, BreakerConfig, BreakerState};
pub use crate::bus::{
    Bus, BusCapabilities, KvBus, LocalBus, Presence, Topic, TopicStream, TypedBus,
};
pub use crate::codec::{Codec, Encodable, Envelope, Framed};
pub use crate::config::{KvBackend, KvConfig};
pub use crate::error::{BoxError, Error, Result};
pub use crate::health::KvHealthCheck;
pub use crate::key::{Key, KeyBuf, KeyError, KeyPart};
pub use crate::kv::{CachedValue, Kv, KvBuilder, KvStats};
pub use crate::lock::{LockGuard, LockOptions};
pub use crate::namespace::{FailureMode, Namespace, days, hours, minutes, seconds};
pub use crate::rate::{RateDecision, RateKey, RateLimit, RateQuota, RateSubject};
pub use crate::store::{Capabilities, KvStore, MessageStream, ScanCursor, SetOpts, Side};

/// Everything an application that uses `moso-kv` imports.
///
/// ```
/// use moso_kv::prelude::*;
///
/// moso_kv::namespace! {
///     /// A cached greeting.
///     pub Greeting: u64 => String, ttl = minutes(1);
/// }
///
/// # #[tokio::main(flavor = "current_thread")] async fn main() -> Result<()> {
/// let kv = Kv::in_memory("shop")?;
/// kv.set::<Greeting>(&1, &"hello".to_owned()).await?;
/// assert_eq!(kv.get::<Greeting>(&1).await?.as_deref(), Some("hello"));
/// # Ok(())
/// # }
/// ```
pub mod prelude {
    pub use crate::codec::{Json, Raw};
    pub use crate::{
        Capabilities, Error, FailureMode, Key, KeyPart, Kv, KvStore, LockOptions, Namespace,
        RateKey, RateLimit, RateQuota, Result, SetOpts, days, hours, minutes, seconds,
    };
}
