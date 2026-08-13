//! Every store this battery keeps in a **table** or in a **map**, and the four
//! tables it owns.
//!
//! A store is where a credential lives between two requests, and every one of
//! them has the same two-implementation shape: a map, which is complete and
//! per-process, and a table, which is the same semantics somewhere a second
//! process can see. A deployment with two instances and a map-backed store is
//! not slower, it is *wrong* — a session issued by one instance is unknown to
//! the other.
//!
//! | Trait | In memory | In a table | Its table |
//! | --- | --- | --- | --- |
//! | [`SessionStore`] | [`MemorySessionStore`] | [`TableSessionStore`] | [`SESSIONS_TABLE`] |
//! | [`RefreshStore`](crate::RefreshStore) | [`MemoryRefreshStore`](crate::MemoryRefreshStore) | [`TableRefreshStore`] | [`REFRESH_TOKENS_TABLE`] |
//! | [`ApiKeyStore`](crate::ApiKeyStore) | [`MemoryApiKeyStore`](crate::MemoryApiKeyStore) | [`TableApiKeyStore`] | [`API_KEYS_TABLE`] |
//! | `PasskeyStore` (behind `passkeys`) | `MemoryPasskeyStore` | `TablePasskeyStore` | [`PASSKEYS_TABLE`] |
//!
//! [`KvSessionStore`](crate::KvSessionStore) is the fifth session store and the
//! default: it covers Redis, PostgreSQL-as-a-key-value-store and an in-process
//! map, and lives in [`session`](crate::session) beside the rest of the session
//! machinery.
//!
//! # Getting the tables
//!
//! Non-negotiable N6 is that a migration is read before it is run, so nothing
//! here creates a table behind an operator's back. There are two supported ways
//! in, and [`descriptors`] is the one to reach for first:
//!
//! 1. **`moso db make-migration`.** Add [`descriptors`] to the entity list your
//!    project's `src/db.rs` passes to `moso_migrate::command::make_migration`,
//!    and the generator writes the migration, its reverse and the snapshot. From
//!    then on `moso db check` reports drift on these tables like any other.
//! 2. **Copy the DDL.** [`SESSIONS_SCHEMA`], [`REFRESH_TOKENS_SCHEMA`],
//!    [`API_KEYS_SCHEMA`] and [`PASSKEYS_SCHEMA`] are the `create table`
//!    statements, valid on PostgreSQL and SQLite alike, for a project that
//!    writes its migrations by hand.
//!
//! Each table store also has a `create_table()`, which runs the constants. It is
//! for tests and for `moso dev`, not for a deployment.
//!
//! # Why a table at all
//!
//! A session store outage logs everybody out, so it is the one cache an
//! application cannot afford to lose. A table is slower per read than Redis and
//! is backed up, replicated and monitored by machinery the team already has.
//! That is a real trade and Moso does not pick a side.

mod apikey;
#[cfg(feature = "passkeys")]
mod passkey;
mod refresh;
mod schema;
mod sql;

#[cfg(test)]
mod conformance;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_orm::{Db, RawQuery, Row};

use crate::session::{DeviceInfo, SessionId, SessionRecord, SessionStore};
use crate::store::sql::{
    create_objects, fetch, json, placeholder, placeholders, run, stamp, text, text_opt,
    unavailable, unstamp,
};
use crate::{Error, Result};

pub use crate::store::apikey::TableApiKeyStore;
#[cfg(feature = "passkeys")]
#[cfg_attr(docsrs, doc(cfg(feature = "passkeys")))]
pub use crate::store::passkey::{MemoryPasskeyStore, TablePasskeyStore};
pub use crate::store::refresh::TableRefreshStore;
pub use crate::store::schema::{
    API_KEYS_OWNER_INDEX, API_KEYS_SCHEMA, API_KEYS_TABLE, PASSKEYS_SCHEMA, PASSKEYS_TABLE,
    PASSKEYS_USER_INDEX, REFRESH_TOKENS_EXPIRY_INDEX, REFRESH_TOKENS_FAMILY_INDEX,
    REFRESH_TOKENS_SCHEMA, REFRESH_TOKENS_SUBJECT_INDEX, REFRESH_TOKENS_TABLE, descriptors,
};

/// The base64 alphabet the `auth_hash` column is written in.
const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// The table [`TableSessionStore`] reads and writes.
///
/// Named rather than configurable: a session table is not something two
/// applications in one database should share, and a fixed name is what lets
/// `moso-migrate` recognise it and `moso doctor` check it.
///
/// ```
/// assert_eq!(moso_auth::store::SESSIONS_TABLE, "moso_auth_sessions");
/// ```
pub const SESSIONS_TABLE: &str = "moso_auth_sessions";

/// The DDL for [`SESSIONS_TABLE`], valid on PostgreSQL and SQLite alike.
///
/// Every column is `text`: timestamps are RFC 3339 with a fixed width, which
/// compares and sorts lexicographically, and which decodes identically on both
/// backends. A `timestamptz` column would be better on PostgreSQL and a
/// different statement on SQLite, and a session store is not where that
/// complexity earns its keep.
///
/// Non-negotiable N6 says migrations are reviewable and never applied
/// automatically in production, so this is a constant an application copies
/// into its own migration — [`TableSessionStore::create_table`] exists for
/// tests and for `moso dev`.
///
/// ```
/// assert!(moso_auth::store::SESSIONS_SCHEMA.contains("moso_auth_sessions"));
/// ```
pub const SESSIONS_SCHEMA: &str = "\
create table if not exists moso_auth_sessions (
    id           text primary key,
    user_id      text,
    auth_hash    text not null,
    data         text not null,
    created_at   text not null,
    last_seen_at text not null,
    expires_at   text not null,
    user_agent   text,
    ip           text,
    label        text
)";

/// The index that makes the \"your devices\" listing one lookup.
///
/// Without it, listing a user's sessions is a sequential scan of every live
/// session in the deployment, which is the reason most applications do not ship
/// the feature at all.
///
/// ```
/// assert!(moso_auth::store::SESSIONS_USER_INDEX.contains("user_id"));
/// ```
pub const SESSIONS_USER_INDEX: &str =
    "create index if not exists moso_auth_sessions_user_id on moso_auth_sessions (user_id)";

/// The index the expiry sweep uses.
///
/// ```
/// assert!(moso_auth::store::SESSIONS_EXPIRY_INDEX.contains("expires_at"));
/// ```
pub const SESSIONS_EXPIRY_INDEX: &str =
    "create index if not exists moso_auth_sessions_expires_at on moso_auth_sessions (expires_at)";

/// The columns, in the order every statement here reads them.
const COLUMNS: &str =
    "id, user_id, auth_hash, data, created_at, last_seen_at, expires_at, user_agent, ip, label";

/// Sessions in a table.
///
/// ```no_run
/// use moso_auth::store::TableSessionStore;
/// use moso_orm::Db;
///
/// # async fn f() -> moso_auth::Result<()> {
/// let db = Db::connect_url("postgres://moso:moso@localhost/moso").await.unwrap();
/// let store = TableSessionStore::new(db);
/// store.create_table().await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct TableSessionStore {
    /// Where the rows are.
    db: Db,
}

impl TableSessionStore {
    /// A store over `db`.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableSessionStore;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = TableSessionStore::new(db); }
    /// ```
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The database this reads and writes.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableSessionStore;
    /// # use moso_orm::{Backend, Db};
    /// # fn f(store: &TableSessionStore) -> Backend { store.db().backend() }
    /// ```
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Create the table and its indexes, if they are not there.
    ///
    /// For tests and for `moso dev`. A production deployment copies
    /// [`SESSIONS_SCHEMA`] into a reviewed migration instead — non-negotiable
    /// N6 is that a migration is read before it is run.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the statement
    /// cannot be executed.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableSessionStore;
    /// # async fn f(store: &TableSessionStore) -> moso_auth::Result<()> {
    /// store.create_table().await
    /// # }
    /// ```
    pub async fn create_table(&self) -> Result<()> {
        create_objects(
            &self.db,
            "session table",
            &[SESSIONS_SCHEMA, SESSIONS_USER_INDEX, SESSIONS_EXPIRY_INDEX],
        )
        .await
    }

    /// Delete every row that has expired, returning how many went.
    ///
    /// The store never serves an expired row — every read carries the expiry in
    /// its `where` clause — so this is housekeeping rather than correctness. Run
    /// it from a job, or not at all if the table is small.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::store::TableSessionStore;
    /// # async fn f(store: &TableSessionStore) -> moso_auth::Result<u64> {
    /// store.sweep().await
    /// # }
    /// ```
    pub async fn sweep(&self) -> Result<u64> {
        let sql = format!(
            "delete from {SESSIONS_TABLE} where expires_at <= {}",
            self.placeholder(1)
        );
        self.run(RawQuery::new(sql).bind_text(&stamp(Utc::now())))
            .await
    }

    /// How many rows the table holds, expired ones included.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::store::TableSessionStore;
    /// # async fn f(store: &TableSessionStore) -> moso_auth::Result<i64> {
    /// store.len().await
    /// # }
    /// ```
    pub async fn len(&self) -> Result<i64> {
        let rows = self
            .query(RawQuery::new(format!(
                "select count(*) from {SESSIONS_TABLE}"
            )))
            .await?;

        rows.first()
            .map(|row| row.get_i64(0).unwrap_or_default())
            .ok_or_else(|| Error::Unavailable {
                component: "session store",
                detail: "count(*) returned no row".to_owned(),
                source: None,
            })
    }

    /// The `n`th bind placeholder in this backend's spelling.
    ///
    /// PostgreSQL numbers them and SQLite does not; getting this wrong is a
    /// runtime error on one backend and silently the wrong parameter on the
    /// other, which is why it is one function — [`sql::placeholder`], shared
    /// with every other table store here — rather than a literal in each
    /// statement.
    fn placeholder(&self, n: usize) -> String {
        placeholder(self.db.backend(), n)
    }

    /// Run a query and hand back the rows.
    async fn query(&self, query: RawQuery) -> Result<Vec<Row>> {
        fetch(&self.db, "session store", query).await
    }

    /// Run a statement for its effect.
    async fn run(&self, query: RawQuery) -> Result<u64> {
        run(&self.db, "session store", query).await
    }
}

impl SessionStore for TableSessionStore {
    fn load<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<Option<SessionRecord>>> {
        Box::pin(async move {
            let sql = format!(
                "select {COLUMNS} from {SESSIONS_TABLE} where id = {} and expires_at > {}",
                self.placeholder(1),
                self.placeholder(2)
            );

            let rows = self
                .query(
                    RawQuery::new(sql)
                        .bind_text(id.as_str())
                        .bind_text(&stamp(Utc::now())),
                )
                .await?;

            rows.first().map(decode_row).transpose()
        })
    }

    fn save<'a>(&'a self, record: &'a SessionRecord, ttl: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let expires_at = Utc::now()
                + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::days(90));

            // One statement, so a concurrent request on the same session cannot
            // observe the row missing between a delete and an insert.
            let sql = format!(
                "insert into {SESSIONS_TABLE} ({COLUMNS}) values ({}) \
                 on conflict (id) do update set \
                 user_id = excluded.user_id, auth_hash = excluded.auth_hash, \
                 data = excluded.data, created_at = excluded.created_at, \
                 last_seen_at = excluded.last_seen_at, expires_at = excluded.expires_at, \
                 user_agent = excluded.user_agent, ip = excluded.ip, label = excluded.label",
                placeholders(self.db.backend(), 10),
            );

            let query = RawQuery::new(sql)
                .bind_text(record.id.as_str())
                .bind(record.user_id.clone())
                .bind_text(&B64.encode(&record.auth_hash))
                .bind_text(&record.data.to_string())
                .bind_text(&stamp(record.created_at))
                .bind_text(&stamp(record.last_seen_at))
                .bind_text(&stamp(expires_at))
                .bind(record.device.user_agent.clone())
                .bind(record.device.ip.clone())
                .bind(record.device.label.clone());

            self.run(query).await?;
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let sql = format!(
                "delete from {SESSIONS_TABLE} where id = {}",
                self.placeholder(1)
            );
            Ok(self.run(RawQuery::new(sql).bind_text(id.as_str())).await? > 0)
        })
    }

    fn rename<'a>(&'a self, from: &'a SessionId, to: &'a SessionId) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            // A single `update` is the atomicity `cycle_id` needs: the row is
            // never absent, and never present under both identifiers.
            let sql = format!(
                "update {SESSIONS_TABLE} set id = {} where id = {}",
                self.placeholder(1),
                self.placeholder(2)
            );
            self.run(
                RawQuery::new(sql)
                    .bind_text(to.as_str())
                    .bind_text(from.as_str()),
            )
            .await?;
            Ok(())
        })
    }

    fn list_for_user<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionRecord>>> {
        Box::pin(async move {
            let sql = format!(
                "select {COLUMNS} from {SESSIONS_TABLE} where user_id = {} and expires_at > {} \
                 order by last_seen_at desc",
                self.placeholder(1),
                self.placeholder(2)
            );

            let rows = self
                .query(
                    RawQuery::new(sql)
                        .bind_text(user_id)
                        .bind_text(&stamp(Utc::now())),
                )
                .await?;

            rows.iter().map(decode_row).collect()
        })
    }

    fn delete_for_user<'a>(
        &'a self,
        user_id: &'a str,
        except: Option<&'a SessionId>,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            match except {
                Some(keep) => {
                    let sql = format!(
                        "delete from {SESSIONS_TABLE} where user_id = {} and id <> {}",
                        self.placeholder(1),
                        self.placeholder(2)
                    );
                    self.run(
                        RawQuery::new(sql)
                            .bind_text(user_id)
                            .bind_text(keep.as_str()),
                    )
                    .await
                }
                None => {
                    let sql = format!(
                        "delete from {SESSIONS_TABLE} where user_id = {}",
                        self.placeholder(1)
                    );
                    self.run(RawQuery::new(sql).bind_text(user_id)).await
                }
            }
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.db
                .ping()
                .await
                .map_err(|error| unavailable("session store", error))
        })
    }
}

/// Rebuild a record from a row of [`SESSIONS_TABLE`].
fn decode_row(row: &Row) -> Result<SessionRecord> {
    let column = |index: usize, name: &'static str| text("session store", row, index, name);

    let mut record = SessionRecord::new(SessionId::parse(&column(0, "id")?)?);
    record.user_id = text_opt(row, 1);
    record.auth_hash = B64.decode(column(2, "auth_hash")?).unwrap_or_default();
    record.data = json(row, 3, serde_json::Value::Object(serde_json::Map::new()));
    record.created_at = unstamp("session store", &column(4, "created_at")?)?;
    record.last_seen_at = unstamp("session store", &column(5, "last_seen_at")?)?;
    record.device = DeviceInfo {
        user_agent: text_opt(row, 7),
        ip: text_opt(row, 8),
        label: text_opt(row, 9),
    };

    Ok(record)
}

// ---------------------------------------------------------------------------
// MemorySessionStore
// ---------------------------------------------------------------------------

/// Sessions in a map, with counters.
///
/// A real store — expiry, atomic rename, a per-user index — so a test that runs
/// against it is testing the same semantics production gets. The counters are
/// what make "a static endpoint costs zero round trips" an assertion rather
/// than a claim.
///
/// ```
/// use std::time::Duration;
///
/// use moso_auth::store::MemorySessionStore;
/// use moso_auth::{SessionId, SessionRecord, SessionStore};
///
/// # #[tokio::main(flavor = "current_thread")]
/// # async fn main() -> moso_auth::Result<()> {
/// let store = MemorySessionStore::new();
/// let record = SessionRecord::new(SessionId::generate());
///
/// store.save(&record, Duration::from_secs(60)).await?;
/// assert_eq!(store.writes(), 1);
/// assert!(store.load(&record.id).await?.is_some());
/// assert_eq!(store.round_trips(), 2);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Default)]
pub struct MemorySessionStore {
    /// The rows, by identifier, with the instant each expires.
    rows: std::sync::Mutex<HashMap<String, (SessionRecord, DateTime<Utc>)>>,
    /// How many operations reached the store.
    round_trips: AtomicU64,
    /// How many of those were writes.
    writes: AtomicU64,
    /// An artificial delay on [`SessionStore::load`], for concurrency tests.
    load_delay: std::sync::Mutex<Duration>,
}

impl MemorySessionStore {
    /// An empty store.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// assert_eq!(MemorySessionStore::new().round_trips(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty store behind an [`Arc`], which is the shape every caller wants.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    /// use moso_auth::SessionStore;
    ///
    /// let store: std::sync::Arc<dyn SessionStore> = MemorySessionStore::shared();
    /// let _ = store;
    /// ```
    #[must_use]
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// How many operations have reached the store.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// assert_eq!(MemorySessionStore::new().round_trips(), 0);
    /// ```
    #[must_use]
    pub fn round_trips(&self) -> u64 {
        self.round_trips.load(Ordering::Relaxed)
    }

    /// How many of those were writes.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// assert_eq!(MemorySessionStore::new().writes(), 0);
    /// ```
    #[must_use]
    pub fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    /// Set both counters back to zero.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// let store = MemorySessionStore::new();
    /// store.reset_round_trips();
    /// assert_eq!(store.round_trips(), 0);
    /// ```
    pub fn reset_round_trips(&self) {
        self.round_trips.store(0, Ordering::Relaxed);
        self.writes.store(0, Ordering::Relaxed);
    }

    /// Make every load take `delay`, so a test can prove that two concurrent
    /// loads share one round trip rather than racing to two.
    ///
    /// ```
    /// use std::time::Duration;
    ///
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// let store = MemorySessionStore::new();
    /// store.set_load_delay(Duration::from_millis(5));
    /// ```
    pub fn set_load_delay(&self, delay: Duration) {
        *self
            .load_delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = delay;
    }

    /// How many live sessions the store holds.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// assert_eq!(MemorySessionStore::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.lock().map(|rows| rows.len()).unwrap_or_default()
    }

    /// Whether the store holds nothing.
    ///
    /// ```
    /// use moso_auth::store::MemorySessionStore;
    ///
    /// assert!(MemorySessionStore::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Count one operation.
    fn touch(&self, write: bool) {
        self.round_trips.fetch_add(1, Ordering::Relaxed);
        if write {
            self.writes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The rows, with poisoning recovered from.
    fn rows(&self) -> std::sync::MutexGuard<'_, HashMap<String, (SessionRecord, DateTime<Utc>)>> {
        self.rows
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl SessionStore for MemorySessionStore {
    fn load<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<Option<SessionRecord>>> {
        Box::pin(async move {
            self.touch(false);

            let delay = *self
                .load_delay
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }

            let now = Utc::now();
            let mut rows = self.rows();
            match rows.get(id.as_str()) {
                Some((_, expires)) if *expires <= now => {
                    rows.remove(id.as_str());
                    Ok(None)
                }
                Some((record, _)) => Ok(Some(record.clone())),
                None => Ok(None),
            }
        })
    }

    fn save<'a>(&'a self, record: &'a SessionRecord, ttl: Duration) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.touch(true);
            let expires = Utc::now()
                + chrono::Duration::from_std(ttl).unwrap_or_else(|_| chrono::Duration::days(90));
            self.rows()
                .insert(record.id.as_str().to_owned(), (record.clone(), expires));
            Ok(())
        })
    }

    fn delete<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            self.touch(true);
            Ok(self.rows().remove(id.as_str()).is_some())
        })
    }

    fn rename<'a>(&'a self, from: &'a SessionId, to: &'a SessionId) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.touch(true);
            let mut rows = self.rows();
            if let Some((mut record, expires)) = rows.remove(from.as_str()) {
                record.id = to.clone();
                rows.insert(to.as_str().to_owned(), (record, expires));
            }
            Ok(())
        })
    }

    fn list_for_user<'a>(&'a self, user_id: &'a str) -> BoxFuture<'a, Result<Vec<SessionRecord>>> {
        Box::pin(async move {
            self.touch(false);
            let now = Utc::now();
            let mut found: Vec<SessionRecord> = self
                .rows()
                .values()
                .filter(|(record, expires)| {
                    *expires > now && record.user_id.as_deref() == Some(user_id)
                })
                .map(|(record, _)| record.clone())
                .collect();

            found.sort_by_key(|record| core::cmp::Reverse(record.last_seen_at));
            Ok(found)
        })
    }

    fn delete_for_user<'a>(
        &'a self,
        user_id: &'a str,
        except: Option<&'a SessionId>,
    ) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            self.touch(true);
            let mut rows = self.rows();
            let doomed: Vec<String> = rows
                .iter()
                .filter(|(id, (record, _))| {
                    record.user_id.as_deref() == Some(user_id)
                        && except.is_none_or(|keep| keep.as_str() != id.as_str())
                })
                .map(|(id, _)| id.clone())
                .collect();

            let count = doomed.len() as u64;
            for id in doomed {
                rows.remove(&id);
            }
            Ok(count)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionConfig;

    // ── the in-memory store ───────────────────────────────────────────────

    #[tokio::test]
    async fn the_memory_store_counts_what_it_was_asked_to_do() {
        let store = MemorySessionStore::new();
        let record = SessionRecord::new(SessionId::generate());

        store.save(&record, Duration::from_secs(60)).await.unwrap();
        store.load(&record.id).await.unwrap();
        store.load(&record.id).await.unwrap();

        assert_eq!(store.round_trips(), 3);
        assert_eq!(store.writes(), 1);

        store.reset_round_trips();
        assert_eq!(store.round_trips(), 0);
    }

    #[tokio::test]
    async fn the_memory_store_expires_a_row_rather_than_serving_it() {
        let store = MemorySessionStore::new();
        let record = SessionRecord::new(SessionId::generate());

        store
            .save(&record, Duration::from_millis(30))
            .await
            .unwrap();
        assert!(store.load(&record.id).await.unwrap().is_some());

        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(store.load(&record.id).await.unwrap().is_none());
        assert!(store.is_empty(), "and the row is gone, not merely hidden");
    }

    #[tokio::test]
    async fn the_memory_store_renames_without_losing_the_row() {
        let store = MemorySessionStore::new();
        let from = SessionId::generate();
        let to = SessionId::generate();
        let mut record = SessionRecord::new(from.clone());
        record.data = serde_json::json!({ "cart": 1 });
        store.save(&record, Duration::from_secs(60)).await.unwrap();

        store.rename(&from, &to).await.unwrap();

        assert!(store.load(&from).await.unwrap().is_none());
        let moved = store.load(&to).await.unwrap().unwrap();
        assert_eq!(moved.id, to);
        assert_eq!(moved.data, serde_json::json!({ "cart": 1 }));
    }

    #[tokio::test]
    async fn the_memory_store_lists_and_revokes_a_users_sessions() {
        let store = MemorySessionStore::new();

        let mut ids = Vec::new();
        for _ in 0..3 {
            let mut record = SessionRecord::new(SessionId::generate());
            record.user_id = Some("usr_1".to_owned());
            store.save(&record, Duration::from_secs(60)).await.unwrap();
            ids.push(record.id);
        }

        let mut other = SessionRecord::new(SessionId::generate());
        other.user_id = Some("usr_2".to_owned());
        store.save(&other, Duration::from_secs(60)).await.unwrap();

        assert_eq!(store.list_for_user("usr_1").await.unwrap().len(), 3);
        assert_eq!(
            store.delete_for_user("usr_1", Some(&ids[0])).await.unwrap(),
            2
        );
        assert_eq!(store.list_for_user("usr_1").await.unwrap().len(), 1);
        assert_eq!(
            store.list_for_user("usr_2").await.unwrap().len(),
            1,
            "another user's sessions are untouched"
        );
    }

    #[tokio::test]
    async fn the_memory_store_probes_healthy() {
        MemorySessionStore::new().probe().await.unwrap();
    }

    // ── the table-backed store ────────────────────────────────────────────

    /// A SQLite database with the session table, for the dialect-independent
    /// half of the table store's behaviour.
    async fn sqlite_store() -> TableSessionStore {
        let db = Db::connect_url("sqlite://:memory:")
            .await
            .expect("the bundled SQLite is always available");
        let store = TableSessionStore::new(db);
        store.create_table().await.unwrap();
        store
    }

    /// A PostgreSQL database, or `None` when `DATABASE_URL` is not set.
    ///
    /// Every table test runs twice: once on SQLite, which needs nothing, and
    /// once on the real server, which is where a dialect divergence would show.
    async fn postgres_store() -> Option<TableSessionStore> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let db = Db::connect_url(&url)
            .await
            .expect("DATABASE_URL is set but the server did not accept a connection");
        let store = TableSessionStore::new(db);
        store.create_table().await.unwrap();
        Some(store)
    }

    /// Run `body` against SQLite, and against PostgreSQL when it is available.
    async fn on_both<F, Fut>(body: F)
    where
        F: Fn(TableSessionStore) -> Fut,
        Fut: Future<Output = ()>,
    {
        body(sqlite_store().await).await;

        match postgres_store().await {
            Some(store) => body(store).await,
            None => eprintln!(
                "skipping the PostgreSQL half: DATABASE_URL is not set. Start the test server \
                 with `scripts/test-db.sh` and re-run."
            ),
        }
    }

    #[tokio::test]
    async fn the_table_store_round_trips_a_record_on_both_dialects() {
        on_both(|store| async move {
            let id = SessionId::generate();
            let mut record = SessionRecord::new(id.clone());
            record.user_id = Some("usr_1".to_owned());
            record.auth_hash = vec![1, 2, 3, 250];
            record.data = serde_json::json!({ "locale": "it-IT", "count": 7 });
            record.device = DeviceInfo::from_request(Some("curl/8.4.0"), Some("203.0.113.7"));

            store.save(&record, Duration::from_secs(600)).await.unwrap();

            let loaded = store.load(&id).await.unwrap().unwrap();
            assert_eq!(loaded.id, id);
            assert_eq!(loaded.user_id.as_deref(), Some("usr_1"));
            assert_eq!(loaded.auth_hash, vec![1, 2, 3, 250]);
            assert_eq!(loaded.data["locale"], "it-IT");
            assert_eq!(loaded.device.ip.as_deref(), Some("203.0.113.7"));
            assert_eq!(loaded.device.label.as_deref(), Some("curl"));
            assert!(
                (loaded.created_at - record.created_at)
                    .num_milliseconds()
                    .abs()
                    < 1
            );

            store.delete(&id).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_table_store_upserts_rather_than_duplicating() {
        on_both(|store| async move {
            let id = SessionId::generate();
            let mut record = SessionRecord::new(id.clone());
            record.user_id = Some("usr_upsert".to_owned());

            store.save(&record, Duration::from_secs(600)).await.unwrap();
            record.data = serde_json::json!({ "second": true });
            store.save(&record, Duration::from_secs(600)).await.unwrap();

            let listed = store.list_for_user("usr_upsert").await.unwrap();
            assert_eq!(listed.len(), 1, "a second save must update, not insert");
            assert_eq!(listed[0].data["second"], true);

            store.delete_for_user("usr_upsert", None).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_table_store_never_serves_an_expired_row() {
        on_both(|store| async move {
            let id = SessionId::generate();
            let mut record = SessionRecord::new(id.clone());
            record.user_id = Some("usr_expiry".to_owned());

            store.save(&record, Duration::from_millis(1)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;

            assert!(store.load(&id).await.unwrap().is_none());
            assert!(
                store.list_for_user("usr_expiry").await.unwrap().is_empty(),
                "an expired session must not appear in the listing either"
            );

            assert!(store.sweep().await.unwrap() >= 1);
            store.delete(&id).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_table_store_renames_in_one_statement() {
        on_both(|store| async move {
            let from = SessionId::generate();
            let to = SessionId::generate();
            let mut record = SessionRecord::new(from.clone());
            record.user_id = Some("usr_rename".to_owned());
            store.save(&record, Duration::from_secs(600)).await.unwrap();

            store.rename(&from, &to).await.unwrap();

            assert!(store.load(&from).await.unwrap().is_none());
            assert_eq!(store.load(&to).await.unwrap().unwrap().id, to);

            store.delete_for_user("usr_rename", None).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_table_store_revokes_everything_but_the_current_session() {
        on_both(|store| async move {
            let mut ids = Vec::new();
            for _ in 0..4 {
                let mut record = SessionRecord::new(SessionId::generate());
                record.user_id = Some("usr_revoke".to_owned());
                store.save(&record, Duration::from_secs(600)).await.unwrap();
                ids.push(record.id);
            }

            let keep = ids[0].clone();
            assert_eq!(
                store
                    .delete_for_user("usr_revoke", Some(&keep))
                    .await
                    .unwrap(),
                3
            );

            let remaining = store.list_for_user("usr_revoke").await.unwrap();
            assert_eq!(remaining.len(), 1);
            assert_eq!(remaining[0].id, keep);

            store.delete_for_user("usr_revoke", None).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_table_store_orders_the_listing_by_last_seen() {
        on_both(|store| async move {
            let older = SessionId::generate();
            let newer = SessionId::generate();

            let mut first = SessionRecord::new(older.clone());
            first.user_id = Some("usr_order".to_owned());
            first.last_seen_at = Utc::now() - chrono::Duration::hours(3);
            store.save(&first, Duration::from_secs(600)).await.unwrap();

            let mut second = SessionRecord::new(newer.clone());
            second.user_id = Some("usr_order".to_owned());
            store.save(&second, Duration::from_secs(600)).await.unwrap();

            let listed = store.list_for_user("usr_order").await.unwrap();
            assert_eq!(listed.len(), 2);
            assert_eq!(listed[0].id, newer, "most recently used first");

            store.delete_for_user("usr_order", None).await.unwrap();
        })
        .await;
    }

    #[tokio::test]
    async fn the_table_store_probes_healthy() {
        on_both(|store| async move {
            store.probe().await.unwrap();
            assert!(store.len().await.unwrap() >= 0);
        })
        .await;
    }

    /// The whole point of having two stores: a [`Session`]
    /// cannot tell them apart.
    #[tokio::test]
    async fn a_session_behaves_identically_over_either_store() {
        let stores: Vec<Arc<dyn SessionStore>> = vec![
            MemorySessionStore::shared(),
            Arc::new(sqlite_store().await),
            Arc::new(crate::KvSessionStore::new(
                moso_kv::Kv::in_memory("auth-parity").unwrap(),
            )),
        ];

        for store in stores {
            let session = crate::Session::detached(Arc::clone(&store), SessionConfig::default());
            session.load().await.unwrap();
            session.insert("locale", "it-IT").unwrap();
            session
                .log_in(&crate::DefaultUser::new("usr_parity", b"epoch".to_vec()))
                .await
                .unwrap();
            session.save().await.unwrap();

            let id = session.id();
            let reloaded = store.load(&id).await.unwrap().unwrap();
            assert_eq!(reloaded.user_id.as_deref(), Some("usr_parity"));
            assert_eq!(reloaded.data["locale"], "it-IT");
            assert_eq!(reloaded.auth_hash, b"epoch".to_vec());

            assert_eq!(store.list_for_user("usr_parity").await.unwrap().len(), 1);
            assert_eq!(store.delete_for_user("usr_parity", None).await.unwrap(), 1);
        }
    }
}
