//! API keys in a table, looked up by the public prefix and never by the secret.
//!
//! [`MemoryApiKeyStore`](crate::MemoryApiKeyStore) is complete and is what an
//! application uses before it has a database. It is also per-process, so a key
//! minted on instance A does not authenticate on instance B. [`TableApiKeyStore`]
//! is the same store with the rows somewhere both instances can see.
//!
//! # The lookup is by prefix, and that is a security decision
//!
//! ```text
//!   mso_live_0123abcd_xLm…9Qw
//!   └┬─┘ └┬─┘ └──┬───┘ └──┬──┘
//!    │    │      │        └── the secret. Never stored, never queried.
//!    │    │      └─────────── the prefix. Stored in the clear, unique, indexed.
//!    │    └────────────────── the environment.
//!    └─────────────────────── the brand, so secret scanners can match it.
//! ```
//!
//! The obvious implementation — `where hash = $1` — is a timing oracle. The
//! database compares the bytes with `memcmp`, which returns as soon as it finds
//! a difference, and the index makes the *shape* of the search observable too.
//! An attacker who can measure that is recovering the stored hash one byte at a
//! time.
//!
//! So the statement filters on `prefix`, which is public by construction, and
//! the secret is checked afterwards in Rust by
//! [`ApiKey::verify_secret`](crate::ApiKey::verify_secret), which is
//! `subtle::ConstantTimeEq` over the whole hash. The `hash` column carries no
//! index and appears in no `where` clause anywhere in this file, and
//! `schema.rs`'s own test asserts that it never will.
//!
//! # Doctests
//!
//! The examples that need a database are `no_run`: they compile on every machine
//! and connect on none, because a doctest that needs a PostgreSQL server would
//! fail on a laptop rather than teach anything.

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_orm::{Db, RawQuery, Row};
use moso_schema::Id;

use crate::apikey::{ApiKey, ApiKeyStore, KeyEnvironment};
use crate::store::schema::{API_KEYS_OWNER_INDEX, API_KEYS_SCHEMA, API_KEYS_TABLE};
use crate::store::sql::{
    create_objects, encode_strings, fetch, malformed, placeholder, placeholders, run, stamp,
    string_array, text, text_opt, unavailable, unstamp, unstamp_opt,
};
use crate::{Error, Result};

/// What this store is called when it cannot be reached.
const COMPONENT: &str = "api key store";

/// The columns, in the order every statement here reads them.
const COLUMNS: &str = "id, prefix, hash, environment, name, owner, scopes, created_at, \
                       expires_at, last_used_at, revoked_at";

/// API keys in a table.
///
/// ```no_run
/// use moso_auth::store::TableApiKeyStore;
/// use moso_auth::{ApiKey, ApiKeyStore, KeyEnvironment};
/// use moso_orm::Db;
///
/// # async fn example() -> moso_auth::Result<()> {
/// let db = Db::connect_url("postgres://moso:moso@localhost/moso").await.unwrap();
/// let store = TableApiKeyStore::new(db);
/// store.create_table().await?;
///
/// let new = ApiKey::generate("ci", "usr_1", KeyEnvironment::Live)?;
/// store.insert(&new.record).await?;
/// assert!(store.find_by_prefix(&new.record.prefix).await?.is_some());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug)]
pub struct TableApiKeyStore {
    /// Where the rows are.
    db: Db,
}

impl TableApiKeyStore {
    /// A store over `db`.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableApiKeyStore;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = TableApiKeyStore::new(db); }
    /// ```
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self { db }
    }

    /// The database this reads and writes.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableApiKeyStore;
    /// # use moso_orm::{Backend, Db};
    /// # fn f(store: &TableApiKeyStore) -> Backend { store.db().backend() }
    /// ```
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Create the table and its index, if they are not there.
    ///
    /// For tests and for `moso dev`. A production deployment puts
    /// [`API_KEYS_SCHEMA`](crate::store::API_KEYS_SCHEMA) in a reviewed
    /// migration instead, or lets [`descriptors`](crate::store::descriptors)
    /// hand the table to `moso db make-migration`.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the statement cannot be executed.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableApiKeyStore;
    /// # async fn f(store: &TableApiKeyStore) -> moso_auth::Result<()> {
    /// store.create_table().await
    /// # }
    /// ```
    pub async fn create_table(&self) -> Result<()> {
        create_objects(
            &self.db,
            COMPONENT,
            &[API_KEYS_SCHEMA, API_KEYS_OWNER_INDEX],
        )
        .await
    }

    /// How many keys are on record, revoked ones included.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::store::TableApiKeyStore;
    /// # async fn f(store: &TableApiKeyStore) -> moso_auth::Result<i64> {
    /// store.len().await
    /// # }
    /// ```
    pub async fn len(&self) -> Result<i64> {
        let rows = fetch(
            &self.db,
            COMPONENT,
            RawQuery::new(format!("select count(*) from {API_KEYS_TABLE}")),
        )
        .await?;
        rows.first()
            .map(|row| row.get_i64(0).unwrap_or_default())
            .ok_or_else(|| Error::Unavailable {
                component: COMPONENT,
                detail: "count(*) returned no row".to_owned(),
                source: None,
            })
    }

    /// The `n`th bind placeholder in this backend's spelling.
    fn placeholder(&self, n: usize) -> String {
        placeholder(self.db.backend(), n)
    }

    /// Read one column list back.
    async fn select(&self, predicate: &str, value: &str, order: &str) -> Result<Vec<ApiKey>> {
        let sql = format!(
            "select {COLUMNS} from {API_KEYS_TABLE} where {predicate} = {}{order}",
            self.placeholder(1)
        );
        let rows = fetch(&self.db, COMPONENT, RawQuery::new(sql).bind_text(value)).await?;
        rows.iter().map(decode_row).collect()
    }
}

impl ApiKeyStore for TableApiKeyStore {
    fn insert<'a>(&'a self, key: &'a ApiKey) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let sql = format!(
                "insert into {API_KEYS_TABLE} ({COLUMNS}) values ({})",
                placeholders(self.db.backend(), 11)
            );
            let scopes = encode_strings(COMPONENT, &key.scopes)?;
            let query = RawQuery::new(sql)
                .bind_text(&key.id.to_string())
                .bind_text(&key.prefix)
                .bind_text(&key.hash)
                .bind_text(key.environment.as_str())
                .bind_text(&key.name)
                .bind_text(&key.owner)
                .bind_text(&scopes)
                .bind_text(&stamp(key.created_at))
                .bind(key.expires_at.map(stamp))
                .bind(key.last_used_at.map(stamp))
                .bind(key.revoked_at.map(stamp));

            match query.execute(&self.db).await {
                Ok(_) => Ok(()),
                // The unique index on `prefix` is what the caller's retry loop
                // reads, and it is the same answer the in-memory store gives, so
                // one retry loop serves both.
                Err(moso_orm::Error::UniqueViolation(_)) => Err(malformed(
                    COMPONENT,
                    format!("the prefix `{}` is already taken", key.prefix),
                )),
                Err(error) => Err(unavailable(COMPONENT, error)),
            }
        })
    }

    fn find_by_prefix<'a>(&'a self, prefix: &'a str) -> BoxFuture<'a, Result<Option<ApiKey>>> {
        Box::pin(async move {
            // One indexed lookup on a public column. The secret is verified
            // afterwards, in constant time, by the caller.
            Ok(self.select("prefix", prefix, "").await?.into_iter().next())
        })
    }

    fn list_for_owner<'a>(&'a self, owner: &'a str) -> BoxFuture<'a, Result<Vec<ApiKey>>> {
        Box::pin(async move { self.select("owner", owner, " order by created_at").await })
    }

    fn revoke<'a>(&'a self, id: Id<ApiKey>) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            // A compare-and-set, so two operators revoking at once produce one
            // `true` and one `false` rather than two `true`s and two audit
            // entries. The row is kept: an audit trail that cannot resolve a key
            // id is not an audit trail.
            let sql = format!(
                "update {API_KEYS_TABLE} set revoked_at = {} where id = {} and revoked_at is null",
                self.placeholder(1),
                self.placeholder(2)
            );
            let affected = run(
                &self.db,
                COMPONENT,
                RawQuery::new(sql)
                    .bind_text(&stamp(Utc::now()))
                    .bind_text(&id.to_string()),
            )
            .await?;
            Ok(affected > 0)
        })
    }

    fn touch<'a>(&'a self, id: Id<ApiKey>, at: DateTime<Utc>) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // Lossy by contract. The caller has already rate-limited this to at
            // most once a minute per key and spawned it off the request path, so
            // the only thing left to decide is what a failure means: a lost
            // timestamp, logged once, and never a failed request.
            let sql = format!(
                "update {API_KEYS_TABLE} set last_used_at = {} where id = {}",
                self.placeholder(1),
                self.placeholder(2)
            );
            let outcome = RawQuery::new(sql)
                .bind_text(&stamp(at))
                .bind_text(&id.to_string())
                .execute(&self.db)
                .await;
            if let Err(error) = outcome {
                tracing::debug!(
                    target: "moso_auth::apikey",
                    key = %id,
                    error = %error,
                    "an api key's last-used timestamp was dropped"
                );
            }
        })
    }
}

/// Rebuild a key from a row of [`API_KEYS_TABLE`].
fn decode_row(row: &Row) -> Result<ApiKey> {
    let id = Id::parse(&text(COMPONENT, row, 0, "id")?)
        .map_err(|error| malformed(COMPONENT, format!("column `id` is not an id: {error}")))?;
    let environment_text = text(COMPONENT, row, 3, "environment")?;
    let environment = KeyEnvironment::parse(&environment_text).ok_or_else(|| {
        malformed(
            COMPONENT,
            format!("column `environment` holds `{environment_text}`, which is not one"),
        )
    })?;

    Ok(ApiKey {
        id,
        prefix: text(COMPONENT, row, 1, "prefix")?,
        hash: text(COMPONENT, row, 2, "hash")?,
        environment,
        name: text(COMPONENT, row, 4, "name")?,
        owner: text(COMPONENT, row, 5, "owner")?,
        scopes: string_array(row, 6),
        created_at: unstamp(COMPONENT, &text(COMPONENT, row, 7, "created_at")?)?,
        expires_at: unstamp_opt(COMPONENT, text_opt(row, 8))?,
        last_used_at: unstamp_opt(COMPONENT, text_opt(row, 9))?,
        revoked_at: unstamp_opt(COMPONENT, text_opt(row, 10))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::conformance::{on_every_backend, unique};

    /// A store over `db`, with its table created.
    async fn store(db: Db) -> TableApiKeyStore {
        let store = TableApiKeyStore::new(db);
        store.create_table().await.expect("the table is created");
        store
    }

    /// The one query on the request path must name the public prefix and
    /// nothing else. A `where hash = $1` would make the database's own
    /// comparison — `memcmp`, which returns at the first differing byte — the
    /// timing oracle that the constant-time check exists to avoid, and no
    /// behavioural test can observe the difference. So this one reads the
    /// statements instead, over code lines only, because the prose above them
    /// has to be free to explain what it is forbidding.
    #[test]
    fn no_statement_in_this_store_filters_on_the_secret_hash() {
        let offending: Vec<&str> = include_str!("apikey.rs")
            .lines()
            .take_while(|line| !line.starts_with("mod tests"))
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains("where hash") || line.contains("hash = {"))
            .collect();
        assert!(
            offending.is_empty(),
            "the secret hash must never appear in a predicate: {offending:?}"
        );
    }

    #[tokio::test]
    async fn a_revoked_key_is_kept_and_still_resolves_for_an_audit() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let owner = unique("usr");
            let new = ApiKey::generate("audit", &owner, KeyEnvironment::Live)
                .expect("generated")
                .record;
            store.insert(&new).await.expect("inserted");

            assert!(store.revoke(new.id).await.expect("revoked"));
            let kept = store
                .find_by_prefix(&new.prefix)
                .await
                .expect("looked up")
                .expect("a revoked key is still resolvable");
            assert_eq!(kept.id, new.id);
            assert_eq!(kept.name, "audit");
            assert!(kept.revoked_at.is_some());
        })
        .await;
    }

    #[tokio::test]
    async fn a_row_with_an_unreadable_environment_is_an_outage_not_a_credential_failure() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let owner = unique("usr");
            let new = ApiKey::generate("broken", &owner, KeyEnvironment::Live)
                .expect("generated")
                .record;
            store.insert(&new).await.expect("inserted");

            let sql = format!(
                "update {API_KEYS_TABLE} set environment = {} where id = {}",
                store.placeholder(1),
                store.placeholder(2)
            );
            run(
                &store.db,
                COMPONENT,
                RawQuery::new(sql)
                    .bind_text("staging")
                    .bind_text(&new.id.to_string()),
            )
            .await
            .expect("rewritten");

            let error = store
                .find_by_prefix(&new.prefix)
                .await
                .expect_err("an unreadable row is refused");
            assert!(
                matches!(error, Error::Unavailable { .. }),
                "a store that cannot read its own row is down, not saying no: {error}"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn scopes_survive_the_round_trip_including_the_empty_set() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let owner = unique("usr");

            let scoped = ApiKey::generate("scoped", &owner, KeyEnvironment::Live)
                .expect("generated")
                .with_scopes(["posts:read", "posts:write", "users:read"])
                .record;
            let bare = ApiKey::generate("bare", &owner, KeyEnvironment::Test)
                .expect("generated")
                .record;
            store.insert(&scoped).await.expect("inserted");
            store.insert(&bare).await.expect("inserted");

            let found = store
                .find_by_prefix(&scoped.prefix)
                .await
                .expect("looked up")
                .expect("there");
            assert_eq!(found.scopes, scoped.scopes);
            assert!(found.has_scope("users:read"));

            let found = store
                .find_by_prefix(&bare.prefix)
                .await
                .expect("looked up")
                .expect("there");
            assert!(found.scopes.is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn the_count_reflects_what_was_written() {
        let store = store(crate::store::conformance::sqlite_db().await).await;
        assert_eq!(store.len().await.expect("counted"), 0);

        let owner = unique("usr");
        for index in 0..3 {
            let key = ApiKey::generate(format!("key {index}"), &owner, KeyEnvironment::Live)
                .expect("generated")
                .record;
            store.insert(&key).await.expect("inserted");
        }
        assert_eq!(store.len().await.expect("counted"), 3);
    }
}
