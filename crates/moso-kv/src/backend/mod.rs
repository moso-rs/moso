//! The three backends, each behind its own cargo feature.
//!
//! | Backend | Feature | Use |
//! | --- | --- | --- |
//! | `MemoryStore` | `memory` (default) | dev, tests, single instance |
//! | `RedisStore` | `redis` | the production standard |
//! | `PostgresStore` | `pg-kv` | teams refusing a second datastore |
//!
//! The names are code spans and not links on purpose: two of the three do not
//! exist unless their feature is on, and a link that resolves only under
//! `--all-features` is a broken link in every other build.
//!
//! Backend choice is configuration, not code — see [`KvConfig`](crate::KvConfig)
//! — so a handler that reads a cache does not know or care which one it is
//! talking to. What it may care about is
//! [`Capabilities`](crate::Capabilities), and that is a runtime value for the
//! same reason.
//!
//! A fourth is a `KvStore` impl in your own crate; nothing here is privileged.

#[cfg(feature = "memory")]
#[cfg_attr(docsrs, doc(cfg(feature = "memory")))]
pub mod memory;

#[cfg(feature = "redis")]
#[cfg_attr(docsrs, doc(cfg(feature = "redis")))]
pub mod redis;

#[cfg(feature = "pg-kv")]
#[cfg_attr(docsrs, doc(cfg(feature = "pg-kv")))]
pub mod postgres;

#[cfg(feature = "memory")]
#[cfg_attr(docsrs, doc(cfg(feature = "memory")))]
pub use crate::backend::memory::MemoryStore;

#[cfg(feature = "redis")]
#[cfg_attr(docsrs, doc(cfg(feature = "redis")))]
pub use crate::backend::redis::{RedisConfig, RedisStore};

#[cfg(feature = "pg-kv")]
#[cfg_attr(docsrs, doc(cfg(feature = "pg-kv")))]
pub use crate::backend::postgres::{PostgresStore, Sweeper};
