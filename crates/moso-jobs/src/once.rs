//! The idempotency ledger behind [`JobCtx::once`](crate::JobCtx::once).
//!
//! Delivery is at-least-once, so a job that must not repeat a side effect needs
//! somewhere durable to record that it already did it. This module is that
//! somewhere: a table when the application has a database, and the key-value
//! store's compare-and-set when it does not.
//!
//! The whole surface is private. An idempotency ledger with a public API is one
//! an application will reach into, and the moment it does, the schema is frozen.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_orm::Executor as _;

use crate::{Error, Result};

/// The table the database-backed ledger uses.
///
/// Not configurable. A per-application prefix would let two deployments of the
/// same code disagree about where "already done" is recorded, which is the one
/// mistake this table exists to make impossible.
pub(crate) const TABLE: &str = "moso_job_once";

/// What claiming a key produced.
#[derive(Debug)]
pub(crate) enum Claim {
    /// Nobody had done this work; the caller owns it and must record an
    /// outcome.
    Mine,
    /// Somebody already did it, and this is what they got.
    Recorded(serde_json::Value),
}

/// Where "already done" is written down.
#[derive(Debug)]
pub(crate) enum Store {
    /// A table. The default when the application has a `Db`.
    Database(std::sync::Arc<moso_orm::Db>),
    /// The key-value store's compare-and-set, for a database-less deployment.
    Kv(std::sync::Arc<moso_kv::Kv>),
}

impl Store {
    /// Pick the ledger this application can support.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) naming both providers when
    /// neither is registered — `once` cannot be faked with an in-process map,
    /// because the whole point is that it survives the process.
    pub(crate) fn from_resolver(resolver: &moso_core::Resolver) -> Result<Self> {
        if let Ok(db) = resolver.get::<moso_orm::Db>() {
            return Ok(Self::Database(db));
        }
        if let Ok(kv) = resolver.get::<moso_kv::Kv>() {
            return Ok(Self::Kv(kv));
        }
        Err(Error::config(
            "`JobCtx::once` needs somewhere durable to record that the work was done, and \
             neither a `Db` nor a `Kv` is registered\n\
             help: add `.provide(db)` (or `.provide(kv)`) in the composition root\n\
             note: an in-process map is not an option here — an exactly-once helper that \
             forgets when the process restarts is worse than no helper at all",
        ))
    }

    /// Take the key, or hand back what the previous holder recorded.
    ///
    /// A claim with no recorded outcome older than `orphan_after` is taken
    /// over: the process that made it was killed mid-body, and blocking the
    /// work forever is not a better answer than running it again.
    pub(crate) async fn claim(&self, key: &str, orphan_after: Duration) -> Result<Claim> {
        match self {
            Self::Database(db) => self.claim_in_database(db, key, orphan_after).await,
            Self::Kv(kv) => self.claim_in_kv(kv, key, orphan_after).await,
        }
    }

    /// Record the outcome against a key this process claimed.
    pub(crate) async fn record(&self, key: &str, outcome: serde_json::Value) -> Result<()> {
        match self {
            Self::Database(db) => {
                moso_orm::RawQuery::new(format!(
                    "update {TABLE} set outcome = $1, completed_at = $2 where key = $3"
                ))
                .bind_text(&outcome.to_string())
                .bind(Utc::now())
                .bind_text(key)
                .execute(db.as_ref())
                .await?;
                Ok(())
            }
            Self::Kv(kv) => {
                let entry = Entry {
                    claimed_at: Utc::now(),
                    outcome: Some(outcome),
                };
                kv.store()
                    .set(
                        &once_key(kv, key)?,
                        serde_json::to_vec(&entry)?.into(),
                        moso_kv::SetOpts::new(),
                    )
                    .await?;
                Ok(())
            }
        }
    }

    /// The database ledger: one row per key, claimed with `ON CONFLICT`.
    async fn claim_in_database(
        &self,
        db: &moso_orm::Db,
        key: &str,
        orphan_after: Duration,
    ) -> Result<Claim> {
        ensure_table(db).await?;
        let now = Utc::now();
        let stale_before = now - chrono::Duration::from_std(orphan_after).unwrap_or_default();

        // One statement does the whole thing: insert if absent, take over an
        // abandoned claim, and otherwise leave the row alone. The `returning`
        // then tells us which of the three happened, because `claimed_at` is
        // `now` exactly when this call won.
        let claimed = moso_orm::RawQuery::new(format!(
            "insert into {TABLE} (key, claimed_at, completed_at, outcome) \
             values ($1, $2, null, null) \
             on conflict (key) do update set claimed_at = excluded.claimed_at \
             where {TABLE}.completed_at is null and {TABLE}.claimed_at < $3 \
             returning claimed_at"
        ))
        .bind_text(key)
        .bind(now)
        .bind(stale_before)
        .into_sql();

        let won = db.handle().fetch_optional_sql(claimed).await?.is_some();
        if won {
            return Ok(Claim::Mine);
        }

        // The insert did nothing, so a live claim exists. Read what it holds:
        // an outcome means the work is done, no outcome means somebody else is
        // still doing it and this attempt must not race them.
        let existing =
            moso_orm::RawQuery::new(format!("select outcome from {TABLE} where key = $1"))
                .bind_text(key)
                .into_sql();
        let row = db.handle().fetch_optional_sql(existing).await?;
        match row {
            Some(row) => match row.get_opt::<String>(0)? {
                Some(text) => Ok(Claim::Recorded(serde_json::from_str(&text)?)),
                None => Err(Error::retry(format!(
                    "another worker is already running the `{key}` side effect; this attempt \
                     will be retried"
                ))),
            },
            // The row vanished between the two statements — a sweeper, or a
            // concurrent take-over. Retrying is correct and cheap.
            None => Err(Error::retry(format!(
                "the idempotency claim for `{key}` disappeared mid-check"
            ))),
        }
    }

    /// The key-value ledger: `set_if_absent`, then read.
    async fn claim_in_kv(
        &self,
        kv: &moso_kv::Kv,
        key: &str,
        orphan_after: Duration,
    ) -> Result<Claim> {
        let full = once_key(kv, key)?;
        let now = Utc::now();
        let fresh = Entry {
            claimed_at: now,
            outcome: None,
        };
        let encoded: bytes::Bytes = serde_json::to_vec(&fresh)?.into();

        let won = kv
            .store()
            .set(&full, encoded.clone(), moso_kv::SetOpts::new().if_absent())
            .await?;
        if won {
            return Ok(Claim::Mine);
        }

        let Some(raw) = kv.store().get(&full).await? else {
            // Expired or evicted between the set and the get.
            return Err(Error::retry(format!(
                "the idempotency claim for `{key}` disappeared mid-check"
            )));
        };
        let entry: Entry = serde_json::from_slice(&raw)?;
        if let Some(outcome) = entry.outcome {
            return Ok(Claim::Recorded(outcome));
        }
        let age = now - entry.claimed_at;
        if age.to_std().unwrap_or_default() >= orphan_after {
            // Take over the abandoned claim. `compare_and_swap` and not a plain
            // `set`, so two workers reaching this line together do not both
            // think they won.
            let took_over = kv
                .store()
                .compare_and_swap(&full, Some(&raw), encoded, moso_kv::SetOpts::new())
                .await?;
            if took_over {
                return Ok(Claim::Mine);
            }
        }
        Err(Error::retry(format!(
            "another worker is already running the `{key}` side effect; this attempt will be \
             retried"
        )))
    }
}

/// One ledger entry, as the key-value store holds it.
#[derive(serde::Serialize, serde::Deserialize)]
struct Entry {
    /// When the claim was taken.
    claimed_at: DateTime<Utc>,
    /// What the body returned, once it did.
    outcome: Option<serde_json::Value>,
}

// The namespace `once` writes under. A `//` comment and not a doc comment:
// rustdoc does not document a macro invocation, and `#[warn(unused_doc_comments)]`
// says so.
moso_kv::namespace! {
    /// One `JobCtx::once` claim. `on_failure = fail`, because a store that is
    /// down must stop the job rather than let the side effect happen twice.
    pub(crate) JobOnce: String => String, ttl = moso_kv::days(30), on_failure = fail;
}

/// Build the store key for `key`.
fn once_key(kv: &moso_kv::Kv, key: &str) -> Result<moso_kv::Key> {
    Ok(kv.key::<JobOnce>(&key.to_owned())?)
}

/// Create the ledger table once per process.
///
/// Not a migration: this table has no application-visible shape, no foreign
/// keys and no data an operator ever reads, so making an application generate a
/// migration for it would be ceremony for its own sake. It is created on first
/// use and never altered.
async fn ensure_table(db: &moso_orm::Db) -> Result<()> {
    static CREATED: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();
    CREATED
        .get_or_try_init(|| async {
            moso_orm::RawQuery::new(format!(
                "create table if not exists {TABLE} (\
                 key text primary key, \
                 claimed_at timestamptz not null, \
                 completed_at timestamptz, \
                 outcome text)"
            ))
            .execute(db)
            .await
            .map(|_| ())
            .map_err(Error::from)
        })
        .await
        .copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The namespace has to fail rather than degrade: a cache miss here means
    /// "run the side effect again", which is exactly what `once` prevents.
    #[test]
    fn the_ledger_namespace_fails_rather_than_degrading() {
        use moso_kv::Namespace as _;
        assert_eq!(JobOnce::FAILURE_MODE, moso_kv::FailureMode::Fail);
    }

    /// An entry round-trips, including the "claimed but not finished" shape
    /// that the orphan take-over reads.
    #[test]
    fn an_entry_round_trips_in_both_states() {
        let claimed = Entry {
            claimed_at: Utc::now(),
            outcome: None,
        };
        let encoded = serde_json::to_vec(&claimed).expect("serialises");
        let back: Entry = serde_json::from_slice(&encoded).expect("deserialises");
        assert!(back.outcome.is_none());

        let done = Entry {
            claimed_at: Utc::now(),
            outcome: Some(serde_json::json!({ "charged": true })),
        };
        let encoded = serde_json::to_vec(&done).expect("serialises");
        let back: Entry = serde_json::from_slice(&encoded).expect("deserialises");
        assert_eq!(back.outcome.unwrap()["charged"], serde_json::json!(true));
    }

    /// Without a `Db` or a `Kv` there is nowhere durable to write, and the
    /// message has to say what to add rather than "not supported".
    #[test]
    fn no_durable_store_is_an_error_that_names_the_fix() {
        let resolver =
            moso_core::Resolver::new(std::sync::Arc::new(moso_core::ProviderMap::default()));
        let error = Store::from_resolver(&resolver).expect_err("nothing is registered");
        let rendered = error.to_string();
        assert!(rendered.contains("`JobCtx::once`"), "{rendered}");
        assert!(rendered.contains(".provide("), "{rendered}");
    }

    /// The whole ledger, against the in-memory store: a first call wins, a
    /// second sees the recorded outcome, and the body runs exactly once.
    #[tokio::test]
    async fn the_kv_ledger_runs_a_body_once() {
        let kv = std::sync::Arc::new(moso_kv::Kv::in_memory("jobs-test").expect("in-memory kv"));
        let store = Store::Kv(std::sync::Arc::clone(&kv));

        let claim = store
            .claim("charge:invoice_42", Duration::from_secs(3600))
            .await
            .expect("the first claim wins");
        assert!(matches!(claim, Claim::Mine));

        store
            .record("charge:invoice_42", serde_json::json!("receipt-1"))
            .await
            .expect("records the outcome");

        let second = store
            .claim("charge:invoice_42", Duration::from_secs(3600))
            .await
            .expect("the second claim reads");
        match second {
            Claim::Recorded(value) => assert_eq!(value, serde_json::json!("receipt-1")),
            Claim::Mine => panic!("the body would have run twice"),
        }
    }

    /// A claim with no outcome must not let a second worker in — that is the
    /// double-charge this whole module exists to prevent.
    #[tokio::test]
    async fn a_live_claim_blocks_a_second_worker() {
        let kv = std::sync::Arc::new(moso_kv::Kv::in_memory("jobs-test-2").expect("in-memory kv"));
        let store = Store::Kv(kv);

        assert!(matches!(
            store
                .claim("charge:invoice_7", Duration::from_secs(3600))
                .await
                .expect("first"),
            Claim::Mine
        ));

        let blocked = store
            .claim("charge:invoice_7", Duration::from_secs(3600))
            .await
            .expect_err("a live claim blocks");
        assert!(blocked.retryable(), "the second worker must come back");
    }

    /// A process killed mid-body leaves a claim nobody will ever finish. After
    /// the orphan window the work has to become available again.
    #[tokio::test]
    async fn an_abandoned_claim_is_taken_over_after_its_window() {
        let kv = std::sync::Arc::new(moso_kv::Kv::in_memory("jobs-test-3").expect("in-memory kv"));
        let store = Store::Kv(kv);

        assert!(matches!(
            store
                .claim("charge:invoice_9", Duration::from_secs(3600))
                .await
                .expect("first"),
            Claim::Mine
        ));

        // A zero window is "everything is already abandoned", which is exactly
        // the state the take-over path has to handle.
        let taken = store
            .claim("charge:invoice_9", Duration::ZERO)
            .await
            .expect("the orphan is taken over");
        assert!(matches!(taken, Claim::Mine));
    }
}
