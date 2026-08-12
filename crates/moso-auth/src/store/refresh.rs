//! Refresh-token families in a table, where the reuse detection is a
//! compare-and-set.
//!
//! [`MemoryRefreshStore`](crate::MemoryRefreshStore) is a complete
//! implementation and it is correct for exactly one process: the atomicity
//! [`RefreshStore::exchange`] demands is a `std::sync::Mutex`, and a mutex in
//! process A says nothing to process B. Two instances behind a load balancer,
//! each with its own map, is three bugs at once: a token issued by one is
//! unknown to the other, reuse detection sees half the traffic, and revoking a
//! family revokes it on one instance.
//!
//! [`TableRefreshStore`] is the same semantics with the database doing the
//! serialising.
//!
//! # The one statement that matters
//!
//! ```text
//! update moso_auth_refresh_tokens
//!    set used = $1                       -- true
//!  where token_hash = $2
//!    and used       = $3                 -- false
//!    and expires_at > $4                 -- now
//! ```
//!
//! The **affected row count is the answer**. One means this caller claimed the
//! token and nobody else can; zero means somebody else already did, or it never
//! existed, or it had expired — and a second, cheap read decides which. There is
//! no window between the read and the write in which a second process can also
//! decide it won, because there is no read.
//!
//! A read-then-write here would race exactly when it matters. The legitimate
//! client and an attacker holding a stolen copy present the same token at
//! roughly the same moment; that is the entire scenario reuse detection exists
//! for, and it is the one a read-then-write gets wrong.
//!
//! # Why the exchange runs in a transaction
//!
//! The compare-and-set alone leaves one window. The winner marks the parent used
//! and then inserts the child; the loser sees `used = true` and burns the
//! family. If the loser's `delete` lands between those two statements, the child
//! survives a family revocation. Wrapping the whole exchange in a transaction
//! closes it: on PostgreSQL the loser's `update` blocks on the winner's row lock
//! and only proceeds once the winner has committed the child, so the burn always
//! sees the whole family.
//!
//! ```text
//!   P1  BEGIN ── update(1 row) ── insert child ── COMMIT
//!   P2  BEGIN ──────── update ▓▓▓ blocked ▓▓▓ ── (0 rows) ── select ── delete family ── COMMIT
//! ```
//!
//! # Doctests
//!
//! The examples that need a database are `no_run`: they compile on every machine
//! and connect on none, because a doctest that needs a PostgreSQL server would
//! fail on a laptop rather than teach anything.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_orm::{Db, RawQuery, Tx};

use crate::jwt::{Claims, Jwt, RefreshOutcome, RefreshStore, RefreshToken};
use crate::store::schema::{
    REFRESH_TOKENS_EXPIRY_INDEX, REFRESH_TOKENS_FAMILY_INDEX, REFRESH_TOKENS_SCHEMA,
    REFRESH_TOKENS_SUBJECT_INDEX, REFRESH_TOKENS_TABLE,
};
use crate::store::sql::{
    create_objects, fetch, flag, placeholder, placeholders, run, stamp, text, unavailable,
};
use crate::{Error, Result};

/// What this store is called when it cannot be reached.
const COMPONENT: &str = "refresh store";

/// The columns, in the order every statement here reads them.
const COLUMNS: &str = "token_hash, family, subject, issued_at, expires_at, used";

/// Refresh-token families in a table.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_auth::store::TableRefreshStore;
/// use moso_auth::{Claims, Jwt, RefreshStore};
/// use moso_orm::Db;
///
/// # async fn example(jwt: Arc<Jwt<Claims>>) -> moso_auth::Result<()> {
/// let db = Db::connect_url("postgres://moso:moso@localhost/moso").await.unwrap();
/// let store = TableRefreshStore::new(db, jwt);
/// store.create_table().await?;
///
/// let first = store.issue("usr_1", std::time::Duration::from_secs(3600)).await?;
/// let _rotated = store.exchange(first.expose()).await?;
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct TableRefreshStore {
    /// Where the rows are.
    db: Db,
    /// Mints the access-token half of a rotation.
    jwt: Arc<Jwt<Claims>>,
}

impl core::fmt::Debug for TableRefreshStore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The signing key is behind `Jwt`'s own redacting `Debug`; the database
        // handle's `Debug` does not print its URL. Neither is restated here.
        f.debug_struct("TableRefreshStore")
            .field("backend", &self.db.backend())
            .field("table", &REFRESH_TOKENS_TABLE)
            .finish_non_exhaustive()
    }
}

impl TableRefreshStore {
    /// A store over `db`, minting its access tokens with `jwt`.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_auth::store::TableRefreshStore;
    /// # use moso_auth::{Claims, Jwt};
    /// # use moso_orm::Db;
    /// # fn f(db: Db, jwt: Arc<Jwt<Claims>>) { let _ = TableRefreshStore::new(db, jwt); }
    /// ```
    #[must_use]
    pub fn new(db: Db, jwt: Arc<Jwt<Claims>>) -> Self {
        Self { db, jwt }
    }

    /// The database this reads and writes.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableRefreshStore;
    /// # use moso_orm::{Backend, Db};
    /// # fn f(store: &TableRefreshStore) -> Backend { store.db().backend() }
    /// ```
    #[must_use]
    pub fn db(&self) -> &Db {
        &self.db
    }

    /// Create the table and its indexes, if they are not there.
    ///
    /// For tests and for `moso dev`. A production deployment puts
    /// [`REFRESH_TOKENS_SCHEMA`](crate::store::REFRESH_TOKENS_SCHEMA) in a
    /// reviewed migration instead, or lets
    /// [`descriptors`](crate::store::descriptors) hand the table to
    /// `moso db make-migration` — non-negotiable N6 is that a migration is read
    /// before it is run.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] when the statement cannot be executed.
    ///
    /// ```no_run
    /// # use moso_auth::store::TableRefreshStore;
    /// # async fn f(store: &TableRefreshStore) -> moso_auth::Result<()> {
    /// store.create_table().await
    /// # }
    /// ```
    pub async fn create_table(&self) -> Result<()> {
        create_objects(
            &self.db,
            COMPONENT,
            &[
                REFRESH_TOKENS_SCHEMA,
                REFRESH_TOKENS_FAMILY_INDEX,
                REFRESH_TOKENS_SUBJECT_INDEX,
                REFRESH_TOKENS_EXPIRY_INDEX,
            ],
        )
        .await
    }

    /// Delete every token that has expired, returning how many went.
    ///
    /// Housekeeping rather than correctness: the compare-and-set carries the
    /// expiry in its `where` clause, so an expired token never rotates whether
    /// or not this ever runs.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::store::TableRefreshStore;
    /// # async fn f(store: &TableRefreshStore) -> moso_auth::Result<u64> {
    /// store.sweep().await
    /// # }
    /// ```
    pub async fn sweep(&self) -> Result<u64> {
        let sql = format!(
            "delete from {REFRESH_TOKENS_TABLE} where expires_at <= {}",
            self.placeholder(1)
        );
        run(
            &self.db,
            COMPONENT,
            RawQuery::new(sql).bind_text(&stamp(Utc::now())),
        )
        .await
    }

    /// How many tokens are on record, used and expired ones included.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`].
    ///
    /// ```no_run
    /// # use moso_auth::store::TableRefreshStore;
    /// # async fn f(store: &TableRefreshStore) -> moso_auth::Result<i64> {
    /// store.len().await
    /// # }
    /// ```
    pub async fn len(&self) -> Result<i64> {
        let rows = fetch(
            &self.db,
            COMPONENT,
            RawQuery::new(format!("select count(*) from {REFRESH_TOKENS_TABLE}")),
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

    /// Write one freshly minted token.
    async fn insert(&self, executor: &Tx, token: &RefreshToken, now: DateTime<Utc>) -> Result<()> {
        let sql = format!(
            "insert into {REFRESH_TOKENS_TABLE} ({COLUMNS}) values ({})",
            placeholders(self.db.backend(), 6)
        );
        let query = RawQuery::new(sql)
            .bind_text(&token.hash())
            .bind_text(&token.family)
            .bind_text(&token.subject)
            .bind_text(&stamp(now))
            .bind_text(&stamp(token.expires_at))
            .bind(false);
        run(executor, COMPONENT, query).await?;
        Ok(())
    }

    /// Claim `hash` for this caller, or find out why we could not.
    ///
    /// Everything here runs on `tx` — never on `self.db` — because a statement
    /// that escaped the transaction would be the window the transaction exists
    /// to close, and on a single-connection SQLite pool it would deadlock.
    async fn claim(&self, tx: &Tx, hash: &str, now: DateTime<Utc>) -> Result<Claim> {
        let stamped = stamp(now);
        let cas = format!(
            "update {REFRESH_TOKENS_TABLE} set used = {} \
             where token_hash = {} and used = {} and expires_at > {}",
            self.placeholder(1),
            self.placeholder(2),
            self.placeholder(3),
            self.placeholder(4),
        );
        let claimed = run(
            tx,
            COMPONENT,
            RawQuery::new(cas)
                .bind(true)
                .bind_text(hash)
                .bind(false)
                .bind_text(&stamped),
        )
        .await?;

        let select = format!(
            "select family, subject, used from {REFRESH_TOKENS_TABLE} where token_hash = {}",
            self.placeholder(1)
        );
        let rows = fetch(tx, COMPONENT, RawQuery::new(select).bind_text(hash)).await?;
        let Some(row) = rows.first() else {
            // Nothing to claim and nothing to read: the token is unknown, or a
            // family burn removed it between the two statements.
            return Ok(Claim::Invalid);
        };
        let family = text(COMPONENT, row, 0, "family")?;
        let subject = text(COMPONENT, row, 1, "subject")?;

        if claimed > 0 {
            let ttl = self.jwt.config().refresh_ttl;
            let next = RefreshToken::mint(subject.clone(), family.clone(), ttl)?;
            self.insert(tx, &next, now).await?;
            return Ok(Claim::Rotated { next });
        }

        if flag(COMPONENT, row, 2, "used")? {
            // Presented twice. Burn the family: the legitimate client is logged
            // out, which is strictly better than an attacker holding a token
            // that rotates forever.
            let revoked = self.delete_where(tx, "family", &family).await?;
            return Ok(Claim::Reused {
                family,
                subject,
                revoked,
            });
        }

        // Unused, unclaimed: the only remaining predicate is the expiry.
        self.delete_where(tx, "token_hash", hash).await?;
        Ok(Claim::Invalid)
    }

    /// `delete from … where <column> = <value>`, for the two columns that are
    /// ever deleted by.
    ///
    /// `column` is one of this module's own string literals and never reaches
    /// here from a request; the *value* is always bound.
    async fn delete_where(&self, tx: &Tx, column: &'static str, value: &str) -> Result<u64> {
        let sql = format!(
            "delete from {REFRESH_TOKENS_TABLE} where {column} = {}",
            self.placeholder(1)
        );
        run(tx, COMPONENT, RawQuery::new(sql).bind_text(value)).await
    }

    /// Revoke every token matching one indexed column.
    async fn revoke_by(&self, column: &'static str, value: &str) -> Result<u64> {
        let sql = format!(
            "delete from {REFRESH_TOKENS_TABLE} where {column} = {}",
            self.placeholder(1)
        );
        run(&self.db, COMPONENT, RawQuery::new(sql).bind_text(value)).await
    }
}

/// What the compare-and-set decided, before the transaction commits.
///
/// Separate from [`RefreshOutcome`] because the access token is minted *after*
/// the commit: signing before the commit would hand out a token for a rotation
/// that then failed to land.
#[derive(Debug)]
enum Claim {
    /// The token was claimed and its successor is written.
    Rotated {
        /// The successor, to hand to the client.
        next: RefreshToken,
    },
    /// The token had already been claimed. The family is gone.
    Reused {
        /// Which family was burned.
        family: String,
        /// Whose it was, for the audit line.
        subject: String,
        /// How many tokens went with it.
        revoked: u64,
    },
    /// Unknown or expired. No family to revoke.
    Invalid,
}

impl RefreshStore for TableRefreshStore {
    fn issue<'a>(&'a self, subject: &'a str, ttl: Duration) -> BoxFuture<'a, Result<RefreshToken>> {
        Box::pin(async move {
            let family = RefreshToken::new_family()?;
            let token = RefreshToken::mint(subject, family, ttl)?;
            let tx = self.db.begin().await.map_err(unavailable_now)?;
            match self.insert(&tx, &token, Utc::now()).await {
                Ok(()) => {
                    tx.commit().await.map_err(unavailable_now)?;
                    Ok(token)
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    Err(error)
                }
            }
        })
    }

    fn exchange<'a>(&'a self, token: &'a str) -> BoxFuture<'a, Result<RefreshOutcome>> {
        Box::pin(async move {
            let hash = RefreshToken::hash_of(token);
            let now = Utc::now();

            let tx = self.db.begin().await.map_err(unavailable_now)?;
            let claim = match self.claim(&tx, &hash, now).await {
                Ok(claim) => {
                    tx.commit().await.map_err(unavailable_now)?;
                    claim
                }
                Err(error) => {
                    let _ = tx.rollback().await;
                    return Err(error);
                }
            };

            match claim {
                Claim::Rotated { next } => {
                    // Signed after the commit, so a rotation that failed to land
                    // never produced an access token.
                    let access = self
                        .jwt
                        .issue(&Claims::new(&next.subject), self.jwt.config().access_ttl)?;
                    Ok(RefreshOutcome::Rotated {
                        access,
                        refresh: next,
                    })
                }
                Claim::Reused {
                    family,
                    subject,
                    revoked,
                } => {
                    tracing::warn!(
                        target: "moso_auth::audit",
                        event = "refresh_token_reuse",
                        family = %family,
                        subject = %subject,
                        revoked,
                        "a refresh token was presented twice; the whole family has been revoked"
                    );
                    Ok(RefreshOutcome::ReuseDetected { family })
                }
                Claim::Invalid => Ok(RefreshOutcome::Invalid),
            }
        })
    }

    fn revoke_family<'a>(&'a self, family: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.revoke_by("family", family).await })
    }

    fn revoke_subject<'a>(&'a self, subject: &'a str) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move { self.revoke_by("subject", subject).await })
    }
}

/// A transaction that could not be opened, committed or rolled back.
fn unavailable_now(error: moso_orm::Error) -> Error {
    unavailable(COMPONENT, error)
}

#[cfg(test)]
mod tests {
    use moso_orm::Row;

    use super::*;
    use crate::store::conformance::{jwt_issuer, on_every_backend, unique};

    /// Read one row back.
    ///
    /// Deliberately not on the trait: nothing in the request path reads a
    /// refresh token by hash without also claiming it, and a method that let it
    /// would be an invitation to write the read-then-write this module exists to
    /// avoid.
    fn decode_row(row: &Row) -> Result<(String, String, bool)> {
        Ok((
            text(COMPONENT, row, 1, "family")?,
            text(COMPONENT, row, 2, "subject")?,
            flag(COMPONENT, row, 5, "used")?,
        ))
    }

    /// Read every row matching one indexed column, for assertions the trait
    /// cannot make.
    ///
    /// Scoped to a column rather than counting the table, because the PostgreSQL
    /// leg shares one server: `len()` there is every concurrent test's rows too,
    /// and an assertion on it would be a test that fails depending on what else
    /// happened to be running.
    async fn rows_where(
        store: &TableRefreshStore,
        column: &str,
        value: &str,
    ) -> Vec<(String, String, bool)> {
        let sql = format!(
            "select {COLUMNS} from {REFRESH_TOKENS_TABLE} where {column} = {}",
            store.placeholder(1)
        );
        fetch(&store.db, COMPONENT, RawQuery::new(sql).bind_text(value))
            .await
            .expect("the rows read back")
            .iter()
            .map(|row| decode_row(row).expect("the row decodes"))
            .collect()
    }

    /// Every row of one family.
    async fn family_rows(store: &TableRefreshStore, family: &str) -> Vec<(String, String, bool)> {
        rows_where(store, "family", family).await
    }

    /// Every row belonging to one subject.
    async fn subject_rows(store: &TableRefreshStore, subject: &str) -> Vec<(String, String, bool)> {
        rows_where(store, "subject", subject).await
    }

    /// A store over `db`, with its table created.
    async fn store(db: Db) -> TableRefreshStore {
        let store = TableRefreshStore::new(db, Arc::new(jwt_issuer()));
        store.create_table().await.expect("the table is created");
        store
    }

    // ── the compare-and-set, under contention ─────────────────────────────

    /// The property the whole module exists for.
    ///
    /// Two exchanges of one token, started together: exactly one rotates,
    /// exactly one is reported as reuse, and the family is gone afterwards —
    /// including the successor the winner minted, which is what the transaction
    /// around the exchange buys.
    ///
    /// On PostgreSQL the two run on two connections and the loser's `update`
    /// really does block on the winner's row lock, which is the cross-process
    /// case. On an in-memory SQLite the pool is pinned to one connection
    /// (`Db::connect` does that, because every connection to `:memory:` is a
    /// different database), so the two serialise at the pool instead; that still
    /// exercises the store's decision, and PostgreSQL is where the locking is
    /// proved.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_concurrent_exchanges_produce_one_rotation_and_one_detected_reuse() {
        on_every_backend(|db| async move {
            let store = Arc::new(store(db).await);
            let subject = unique("usr");
            let first = store
                .issue(&subject, Duration::from_secs(3600))
                .await
                .expect("issued");
            let family = first.family().to_owned();
            let presented = first.expose().to_owned();

            let left = {
                let store = Arc::clone(&store);
                let presented = presented.clone();
                tokio::spawn(async move { store.exchange(&presented).await })
            };
            let right = {
                let store = Arc::clone(&store);
                tokio::spawn(async move { store.exchange(&presented).await })
            };

            let outcomes = [
                left.await.expect("the task did not panic").expect("left"),
                right.await.expect("the task did not panic").expect("right"),
            ];

            let rotations = outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RefreshOutcome::Rotated { .. }))
                .count();
            let reuses = outcomes
                .iter()
                .filter(|outcome| matches!(outcome, RefreshOutcome::ReuseDetected { .. }))
                .count();
            assert_eq!(rotations, 1, "exactly one caller may win: {outcomes:?}");
            assert_eq!(reuses, 1, "and the other is reuse: {outcomes:?}");

            assert!(
                family_rows(&store, &family).await.is_empty(),
                "the family must be burned, successor included"
            );

            store.revoke_subject(&subject).await.expect("cleaned up");
        })
        .await;
    }

    // ── the same rules, one caller at a time ──────────────────────────────

    #[tokio::test]
    async fn a_replayed_token_burns_every_descendant() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let subject = unique("usr");
            let first = store
                .issue(&subject, Duration::from_secs(3600))
                .await
                .expect("issued");
            let family = first.family().to_owned();

            let RefreshOutcome::Rotated {
                refresh: second, ..
            } = store.exchange(first.expose()).await.expect("rotated")
            else {
                panic!("the first exchange must rotate");
            };
            assert_eq!(family_rows(&store, &family).await.len(), 2);

            let replayed = store.exchange(first.expose()).await.expect("exchanged");
            assert!(
                matches!(replayed, RefreshOutcome::ReuseDetected { family: ref burned }
                    if *burned == family),
                "{replayed:?}"
            );

            assert!(family_rows(&store, &family).await.is_empty());
            assert!(
                matches!(
                    store.exchange(second.expose()).await.expect("exchanged"),
                    RefreshOutcome::Invalid
                ),
                "the descendant died with the family"
            );

            store.revoke_subject(&subject).await.expect("cleaned up");
        })
        .await;
    }

    #[tokio::test]
    async fn an_expired_token_is_invalid_and_is_not_reported_as_reuse() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let subject = unique("usr");
            let token = store
                .issue(&subject, Duration::from_millis(1))
                .await
                .expect("issued");
            tokio::time::sleep(Duration::from_millis(20)).await;

            assert!(
                matches!(
                    store.exchange(token.expose()).await.expect("exchanged"),
                    RefreshOutcome::Invalid
                ),
                "an expired token has no family to burn"
            );
            assert!(
                subject_rows(&store, &subject).await.is_empty(),
                "and the row is reclaimed on the way past"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn the_sweep_reclaims_expired_rows_without_affecting_live_ones() {
        on_every_backend(|db| async move {
            let store = store(db).await;
            let live = unique("usr");
            let dead = unique("usr");
            store
                .issue(&live, Duration::from_secs(3600))
                .await
                .expect("issued");
            store
                .issue(&dead, Duration::from_millis(1))
                .await
                .expect("issued");
            tokio::time::sleep(Duration::from_millis(20)).await;

            assert!(store.sweep().await.expect("swept") >= 1);
            assert!(subject_rows(&store, &dead).await.is_empty());
            assert_eq!(
                subject_rows(&store, &live).await.len(),
                1,
                "a live token must survive the sweep"
            );
            assert!(store.len().await.expect("counted") >= 1);

            store.revoke_subject(&live).await.expect("cleaned up");
        })
        .await;
    }

    #[tokio::test]
    async fn the_debug_impl_names_the_table_and_no_key() {
        let store = TableRefreshStore::new(
            Db::connect_url("sqlite://:memory:")
                .await
                .expect("the bundled SQLite is always available"),
            Arc::new(jwt_issuer()),
        );
        let rendered = format!("{store:?}");
        assert!(rendered.contains(REFRESH_TOKENS_TABLE), "{rendered}");
        assert!(!rendered.contains("key"), "{rendered}");
    }
}
