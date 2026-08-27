//! The PostgreSQL backend: one table, a TTL sweeper, and `LISTEN`/`NOTIFY`.
//!
//! For the team that will not run a second datastore. It is slower than Redis
//! and it is not pretending otherwise; what it buys is one thing to back up,
//! one thing to monitor and one thing to page about.
//!
//! # The table
//!
//! ```sql
//! CREATE TABLE moso_kv (
//!   key        text COLLATE "C" PRIMARY KEY,
//!   value      bytea       NOT NULL,
//!   kind       smallint    NOT NULL DEFAULT 0,
//!   expires_at timestamptz,
//!   updated_at timestamptz NOT NULL DEFAULT now()
//! );
//! CREATE INDEX moso_kv_expires_at_idx ON moso_kv (expires_at)
//!   WHERE expires_at IS NOT NULL;
//! ```
//!
//! `COLLATE "C"` is load-bearing. A prefix scan is `key LIKE 'prefix%'`, and
//! only a byte-ordered index can answer that without a sequential scan — a
//! locale-aware collation would make `delete_prefix` read the whole table.
//!
//! # Expiry is a predicate, not a job
//!
//! Every read carries `(expires_at IS NULL OR expires_at > now())`. An expired
//! row is invisible the microsecond it expires, whether or not the sweeper has
//! got to it. [`Sweeper`] only reclaims space; correctness does not depend on
//! it ever running, which is why it can be turned off in favour of `cron`.
//!
//! # Atomicity
//!
//! Every read-modify-write is one statement where that is possible — `incr`
//! and the three compare-and-swaps are single `INSERT … ON CONFLICT` or
//! `UPDATE … WHERE` statements — and a transaction with `SELECT … FOR UPDATE`
//! where it is not, which is the structure operations. The row is materialised
//! with an `INSERT … ON CONFLICT DO NOTHING` first, because `FOR UPDATE` locks
//! no row when there is no row and two concurrent pushes onto a new list would
//! otherwise both insert.
//!
//! # Pubsub is `LISTEN`/`NOTIFY`, with two real limits
//!
//! 1. A channel name is a PostgreSQL identifier: **63 bytes**, and anything
//!    longer is [`Error::Channel`] rather than a silently truncated name that
//!    two services would then share.
//! 2. A payload is at most 8000 bytes on the wire, and Moso hex-encodes so
//!    that arbitrary bytes survive a `text` round trip — so the usable limit is
//!    **3999 bytes**. Longer is [`Error::Channel`], not a truncated message.
//!
//! Notifications are also lost while nothing is listening, exactly like Redis
//! pubsub. It is a notification channel, not a queue.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt as _;
use moso_core::{BoxFuture, HealthStatus};
use sqlx::Row as _;
use sqlx::postgres::{PgPool, PgPoolOptions};

use crate::error::{Error, Result};
use crate::key::{Key, validate_name};
use crate::store::{Capabilities, KvStore, MessageStream, ScanCursor, SetOpts, Side};

/// The name this backend reports.
pub(crate) const BACKEND: &str = "postgres";

/// The longest channel name `LISTEN` accepts: `NAMEDATALEN - 1`.
///
/// ```
/// use moso_kv::backend::postgres::MAX_CHANNEL_LEN;
///
/// assert_eq!(MAX_CHANNEL_LEN, 63);
/// ```
pub const MAX_CHANNEL_LEN: usize = 63;

/// The longest payload a `NOTIFY` can carry, after hex encoding.
///
/// PostgreSQL's limit is 8000 bytes of `text`; hex doubles, and two bytes are
/// left for the encoder's own margin.
///
/// ```
/// use moso_kv::backend::postgres::MAX_PAYLOAD_LEN;
///
/// assert_eq!(MAX_PAYLOAD_LEN, 3_999);
/// ```
pub const MAX_PAYLOAD_LEN: usize = 3_999;

/// The predicate every read carries.
const LIVE: &str = "(expires_at IS NULL OR expires_at > now())";

/// `kind` for a plain byte string.
const KIND_BYTES: i16 = 0;
/// `kind` for a list.
const KIND_LIST: i16 = 1;
/// `kind` for a set.
const KIND_SET: i16 = 2;
/// `kind` for a sorted set.
const KIND_ZSET: i16 = 3;

/// The name `kind` goes by in a `WRONGTYPE` message.
const fn kind_name(kind: i16) -> &'static str {
    match kind {
        KIND_LIST => "list",
        KIND_SET => "set",
        KIND_ZSET => "zset",
        _ => "string",
    }
}

/// Hand a generated statement to `sqlx`.
///
/// `sqlx` 0.9 refuses a non-`'static` SQL string unless the caller asserts it
/// is safe, which is exactly the right default. Every statement in this module
/// is a `format!` whose only interpolation is
/// [`PostgresStore::table`](PostgresStore::table) — validated against
/// `[a-z0-9_-]{1,48}` by [`validate_name`] before the store is built — and the
/// data always arrives through a bind parameter. That is the audit this call
/// asserts, and it is why the table name is validated rather than quoted.
fn query(sql: String) -> sqlx::query::Query<'static, sqlx::Postgres, sqlx::postgres::PgArguments> {
    sqlx::query(sqlx::AssertSqlSafe(sql))
}

// ---------------------------------------------------------------------------
// PostgresStore
// ---------------------------------------------------------------------------

/// A [`KvStore`] over one PostgreSQL table.
///
/// ```no_run
/// use moso_kv::backend::PostgresStore;
/// use moso_kv::{Kv, Result};
/// use std::time::Duration;
///
/// # #[tokio::main] async fn main() -> Result<()> {
/// let store = PostgresStore::connect(
///     "postgres://moso:moso@localhost:5432/moso",
///     "moso_kv",
///     8,
///     Duration::from_secs(5),
/// )
/// .await?;
/// store.spawn_sweeper(Duration::from_secs(30));
///
/// let kv = Kv::builder("shop").store(store).build()?;
/// assert_eq!(kv.store().name(), "postgres");
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct PostgresStore {
    inner: Arc<PgInner>,
}

struct PgInner {
    pool: PgPool,
    table: String,
    /// The sweeper installed by [`PostgresStore::start_sweeper`], kept here so
    /// that it lives exactly as long as the store does.
    sweeper: std::sync::OnceLock<Sweeper>,
}

impl std::fmt::Debug for PostgresStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStore")
            .field("table", &self.inner.table)
            .field("connections", &self.inner.pool.size())
            .finish_non_exhaustive()
    }
}

impl PostgresStore {
    /// Open a pool and create the table if it is not there.
    ///
    /// Creating the table here rather than in a migration is deliberate: the
    /// cache's schema is an implementation detail of the cache, and a
    /// deployment that has to remember to run a migration before the cache
    /// works has a footgun rather than a feature. It is `CREATE TABLE IF NOT
    /// EXISTS`, so it is safe to run from every instance on every boot.
    ///
    /// # Errors
    ///
    /// [`Error::Key`] when `table` is not a plain lower-case identifier —
    /// which is checked because the name is interpolated into SQL — and
    /// [`Error::Backend`] when the database cannot be reached or the table
    /// cannot be created.
    ///
    /// ```no_run
    /// use moso_kv::backend::PostgresStore;
    /// use std::time::Duration;
    ///
    /// # #[tokio::main] async fn main() {
    /// let store = PostgresStore::connect(
    ///     "postgres://localhost/moso",
    ///     "moso_kv",
    ///     4,
    ///     Duration::from_secs(5),
    /// )
    /// .await
    /// .expect("connected");
    /// # let _ = store;
    /// # }
    /// ```
    pub async fn connect(
        url: &str,
        table: &str,
        pool_size: u32,
        connect_timeout: Duration,
    ) -> Result<Self> {
        // The table name reaches SQL as text, so it is validated to the same
        // alphabet as a namespace prefix rather than quoted and hoped for.
        validate_name("table", table)?;

        let pool = PgPoolOptions::new()
            .max_connections(pool_size.max(1))
            .acquire_timeout(connect_timeout)
            .connect(url)
            .await
            .map_err(|error| Error::backend(BACKEND, "connect", error))?;

        let store = Self {
            inner: Arc::new(PgInner {
                pool,
                table: table.to_owned(),
                sweeper: std::sync::OnceLock::new(),
            }),
        };
        store.create_table().await?;
        Ok(store)
    }

    /// Adopt an already-open pool.
    ///
    /// For an application that already has one — sharing it means the cache
    /// does not double the connection count, which is the number that takes
    /// PostgreSQL down.
    ///
    /// # Errors
    ///
    /// As [`connect`](Self::connect), minus the connection itself.
    ///
    /// ```no_run
    /// use moso_kv::backend::PostgresStore;
    /// use sqlx::postgres::PgPool;
    ///
    /// # #[tokio::main] async fn main() {
    /// # let pool: PgPool = unimplemented!();
    /// let store = PostgresStore::with_pool(pool, "moso_kv").await.expect("ready");
    /// # let _ = store;
    /// # }
    /// ```
    pub async fn with_pool(pool: PgPool, table: &str) -> Result<Self> {
        validate_name("table", table)?;
        let store = Self {
            inner: Arc::new(PgInner {
                pool,
                table: table.to_owned(),
                sweeper: std::sync::OnceLock::new(),
            }),
        };
        store.create_table().await?;
        Ok(store)
    }

    /// The pool, for the queries [`KvStore`] does not expose.
    ///
    /// ```no_run
    /// # use moso_kv::backend::PostgresStore;
    /// # async fn example(store: &PostgresStore) {
    /// let pool = store.pool();
    /// # let _ = pool;
    /// # }
    /// ```
    #[must_use]
    pub fn pool(&self) -> &PgPool {
        &self.inner.pool
    }

    /// The table this store reads and writes.
    ///
    /// ```no_run
    /// # use moso_kv::backend::PostgresStore;
    /// # async fn example(store: &PostgresStore) {
    /// assert_eq!(store.table(), "moso_kv");
    /// # }
    /// ```
    #[must_use]
    pub fn table(&self) -> &str {
        &self.inner.table
    }

    /// Create the table and its index if they are not there.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`] when the statements fail — most often because the
    /// role has no `CREATE` on the schema, which the message will say.
    pub async fn create_table(&self) -> Result<()> {
        let table = &self.inner.table;

        // `CREATE TABLE IF NOT EXISTS` is *not* race-free: two sessions that
        // run it at the same instant both pass the existence check and one gets
        // a unique-violation on `pg_type`. Every instance runs this on boot, so
        // the race is the common case rather than the rare one. A transaction
        // advisory lock — released by the commit, so a crash cannot strand it —
        // makes it serial.
        let mut tx = self
            .inner
            .pool
            .begin()
            .await
            .map_err(|error| Error::backend(BACKEND, "create_table", error))?;

        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(format!("moso_kv:create_table:{table}"))
            .execute(&mut *tx)
            .await
            .map_err(|error| Error::backend(BACKEND, "create_table", error))?;

        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS {table} (\
               key        text COLLATE \"C\" PRIMARY KEY,\
               value      bytea       NOT NULL,\
               kind       smallint    NOT NULL DEFAULT 0,\
               expires_at timestamptz,\
               updated_at timestamptz NOT NULL DEFAULT now()\
             )"
        );
        query(ddl)
            .execute(&mut *tx)
            .await
            .map_err(|error| Error::backend(BACKEND, "create_table", error))?;

        let index = format!(
            "CREATE INDEX IF NOT EXISTS {table}_expires_at_idx ON {table} (expires_at) \
             WHERE expires_at IS NOT NULL"
        );
        query(index)
            .execute(&mut *tx)
            .await
            .map_err(|error| Error::backend(BACKEND, "create_table", error))?;

        tx.commit()
            .await
            .map_err(|error| Error::backend(BACKEND, "create_table", error))?;
        Ok(())
    }

    /// Delete every expired row, returning how many went.
    ///
    /// Correctness never depends on this: an expired row is already invisible.
    /// It reclaims space.
    ///
    /// # Errors
    ///
    /// [`Error::Backend`].
    ///
    /// ```no_run
    /// # use moso_kv::backend::PostgresStore;
    /// # async fn example(store: &PostgresStore) {
    /// let reclaimed = store.sweep().await.expect("swept");
    /// println!("{reclaimed} expired rows removed");
    /// # }
    /// ```
    pub async fn sweep(&self) -> Result<u64> {
        let sql = format!(
            "DELETE FROM {} WHERE expires_at IS NOT NULL AND expires_at <= now()",
            self.inner.table
        );
        let done = query(sql)
            .execute(&self.inner.pool)
            .await
            .map_err(|error| Error::backend(BACKEND, "sweep", error))?;
        Ok(done.rows_affected())
    }

    /// Run [`sweep`](Self::sweep) every `interval` in the background.
    ///
    /// The returned [`Sweeper`] stops the task when it is dropped, so a test —
    /// or a graceful shutdown — can turn it off without waiting for a tick.
    ///
    /// ```no_run
    /// # use moso_kv::backend::PostgresStore;
    /// # use std::time::Duration;
    /// # async fn example(store: &PostgresStore) {
    /// let sweeper = store.spawn_sweeper(Duration::from_secs(30));
    /// // ... and dropping it stops the task.
    /// drop(sweeper);
    /// # }
    /// ```
    #[must_use]
    pub fn spawn_sweeper(&self, interval: Duration) -> Sweeper {
        // A `Weak` and not a clone: a task that held the store alive would keep
        // the pool open for the lifetime of the process, and a `Sweeper` stored
        // *inside* the store would be a reference cycle that never drops.
        let weak = Arc::downgrade(&self.inner);
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(1)));
            // `Delay` and not the default `Burst`: a process that was starved
            // for a minute must not then run sixty sweeps back to back.
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    // The store is gone, so there is nothing to sweep.
                    return;
                };
                let store = PostgresStore { inner };
                match store.sweep().await {
                    Ok(0) => {}
                    Ok(removed) => {
                        tracing::debug!(
                            removed,
                            table = %store.inner.table,
                            "swept expired kv rows"
                        );
                    }
                    Err(error) => {
                        tracing::warn!(error = %error, "the kv sweeper failed; retrying next tick");
                    }
                }
            }
        });
        Sweeper { handle }
    }

    /// Start a sweeper owned by the store itself, and say whether it started.
    ///
    /// `false` when one is already running: the sweeper is installed once, so
    /// two calls do not produce two tasks deleting the same rows.
    ///
    /// This is what [`KvConfig::build`](crate::KvConfig::build) uses. The
    /// alternative — holding the returned [`Sweeper`] — is for a caller that
    /// wants to stop it on purpose.
    ///
    /// ```no_run
    /// # use moso_kv::backend::PostgresStore;
    /// # use std::time::Duration;
    /// # async fn example(store: &PostgresStore) {
    /// assert!(store.start_sweeper(Duration::from_secs(30)));
    /// assert!(!store.start_sweeper(Duration::from_secs(30)), "only one");
    /// # }
    /// ```
    pub fn start_sweeper(&self, interval: Duration) -> bool {
        if self.inner.sweeper.get().is_some() {
            return false;
        }
        let sweeper = self.spawn_sweeper(interval);
        self.inner.sweeper.set(sweeper).is_ok()
    }

    /// The row for `key`, when it is live: `(value, kind)`.
    async fn live_row(&self, key: &Key, operation: &'static str) -> Result<Option<(Vec<u8>, i16)>> {
        let sql = format!(
            "SELECT value, kind FROM {} WHERE key = $1 AND {LIVE}",
            self.inner.table
        );
        let row = query(sql)
            .bind(key.as_str())
            .fetch_optional(&self.inner.pool)
            .await
            .map_err(|error| Error::backend(BACKEND, operation, error))?;

        Ok(row.map(|row| {
            let value: Vec<u8> = row.get("value");
            let kind: i16 = row.get("kind");
            (value, kind)
        }))
    }

    /// A structure's members, taking the row lock. `Ok(None)` for the wrong
    /// kind is impossible: it is an error.
    async fn locked_structure(
        &self,
        key: &Key,
        wanted: i16,
        operation: &'static str,
    ) -> Result<(sqlx::Transaction<'_, sqlx::Postgres>, Vec<u8>)> {
        let table = &self.inner.table;
        let mut tx = self
            .inner
            .pool
            .begin()
            .await
            .map_err(|error| Error::backend(BACKEND, operation, error))?;

        // Materialise the row so that `FOR UPDATE` has something to lock: on an
        // absent key it would lock nothing and two concurrent pushes would both
        // insert.
        let insert = format!(
            "INSERT INTO {table} (key, value, kind) VALUES ($1, ''::bytea, $2) \
             ON CONFLICT (key) DO NOTHING"
        );
        query(insert)
            .bind(key.as_str())
            .bind(wanted)
            .execute(&mut *tx)
            .await
            .map_err(|error| Error::backend(BACKEND, operation, error))?;

        let select = format!(
            "SELECT value, kind, (expires_at IS NOT NULL AND expires_at <= now()) AS expired \
             FROM {table} WHERE key = $1 FOR UPDATE"
        );
        let row = query(select)
            .bind(key.as_str())
            .fetch_one(&mut *tx)
            .await
            .map_err(|error| Error::backend(BACKEND, operation, error))?;

        let expired: bool = row.get("expired");
        let kind: i16 = row.get("kind");
        let value: Vec<u8> = row.get("value");

        if expired {
            // An expired row is a fresh one: its old bytes are invisible.
            return Ok((tx, Vec::new()));
        }
        if kind != wanted && !value.is_empty() {
            return Err(wrong_type(operation, kind_name(wanted), kind_name(kind)));
        }
        Ok((tx, value))
    }

    /// Write a structure back and commit, or delete the row when it is empty.
    async fn write_structure(
        &self,
        mut tx: sqlx::Transaction<'_, sqlx::Postgres>,
        key: &Key,
        kind: i16,
        value: Vec<u8>,
        operation: &'static str,
    ) -> Result<()> {
        let table = &self.inner.table;
        if value.is_empty() {
            let sql = format!("DELETE FROM {table} WHERE key = $1");
            query(sql)
                .bind(key.as_str())
                .execute(&mut *tx)
                .await
                .map_err(|error| Error::backend(BACKEND, operation, error))?;
        } else {
            let sql = format!(
                "UPDATE {table} SET value = $2, kind = $3, updated_at = now(), \
                 expires_at = CASE WHEN expires_at IS NOT NULL AND expires_at <= now() \
                                   THEN NULL ELSE expires_at END \
                 WHERE key = $1"
            );
            query(sql)
                .bind(key.as_str())
                .bind(value)
                .bind(kind)
                .execute(&mut *tx)
                .await
                .map_err(|error| Error::backend(BACKEND, operation, error))?;
        }
        tx.commit()
            .await
            .map_err(|error| Error::backend(BACKEND, operation, error))
    }
}

/// A running TTL sweeper. Dropping it stops the task.
///
/// ```no_run
/// # use moso_kv::backend::PostgresStore;
/// # use std::time::Duration;
/// # async fn example(store: &PostgresStore) {
/// let sweeper = store.spawn_sweeper(Duration::from_secs(30));
/// assert!(!sweeper.is_finished());
/// # }
/// ```
#[derive(Debug)]
pub struct Sweeper {
    handle: tokio::task::JoinHandle<()>,
}

impl Sweeper {
    /// Whether the task has stopped, which it only does when aborted.
    ///
    /// ```no_run
    /// # use moso_kv::backend::Sweeper;
    /// # fn example(sweeper: &Sweeper) {
    /// assert!(!sweeper.is_finished());
    /// # }
    /// ```
    #[must_use]
    pub fn is_finished(&self) -> bool {
        self.handle.is_finished()
    }

    /// Stop it now rather than at the next drop.
    ///
    /// ```no_run
    /// # use moso_kv::backend::Sweeper;
    /// # fn example(sweeper: Sweeper) {
    /// sweeper.stop();
    /// # }
    /// ```
    pub fn stop(self) {
        // `Drop` does the aborting; this is the spelling that says so.
        drop(self);
    }
}

impl Drop for Sweeper {
    fn drop(&mut self) {
        self.handle.abort();
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

/// A TTL as seconds for `make_interval`, or `NULL`.
fn ttl_seconds(ttl: Option<Duration>) -> Option<f64> {
    ttl.map(|ttl| ttl.as_secs_f64())
}

/// Escape the `LIKE` metacharacters so a key part cannot widen a prefix scan.
///
/// The same class of bug as an unescaped glob on Redis: a key containing `%`
/// would make `delete_prefix` delete other namespaces.
///
/// ```
/// use moso_kv::backend::postgres::like_escape;
///
/// assert_eq!(like_escape("moso:v1:a:b:1:"), "moso:v1:a:b:1:");
/// assert_eq!(like_escape("a%b"), "a\\%b");
/// assert_eq!(like_escape("a_b"), "a\\_b");
/// assert_eq!(like_escape("a\\b"), "a\\\\b");
/// ```
#[must_use]
pub fn like_escape(prefix: &str) -> String {
    let mut out = String::with_capacity(prefix.len() + 8);
    for ch in prefix.chars() {
        if matches!(ch, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

// ---------------------------------------------------------------------------
// Framing for the structure kinds
// ---------------------------------------------------------------------------

/// Length-prefixed frames: `[u32 LE length][bytes]`, repeated.
fn encode_frames(items: &[Bytes]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|item| item.len() + 4).sum());
    for item in items {
        let length = u32::try_from(item.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&item[..usize::try_from(length).unwrap_or(usize::MAX)]);
    }
    out
}

/// The inverse of [`encode_frames`].
fn decode_frames(bytes: &[u8], operation: &'static str) -> Result<Vec<Bytes>> {
    let mut out = Vec::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if cursor + 4 > bytes.len() {
            return Err(corrupt(operation));
        }
        let mut length = [0_u8; 4];
        length.copy_from_slice(&bytes[cursor..cursor + 4]);
        let length = usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX);
        cursor += 4;
        let end = cursor
            .checked_add(length)
            .ok_or_else(|| corrupt(operation))?;
        if end > bytes.len() {
            return Err(corrupt(operation));
        }
        out.push(Bytes::copy_from_slice(&bytes[cursor..end]));
        cursor = end;
    }
    Ok(out)
}

/// A sorted set: `[f64 LE score][u32 LE length][member]`, repeated.
fn encode_scored(items: &[(f64, Bytes)]) -> Vec<u8> {
    let mut out = Vec::with_capacity(items.iter().map(|(_, item)| item.len() + 12).sum());
    for (score, member) in items {
        out.extend_from_slice(&score.to_le_bytes());
        let length = u32::try_from(member.len()).unwrap_or(u32::MAX);
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&member[..usize::try_from(length).unwrap_or(usize::MAX)]);
    }
    out
}

/// The inverse of [`encode_scored`].
fn decode_scored(bytes: &[u8], operation: &'static str) -> Result<Vec<(f64, Bytes)>> {
    let mut out = Vec::new();
    let mut cursor = 0_usize;
    while cursor < bytes.len() {
        if cursor + 12 > bytes.len() {
            return Err(corrupt(operation));
        }
        let mut score = [0_u8; 8];
        score.copy_from_slice(&bytes[cursor..cursor + 8]);
        let mut length = [0_u8; 4];
        length.copy_from_slice(&bytes[cursor + 8..cursor + 12]);
        let length = usize::try_from(u32::from_le_bytes(length)).unwrap_or(usize::MAX);
        cursor += 12;
        let end = cursor
            .checked_add(length)
            .ok_or_else(|| corrupt(operation))?;
        if end > bytes.len() {
            return Err(corrupt(operation));
        }
        out.push((
            f64::from_le_bytes(score),
            Bytes::copy_from_slice(&bytes[cursor..end]),
        ));
        cursor = end;
    }
    Ok(out)
}

/// Sort by score, then by member, so iteration order is deterministic.
fn sort_scored(items: &mut [(f64, Bytes)]) {
    items.sort_by(|(left_score, left), (right_score, right)| {
        left_score
            .partial_cmp(right_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.cmp(right))
    });
}

/// The error a truncated or foreign structure encoding produces.
fn corrupt(operation: &'static str) -> Error {
    Error::backend(
        BACKEND,
        operation,
        "the stored structure is truncated or was not written by moso-kv",
    )
}

/// Hex-encode, so arbitrary bytes survive `NOTIFY`'s `text` payload.
fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// The inverse of [`to_hex`]. A malformed payload yields no message rather than
/// a wrong one.
fn from_hex(text: &str) -> Option<Bytes> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let high = char::from(pair[0]).to_digit(16)?;
        let low = char::from(pair[1]).to_digit(16)?;
        out.push(u8::try_from(high * 16 + low).ok()?);
    }
    Some(Bytes::from(out))
}

/// Reject a channel name or payload `NOTIFY` cannot carry.
fn check_channel(channel: &str) -> Result<()> {
    if channel.is_empty() || channel.len() > MAX_CHANNEL_LEN {
        return Err(Error::Channel {
            backend: BACKEND,
            channel: channel.to_owned(),
            reason: "a PostgreSQL channel name is 1 to 63 bytes",
        });
    }
    Ok(())
}

impl KvStore for PostgresStore {
    fn name(&self) -> &'static str {
        BACKEND
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::postgres()
    }

    fn health(&self) -> BoxFuture<'_, HealthStatus> {
        Box::pin(async move {
            match sqlx::query("SELECT 1").execute(&self.inner.pool).await {
                Ok(_) => HealthStatus::Up,
                Err(error) => HealthStatus::Down(error.to_string()),
            }
        })
    }

    fn get<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            match self.live_row(key, "get").await? {
                None => Ok(None),
                Some((value, KIND_BYTES)) => Ok(Some(Bytes::from(value))),
                Some((_, kind)) => Err(wrong_type("get", "string", kind_name(kind))),
            }
        })
    }

    fn set<'a>(&'a self, key: &'a Key, value: Bytes, opts: SetOpts) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            opts.validate()?;
            let table = &self.inner.table;
            let seconds = ttl_seconds(opts.ttl);

            // `keep_ttl` keeps whatever the row had; otherwise the expiry is
            // replaced, including with `NULL`.
            let expiry = if opts.keep_ttl {
                format!("{table}.expires_at")
            } else {
                String::from(
                    "CASE WHEN $3::double precision IS NULL THEN NULL \
                          ELSE now() + make_interval(secs => $3) END",
                )
            };

            let sql = if opts.if_present {
                format!(
                    "UPDATE {table} SET value = $2, kind = 0, updated_at = now(), \
                     expires_at = {expiry} \
                     WHERE key = $1 AND {LIVE} RETURNING 1"
                )
            } else if opts.if_absent {
                // "Absent" includes "expired": an expired row is invisible, so
                // an `NX` write must be allowed to take its place.
                format!(
                    "INSERT INTO {table} (key, value, kind, expires_at, updated_at) \
                     VALUES ($1, $2, 0, \
                       CASE WHEN $3::double precision IS NULL THEN NULL \
                            ELSE now() + make_interval(secs => $3) END, now()) \
                     ON CONFLICT (key) DO UPDATE SET \
                       value = EXCLUDED.value, kind = 0, expires_at = EXCLUDED.expires_at, \
                       updated_at = now() \
                     WHERE {table}.expires_at IS NOT NULL AND {table}.expires_at <= now() \
                     RETURNING 1"
                )
            } else {
                format!(
                    "INSERT INTO {table} (key, value, kind, expires_at, updated_at) \
                     VALUES ($1, $2, 0, \
                       CASE WHEN $3::double precision IS NULL THEN NULL \
                            ELSE now() + make_interval(secs => $3) END, now()) \
                     ON CONFLICT (key) DO UPDATE SET \
                       value = EXCLUDED.value, kind = 0, expires_at = {expiry}, \
                       updated_at = now() \
                     RETURNING 1"
                )
            };

            let applied = query(sql)
                .bind(key.as_str())
                .bind(value.as_ref())
                .bind(seconds)
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "set", error))?;
            Ok(applied.is_some())
        })
    }

    fn delete<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // The row goes whether or not it had expired; the *answer* is
            // whether it was still visible.
            let sql = format!(
                "DELETE FROM {} WHERE key = $1 RETURNING {LIVE} AS was_live",
                self.inner.table
            );
            let row = query(sql)
                .bind(key.as_str())
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "delete", error))?;
            Ok(row.is_some_and(|row| row.get::<bool, _>("was_live")))
        })
    }

    fn exists<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let sql = format!(
                "SELECT 1 FROM {} WHERE key = $1 AND {LIVE}",
                self.inner.table
            );
            let row = query(sql)
                .bind(key.as_str())
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "exists", error))?;
            Ok(row.is_some())
        })
    }

    fn expire<'a>(&'a self, key: &'a Key, ttl: Duration) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let sql = format!(
                "UPDATE {} SET expires_at = now() + make_interval(secs => $2), \
                 updated_at = now() WHERE key = $1 AND {LIVE} RETURNING 1",
                self.inner.table
            );
            let row = query(sql)
                .bind(key.as_str())
                .bind(ttl.as_secs_f64())
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "expire", error))?;
            Ok(row.is_some())
        })
    }

    fn ttl<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async move {
            let sql = format!(
                "SELECT EXTRACT(EPOCH FROM (expires_at - now()))::double precision AS remaining \
                 FROM {} WHERE key = $1 AND {LIVE} AND expires_at IS NOT NULL",
                self.inner.table
            );
            let row = query(sql)
                .bind(key.as_str())
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "ttl", error))?;

            Ok(row.and_then(|row| {
                let remaining: Option<f64> = row.get("remaining");
                remaining.map(|seconds| Duration::from_secs_f64(seconds.max(0.0)))
            }))
        })
    }

    fn incr<'a>(
        &'a self,
        key: &'a Key,
        by: i64,
        ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move {
            let table = &self.inner.table;
            // One statement, so two concurrent increments cannot both read the
            // same starting value. The expiry is set only when the row is
            // created or was expired, which is what `Redis`' script does too.
            let sql = format!(
                "INSERT INTO {table} (key, value, kind, expires_at, updated_at) \
                 VALUES ($1, $2::text::bytea, 0, \
                   CASE WHEN $4::double precision IS NULL THEN NULL \
                        ELSE now() + make_interval(secs => $4) END, now()) \
                 ON CONFLICT (key) DO UPDATE SET \
                   value = CASE \
                     WHEN {table}.expires_at IS NOT NULL AND {table}.expires_at <= now() \
                       THEN EXCLUDED.value \
                     ELSE ((convert_from({table}.value, 'UTF8')::bigint + $3)::text)::bytea \
                   END, \
                   kind = 0, \
                   expires_at = CASE \
                     WHEN {table}.expires_at IS NULL THEN EXCLUDED.expires_at \
                     WHEN {table}.expires_at <= now() THEN EXCLUDED.expires_at \
                     ELSE {table}.expires_at \
                   END, \
                   updated_at = now() \
                 RETURNING convert_from(value, 'UTF8')::bigint AS counter"
            );

            let row = query(sql)
                .bind(key.as_str())
                .bind(by.to_string())
                .bind(by)
                .bind(ttl_seconds(ttl))
                .fetch_one(&self.inner.pool)
                .await
                .map_err(|error| {
                    // A non-numeric or non-UTF-8 value is the `INCR` on a
                    // string that Redis reports; say the same thing.
                    if matches!(&error, sqlx::Error::Database(_)) {
                        Error::backend(
                            BACKEND,
                            "incr",
                            "the value under this key is not an integer",
                        )
                    } else {
                        Error::backend(BACKEND, "incr", error)
                    }
                })?;
            Ok(row.get::<i64, _>("counter"))
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
            match old {
                // "Must be absent" is exactly an `NX` write.
                None => self.set(key, new, opts.if_absent()).await,
                Some(expected) => {
                    let table = &self.inner.table;
                    let expiry = if opts.keep_ttl {
                        String::from("expires_at")
                    } else {
                        String::from(
                            "CASE WHEN $4::double precision IS NULL THEN NULL \
                                  ELSE now() + make_interval(secs => $4) END",
                        )
                    };
                    let sql = format!(
                        "UPDATE {table} SET value = $2, kind = 0, updated_at = now(), \
                         expires_at = {expiry} \
                         WHERE key = $1 AND {LIVE} AND kind = 0 AND value = $3 RETURNING 1"
                    );
                    let row = query(sql)
                        .bind(key.as_str())
                        .bind(new.as_ref())
                        .bind(expected)
                        .bind(ttl_seconds(opts.ttl))
                        .fetch_optional(&self.inner.pool)
                        .await
                        .map_err(|error| Error::backend(BACKEND, "compare_and_swap", error))?;
                    Ok(row.is_some())
                }
            }
        })
    }

    fn compare_and_delete<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let sql = format!(
                "DELETE FROM {} WHERE key = $1 AND {LIVE} AND kind = 0 AND value = $2 RETURNING 1",
                self.inner.table
            );
            let row = query(sql)
                .bind(key.as_str())
                .bind(expected)
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "compare_and_delete", error))?;
            Ok(row.is_some())
        })
    }

    fn compare_and_expire<'a>(
        &'a self,
        key: &'a Key,
        expected: &'a [u8],
        ttl: Duration,
    ) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let sql = format!(
                "UPDATE {} SET expires_at = now() + make_interval(secs => $3), \
                 updated_at = now() \
                 WHERE key = $1 AND {LIVE} AND kind = 0 AND value = $2 RETURNING 1",
                self.inner.table
            );
            let row = query(sql)
                .bind(key.as_str())
                .bind(expected)
                .bind(ttl.as_secs_f64())
                .fetch_optional(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "compare_and_expire", error))?;
            Ok(row.is_some())
        })
    }

    fn scan<'a>(
        &'a self,
        prefix: &'a Key,
        cursor: ScanCursor,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<Key>, ScanCursor)>> {
        Box::pin(async move {
            let pattern = format!("{}%", like_escape(prefix.as_str()));
            let after = cursor.bookmark().unwrap_or("").to_owned();
            let take = i64::from(limit.max(1));

            let sql = format!(
                "SELECT key FROM {} WHERE key LIKE $1 ESCAPE '\\' AND key > $2 AND {LIVE} \
                 ORDER BY key LIMIT $3",
                self.inner.table
            );
            let rows = query(sql)
                .bind(&pattern)
                .bind(&after)
                .bind(take)
                .fetch_all(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "scan", error))?;

            let mut keys = Vec::with_capacity(rows.len());
            for row in &rows {
                keys.push(Key::from_raw(row.get::<String, _>("key"))?);
            }

            // A short page is the last page: the query is a `LIMIT` over an
            // ordered index, not a probabilistic walk.
            let next = match keys.last() {
                Some(last) if i64::try_from(keys.len()).unwrap_or(i64::MAX) == take => {
                    ScanCursor::at(last.as_str())
                }
                _ => ScanCursor::end(),
            };
            Ok((keys, next))
        })
    }

    fn delete_prefix<'a>(&'a self, prefix: &'a Key) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            // One statement rather than the trait's scan-and-delete loop.
            let pattern = format!("{}%", like_escape(prefix.as_str()));
            let sql = format!(
                "DELETE FROM {} WHERE key LIKE $1 ESCAPE '\\' AND {LIVE}",
                self.inner.table
            );
            let done = query(sql)
                .bind(&pattern)
                .execute(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "delete_prefix", error))?;
            Ok(done.rows_affected())
        })
    }

    fn list_push<'a>(
        &'a self,
        key: &'a Key,
        values: &'a [Bytes],
        side: Side,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let (tx, held) = self.locked_structure(key, KIND_LIST, "list_push").await?;
            let mut items = decode_frames(&held, "list_push")?;
            match side {
                Side::Right => items.extend(values.iter().cloned()),
                Side::Left => {
                    for value in values {
                        items.insert(0, value.clone());
                    }
                }
            }
            let length = items.len() as u64;
            self.write_structure(tx, key, KIND_LIST, encode_frames(&items), "list_push")
                .await?;
            Ok(length)
        })
    }

    fn list_pop<'a>(
        &'a self,
        key: &'a Key,
        side: Side,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async move {
            let deadline = timeout.map(|timeout| std::time::Instant::now() + timeout);
            loop {
                let (tx, held) = self.locked_structure(key, KIND_LIST, "list_pop").await?;
                let mut items = decode_frames(&held, "list_pop")?;
                if !items.is_empty() {
                    let popped = match side {
                        Side::Left => items.remove(0),
                        Side::Right => items.pop().unwrap_or_default(),
                    };
                    self.write_structure(tx, key, KIND_LIST, encode_frames(&items), "list_pop")
                        .await?;
                    return Ok(Some(popped));
                }
                drop(tx);

                match deadline {
                    // No blocking pop without `LISTEN`, so poll. Twenty-five
                    // milliseconds bounds the added latency and keeps the
                    // statement rate sane on a shared database.
                    Some(deadline) if std::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    _ => return Ok(None),
                }
            }
        })
    }

    fn list_len<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            match self.live_row(key, "list_len").await? {
                None => Ok(0),
                Some((value, KIND_LIST)) => Ok(decode_frames(&value, "list_len")?.len() as u64),
                Some((_, kind)) => Err(wrong_type("list_len", "list", kind_name(kind))),
            }
        })
    }

    fn set_add<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let (tx, held) = self.locked_structure(key, KIND_SET, "set_add").await?;
            let mut items = decode_frames(&held, "set_add")?;
            let mut added = 0_u64;
            for member in members {
                if !items.iter().any(|existing| existing == member) {
                    items.push(member.clone());
                    added += 1;
                }
            }
            self.write_structure(tx, key, KIND_SET, encode_frames(&items), "set_add")
                .await?;
            Ok(added)
        })
    }

    fn set_remove<'a>(&'a self, key: &'a Key, members: &'a [Bytes]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let (tx, held) = self.locked_structure(key, KIND_SET, "set_remove").await?;
            let mut items = decode_frames(&held, "set_remove")?;
            let before = items.len();
            items.retain(|existing| !members.iter().any(|member| member == existing));
            let removed = (before - items.len()) as u64;
            self.write_structure(tx, key, KIND_SET, encode_frames(&items), "set_remove")
                .await?;
            Ok(removed)
        })
    }

    fn set_members<'a>(&'a self, key: &'a Key) -> BoxFuture<'a, Result<Vec<Bytes>>> {
        Box::pin(async move {
            match self.live_row(key, "set_members").await? {
                None => Ok(Vec::new()),
                Some((value, KIND_SET)) => decode_frames(&value, "set_members"),
                Some((_, kind)) => Err(wrong_type("set_members", "set", kind_name(kind))),
            }
        })
    }

    fn zadd<'a>(&'a self, key: &'a Key, scored: &'a [(f64, Bytes)]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let (tx, held) = self.locked_structure(key, KIND_ZSET, "zadd").await?;
            let mut items = decode_scored(&held, "zadd")?;
            let mut added = 0_u64;
            for (score, member) in scored {
                match items.iter_mut().find(|(_, existing)| existing == member) {
                    Some(entry) => entry.0 = *score,
                    None => {
                        items.push((*score, member.clone()));
                        added += 1;
                    }
                }
            }
            sort_scored(&mut items);
            self.write_structure(tx, key, KIND_ZSET, encode_scored(&items), "zadd")
                .await?;
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
            let items = match self.live_row(key, "zrange_by_score").await? {
                None => return Ok(Vec::new()),
                Some((value, KIND_ZSET)) => decode_scored(&value, "zrange_by_score")?,
                Some((_, kind)) => {
                    return Err(wrong_type("zrange_by_score", "zset", kind_name(kind)));
                }
            };
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
            let (tx, held) = self.locked_structure(key, KIND_ZSET, "zrem").await?;
            let mut items = decode_scored(&held, "zrem")?;
            let before = items.len();
            items.retain(|(_, existing)| !members.iter().any(|member| member == existing));
            let removed = (before - items.len()) as u64;
            self.write_structure(tx, key, KIND_ZSET, encode_scored(&items), "zrem")
                .await?;
            Ok(removed)
        })
    }

    fn publish<'a>(&'a self, channel: &'a str, payload: Bytes) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            check_channel(channel)?;
            if payload.len() > MAX_PAYLOAD_LEN {
                return Err(Error::Channel {
                    backend: BACKEND,
                    channel: channel.to_owned(),
                    reason: "a NOTIFY payload is at most 3999 bytes once hex-encoded",
                });
            }

            sqlx::query("SELECT pg_notify($1, $2)")
                .bind(channel)
                .bind(to_hex(&payload))
                .execute(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "publish", error))?;

            // `pg_notify` does not report how many sessions were listening, and
            // inventing a number would be worse than saying zero.
            Ok(0)
        })
    }

    fn subscribe<'a>(&'a self, channel: &'a str) -> BoxFuture<'a, Result<MessageStream>> {
        Box::pin(async move {
            check_channel(channel)?;

            // `LISTEN` holds a session, so this takes a connection of its own
            // rather than one of the pool's.
            let mut listener = sqlx::postgres::PgListener::connect_with(&self.inner.pool)
                .await
                .map_err(|error| Error::backend(BACKEND, "subscribe", error))?;
            listener
                .listen(channel)
                .await
                .map_err(|error| Error::backend(BACKEND, "subscribe", error))?;

            let stream = listener
                .into_stream()
                .filter_map(|notification| async move {
                    // A payload that is not hex was not written by `moso-kv`;
                    // skipping it is right, because guessing would deliver a
                    // corrupt message.
                    notification
                        .ok()
                        .and_then(|notification| from_hex(notification.payload()))
                });
            Ok(Box::pin(stream) as MessageStream)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn like_metacharacters_are_escaped() {
        assert_eq!(
            like_escape("moso:v1:shop:profile:1:"),
            "moso:v1:shop:profile:1:"
        );
        assert_eq!(like_escape("100%"), "100\\%");
        assert_eq!(like_escape("a_b"), "a\\_b");
        assert_eq!(like_escape("a\\cb"), "a\\\\cb");
    }

    #[test]
    fn frames_round_trip() {
        let items = vec![
            Bytes::from_static(b""),
            Bytes::from_static(b"a"),
            Bytes::from_static(b"\x00\xff"),
        ];
        let encoded = encode_frames(&items);
        assert_eq!(decode_frames(&encoded, "test").expect("decodes"), items);
        assert!(decode_frames(&[], "test").expect("empty").is_empty());
    }

    #[test]
    fn a_truncated_frame_is_an_error_and_not_a_guess() {
        let encoded = encode_frames(&[Bytes::from_static(b"abcd")]);
        for cut in 1..encoded.len() {
            assert!(
                decode_frames(&encoded[..cut], "test").is_err(),
                "{cut} bytes decoded"
            );
        }
    }

    #[test]
    fn scored_frames_round_trip_and_sort() {
        let mut items = vec![
            (2.0, Bytes::from_static(b"b")),
            (1.0, Bytes::from_static(b"a")),
            (1.0, Bytes::from_static(b"z")),
        ];
        sort_scored(&mut items);
        assert_eq!(
            items.iter().map(|(_, m)| m.clone()).collect::<Vec<_>>(),
            vec![
                Bytes::from_static(b"a"),
                Bytes::from_static(b"z"),
                Bytes::from_static(b"b"),
            ]
        );

        let encoded = encode_scored(&items);
        assert_eq!(decode_scored(&encoded, "test").expect("decodes"), items);
        assert!(decode_scored(&encoded[..7], "test").is_err());
    }

    #[test]
    fn hex_round_trips_arbitrary_bytes() {
        let payload: Vec<u8> = (0..=255_u8).collect();
        let hex = to_hex(&payload);
        assert_eq!(hex.len(), 512);
        assert_eq!(from_hex(&hex).expect("decodes"), Bytes::from(payload));

        assert_eq!(from_hex("abc"), None, "odd length");
        assert_eq!(from_hex("zz"), None, "not hex");
        assert_eq!(from_hex(""), Some(Bytes::new()));
    }

    #[test]
    fn a_channel_name_is_checked_against_the_real_limit() {
        assert!(check_channel("orders").is_ok());
        assert!(check_channel("").is_err());
        assert!(check_channel(&"x".repeat(MAX_CHANNEL_LEN)).is_ok());
        assert!(check_channel(&"x".repeat(MAX_CHANNEL_LEN + 1)).is_err());
    }

    #[test]
    fn a_kind_names_itself() {
        assert_eq!(kind_name(KIND_BYTES), "string");
        assert_eq!(kind_name(KIND_LIST), "list");
        assert_eq!(kind_name(KIND_SET), "set");
        assert_eq!(kind_name(KIND_ZSET), "zset");
        assert_eq!(kind_name(99), "string");
    }

    #[test]
    fn a_ttl_becomes_seconds_or_null() {
        assert_eq!(ttl_seconds(None), None);
        assert_eq!(ttl_seconds(Some(Duration::from_millis(1_500))), Some(1.5));
    }
}
