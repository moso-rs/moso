//! The Redis queue, written against [`KvStore`](moso_kv::KvStore).
//!
//! # Why not a Redis client
//!
//! This crate already depends on `moso-kv`, which already owns a Redis
//! connection pool, a circuit breaker, a key scheme and a reconnect policy.
//! Taking a second dependency on `fred` would mean two pools, two breakers and
//! two opinions about what a key looks like — and it would put a Redis type in
//! reach of this crate's public API, which
//! [`Queue`](crate::Queue) exists to prevent.
//!
//! The consequence worth stating: everything here is expressed in the eleven
//! `KvStore` operations, so the **same code** runs against the in-memory store.
//! That is not a test double of the Redis queue; it *is* the Redis queue, with a
//! different store underneath.
//!
//! # What that costs
//!
//! `KvStore` has no `ZCARD` and no multi-key transaction, so
//! [`Queue::stats`](crate::Queue::stats) counts members rather than asking for a
//! cardinality, and the pull path claims a job with `ZREM` — which returns 1 to
//! exactly one caller — rather than with a Lua script. Both are correct; the
//! first is O(depth) and is why `stats` is called on a tick and not per pull.
//!
//! # Durability
//!
//! Whatever the Redis persistence configuration gives, and the boot log says so
//! when append-only writing is off. Transactional enqueue goes through
//! [`Outbox`](super::Outbox), which is a table plus a relay, and the
//! documentation is explicit about that rather than implying the two are
//! identical.

use std::time::Duration;

use bytes::Bytes;
use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_kv::{Key, KvStore, SetOpts};
use serde::{Deserialize, Serialize};

use crate::{
    DeadLetter, DeadLetterQueue, DlqFilter, DlqStats, JobId, JobState, Lease, Queue,
    QueueCapabilities, QueueStats, QueuedJob, Result, WorkerId,
};

/// How much one priority step is worth, in the sorted-set score.
///
/// A day. `Priority::High` therefore beats `Priority::Normal` unless the normal
/// job has been ready for more than a day, at which point the starving job wins
/// — which is the behaviour you want out of a priority queue and not the one a
/// strict lexicographic score gives.
const PRIORITY_WEIGHT: f64 = 86_400_000.0;

/// Redis, for throughput.
///
/// Faster than the Postgres backend and honest about the trade: durability is
/// whatever the Redis persistence configuration gives, and the boot log says so
/// when append-only file writing is off. Transactional enqueue goes through
/// [`Outbox`](super::Outbox).
///
/// ```no_run
/// use moso_jobs::backend::RedisQueue;
///
/// let _ = RedisQueue::new("redis://localhost:6379");
/// ```
pub struct RedisQueue {
    /// Where Redis is.
    url: String,
    /// The key prefix, so one Redis can host several applications.
    prefix: String,
    /// The wire names that must never have two instances leased at once.
    serial: std::sync::RwLock<std::collections::BTreeSet<String>>,
    /// The store, opened on first use.
    ///
    /// `RedisQueue::new` is infallible and synchronous — it is called in a
    /// composition root, next to twenty other builders — and opening a
    /// connection is neither.
    store: tokio::sync::OnceCell<std::sync::Arc<dyn KvStore>>,
}

impl RedisQueue {
    /// A queue on the Redis at `url`.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::RedisQueue;
    /// let _ = RedisQueue::new("redis://localhost:6379");
    /// ```
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            prefix: "jobs".to_owned(),
            serial: std::sync::RwLock::new(std::collections::BTreeSet::new()),
            store: tokio::sync::OnceCell::new(),
        }
    }

    /// Set the key prefix.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::RedisQueue;
    /// let _ = RedisQueue::new("redis://localhost").prefix("shop-jobs");
    /// ```
    #[must_use]
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Use a store that is already open.
    ///
    /// What makes the in-memory store a first-class way to run this backend,
    /// and what `moso dev` uses when it wants Redis semantics without Redis.
    ///
    /// ```
    /// use moso_jobs::backend::RedisQueue;
    /// use moso_kv::backend::MemoryStore;
    ///
    /// let queue = RedisQueue::with_store(std::sync::Arc::new(MemoryStore::new()));
    /// assert_eq!(moso_jobs::Queue::name(&queue), "redis");
    /// ```
    #[must_use]
    pub fn with_store(store: std::sync::Arc<dyn KvStore>) -> Self {
        let cell = tokio::sync::OnceCell::new();
        // The cell was just created, so this cannot already be set.
        let _ = cell.set(store);
        Self {
            url: String::new(),
            prefix: "jobs".to_owned(),
            serial: std::sync::RwLock::new(std::collections::BTreeSet::new()),
            store: cell,
        }
    }

    /// The store, opening it on first use.
    async fn store(&self) -> Result<&std::sync::Arc<dyn KvStore>> {
        self.store
            .get_or_try_init(|| async {
                let kv = moso_kv::KvConfig::new(self.prefix.clone(), moso_kv::KvBackend::Redis)
                    .url(self.url.clone())
                    .build()
                    .await?;
                if !kv.capabilities().structures {
                    return Err(crate::Error::config(
                        "the configured key-value backend has no sorted sets, and the Redis \
                         job queue is built out of them\n\
                         help: use `JobsBackendKind::Postgres`, or point `JOBS_URL` at a real \
                         Redis",
                    ));
                }
                Ok(std::sync::Arc::clone(kv.store()))
            })
            .await
    }

    /// `moso:v1:{prefix}:jobs:{parts}`.
    fn key(&self, parts: &[&str]) -> Result<Key> {
        let mut buf = moso_kv::KeyBuf::new(&self.prefix, "jobs", 1)
            .map_err(|error| crate::Error::config(error.to_string()))?;
        for part in parts {
            buf.segment_str(part);
        }
        buf.finish()
            .map_err(|error| crate::Error::config(error.to_string()))
    }

    /// The blob holding one job.
    fn job_key(&self, id: JobId) -> Result<Key> {
        self.key(&["job", &id.to_string()])
    }

    /// The ready set for one queue.
    fn ready_key(&self, queue: &str) -> Result<Key> {
        self.key(&["ready", queue])
    }

    /// The leased set for one queue.
    fn leased_key(&self, queue: &str) -> Result<Key> {
        self.key(&["leased", queue])
    }

    /// The dead-letter set.
    fn dead_key(&self) -> Result<Key> {
        self.key(&["dead"])
    }

    /// The marker that deduplicates a unique key.
    fn unique_key(&self, key: &str) -> Result<Key> {
        self.key(&["uniq", key])
    }

    /// The marker that holds a serial job's one running slot.
    ///
    /// The same primitive `push` already uses for deduplication — a `set` with
    /// `if_absent`, which exactly one caller wins — with the lease as its TTL,
    /// so a worker that dies frees the slot on the same schedule its job lease
    /// expires on.
    fn serial_key(&self, name: &str) -> Result<Key> {
        self.key(&["serial", name])
    }

    /// The key holding every schedule's last recorded run.
    ///
    /// One key rather than one per schedule: `KvStore` has no scan on every
    /// backend, and a dashboard field is not worth making one a requirement.
    /// Two leaders writing different schedules in the same instant can lose one
    /// update — last writer wins — which costs a stale timestamp on a dashboard
    /// and nothing else.
    fn schedules_key(&self) -> Result<Key> {
        self.key(&["schedules"])
    }

    /// Whether `name` is a job that must not run twice at once.
    fn is_serial(&self, name: &str) -> bool {
        self.serial
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(name)
    }

    /// Take the one running slot for `name`, for the length of `lease`.
    async fn claim_serial(&self, name: &str, id: JobId, lease: Duration) -> Result<bool> {
        let store = self.store().await?;
        store
            .set(
                &self.serial_key(name)?,
                Bytes::from(id.to_string().into_bytes()),
                SetOpts::new()
                    .if_absent()
                    .ttl(lease.max(Duration::from_secs(1))),
            )
            .await
            .map_err(crate::Error::from)
    }

    /// Give the slot back, but only if this job is the one holding it.
    ///
    /// A compare-and-delete rather than a delete: a worker whose lease was
    /// reclaimed must not free the slot the worker that took over is holding.
    async fn release_serial(&self, held: &Held) -> Result {
        if !self.is_serial(&held.job.name) {
            return Ok(());
        }
        let store = self.store().await?;
        let holder = held.job.id.to_string();
        store
            .compare_and_delete(&self.serial_key(&held.job.name)?, holder.as_bytes())
            .await?;
        Ok(())
    }

    /// Read one job blob.
    async fn load(&self, id: JobId) -> Result<Option<Held>> {
        let store = self.store().await?;
        let Some(raw) = store.get(&self.job_key(id)?).await? else {
            return Ok(None);
        };
        Ok(Some(serde_json::from_slice(&raw)?))
    }

    /// Write one job blob.
    async fn save(&self, held: &Held) -> Result<()> {
        let store = self.store().await?;
        store
            .set(
                &self.job_key(held.job.id)?,
                serde_json::to_vec(held)?.into(),
                SetOpts::new(),
            )
            .await?;
        Ok(())
    }

    /// The sorted-set score for a ready job: earlier and more urgent is lower.
    fn ready_score(job: &QueuedJob) -> f64 {
        job.run_at.timestamp_millis() as f64 - f64::from(job.priority.as_i16()) * PRIORITY_WEIGHT
    }

    /// The member a sorted set holds: the job's identifier.
    fn member(id: JobId) -> Bytes {
        Bytes::from(id.to_string().into_bytes())
    }

    /// Parse a member back into an identifier.
    fn member_id(raw: &Bytes) -> Option<JobId> {
        core::str::from_utf8(raw).ok()?.parse().ok()
    }

    /// Every schedule's last recorded run, by schedule key.
    ///
    /// A blob that does not decode is treated as absent rather than as an
    /// error: this is a dashboard field, and a schedule whose last run is
    /// unknown is better than a `/schedules` page that returns 503 because one
    /// key was written by a different version.
    async fn load_schedule_runs(
        &self,
    ) -> Result<std::collections::BTreeMap<String, crate::ScheduleRun>> {
        let store = self.store().await?;
        let Some(raw) = store.get(&self.schedules_key()?).await? else {
            return Ok(std::collections::BTreeMap::new());
        };
        let runs: Vec<crate::ScheduleRun> = serde_json::from_slice(&raw).unwrap_or_default();
        Ok(runs
            .into_iter()
            .map(|run| (run.schedule.as_str().to_owned(), run))
            .collect())
    }

    /// Every identifier in a sorted set, oldest score first.
    async fn members(&self, key: &Key, limit: u32) -> Result<Vec<JobId>> {
        let store = self.store().await?;
        Ok(store
            .zrange_by_score(key, f64::NEG_INFINITY, f64::INFINITY, limit)
            .await?
            .iter()
            .filter_map(Self::member_id)
            .collect())
    }
}

/// One job as Redis holds it: the row, plus who holds its lease.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Held {
    /// The row.
    job: QueuedJob,
    /// The lease token, when one is out.
    token: Option<String>,
    /// How many attempts were made, for the dead-letter record.
    attempts: u32,
    /// When it gave up.
    failed_at: Option<DateTime<Utc>>,
}

impl core::fmt::Debug for RedisQueue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // The URL can carry a password.
        f.debug_struct("RedisQueue")
            .field("prefix", &self.prefix)
            .field("connected", &self.store.initialized())
            .finish_non_exhaustive()
    }
}

impl Queue for RedisQueue {
    fn name(&self) -> &'static str {
        "redis"
    }

    fn serial_jobs(&self, names: &[&str]) {
        let mut guard = self
            .serial
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
        guard.extend(names.iter().map(|name| (*name).to_owned()));
    }

    fn capabilities(&self) -> QueueCapabilities {
        QueueCapabilities {
            // No transaction to join. `Outbox` covers the gap, and the boot log
            // says which of the two an application is getting.
            transactional_enqueue: false,
            push_notify: false,
            unique_keys: true,
            serial_chains: true,
            // Subject to the Redis persistence configuration, which this crate
            // cannot see from here; the boot log reports what the server says.
            durable: true,
            cancel: true,
            min_delay: Duration::ZERO,
        }
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let store = self.store().await?;

            if let Some(key) = &job.unique_key {
                // The marker *is* the deduplication: `if_absent` returns false
                // for the second caller, and a false is a successful no-op
                // rather than an error, because that is what dedupe means.
                let won = store
                    .set(
                        &self.unique_key(key)?,
                        Bytes::from(job.id.to_string().into_bytes()),
                        SetOpts::new().if_absent().ttl(Duration::from_secs(86_400)),
                    )
                    .await?;
                if !won {
                    return Ok(());
                }
            }

            let score = Self::ready_score(&job);
            let queue = job.queue.clone();
            let id = job.id;
            self.save(&Held {
                job,
                token: None,
                attempts: 0,
                failed_at: None,
            })
            .await?;
            store
                .zadd(&self.ready_key(&queue)?, &[(score, Self::member(id))])
                .await?;
            Ok(())
        })
    }

    fn pull<'a>(
        &'a self,
        queues: &'a [String],
        limit: u32,
        lease: Duration,
        worker: WorkerId,
    ) -> BoxFuture<'a, Result<Vec<(QueuedJob, Lease)>>> {
        Box::pin(async move {
            let store = self.store().await?;
            let now = Utc::now();
            let until = now + chrono::Duration::from_std(lease).unwrap_or_default();
            let ceiling = now.timestamp_millis() as f64;
            let mut leased = Vec::new();

            for queue in queues {
                if leased.len() >= limit as usize {
                    break;
                }
                let ready = self.ready_key(queue)?;
                let remaining = u32::try_from(limit as usize - leased.len()).unwrap_or(limit);
                // Everything whose score is at or below "now, at any priority".
                let candidates = store
                    .zrange_by_score(&ready, f64::NEG_INFINITY, ceiling, remaining)
                    .await?;

                for member in candidates {
                    // `zrem` returning 1 is the claim: exactly one caller gets
                    // it, whatever else is racing. This is what replaces the
                    // Lua script a native client would use.
                    if store.zrem(&ready, core::slice::from_ref(&member)).await? != 1 {
                        continue;
                    }
                    let Some(id) = Self::member_id(&member) else {
                        continue;
                    };
                    let Some(mut held) = self.load(id).await? else {
                        // The blob expired under the index. Dropping the index
                        // entry is the repair.
                        continue;
                    };

                    // `Job::SERIAL`: one running instance of this job name in
                    // the whole fleet. The slot is a `set … if_absent` — the
                    // same primitive deduplication uses — so exactly one worker
                    // wins it. The loser puts the member back at the score it
                    // came in with rather than nacking, because a contended
                    // chain must not spend the row's retry budget.
                    if self.is_serial(&held.job.name)
                        && !self.claim_serial(&held.job.name, id, lease).await?
                    {
                        store
                            .zadd(&ready, &[(Self::ready_score(&held.job), member.clone())])
                            .await?;
                        continue;
                    }

                    let token = uuid::Uuid::new_v4().to_string();
                    held.job.state = JobState::Running;
                    held.job.locked_by = Some(worker.clone());
                    held.job.locked_until = Some(until);
                    held.attempts = held.job.attempt;
                    held.token = Some(token.clone());
                    self.save(&held).await?;
                    store
                        .zadd(
                            &self.leased_key(queue)?,
                            &[(until.timestamp_millis() as f64, member.clone())],
                        )
                        .await?;
                    leased.push((held.job, Lease::new(id, token, until)));
                }
            }
            Ok(leased)
        })
    }

    fn ack<'a>(&'a self, lease: Lease) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let store = self.store().await?;
            let Some(mut held) = self.load(lease.job_id()).await? else {
                return Ok(());
            };
            if held.token.as_deref() != Some(lease.token()) {
                return Err(reclaimed(lease.job_id()));
            }
            store
                .zrem(
                    &self.leased_key(&held.job.queue)?,
                    &[Self::member(lease.job_id())],
                )
                .await?;
            if let Some(key) = &held.job.unique_key {
                store.delete(&self.unique_key(key)?).await?;
            }
            self.release_serial(&held).await?;
            held.job.state = JobState::Done;
            held.token = None;
            held.job.locked_until = None;
            self.save(&held).await?;
            // Finished rows are not kept: the dashboard reads Postgres, and a
            // Redis instance that grows without bound is an outage waiting.
            store.delete(&self.job_key(lease.job_id())?).await?;
            Ok(())
        })
    }

    fn nack<'a>(
        &'a self,
        lease: Lease,
        error: &'a str,
        run_at: Option<DateTime<Utc>>,
    ) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let store = self.store().await?;
            let Some(mut held) = self.load(lease.job_id()).await? else {
                return Ok(());
            };
            if held.token.as_deref() != Some(lease.token()) {
                return Err(reclaimed(lease.job_id()));
            }

            let member = Self::member(lease.job_id());
            store
                .zrem(
                    &self.leased_key(&held.job.queue)?,
                    std::slice::from_ref(&member),
                )
                .await?;
            held.token = None;
            held.job.locked_until = None;
            held.job.last_error = Some(error.to_owned());
            // Whether it retries or dies, this attempt is over and the serial
            // chain moves on.
            self.release_serial(&held).await?;

            match run_at {
                Some(at) => {
                    held.job.state = JobState::Retrying;
                    held.job.attempt += 1;
                    held.job.run_at = at;
                    let score = Self::ready_score(&held.job);
                    let queue = held.job.queue.clone();
                    self.save(&held).await?;
                    store
                        .zadd(&self.ready_key(&queue)?, &[(score, member)])
                        .await?;
                }
                None => {
                    held.job.state = JobState::Dead;
                    held.failed_at = Some(Utc::now());
                    if let Some(key) = &held.job.unique_key {
                        store.delete(&self.unique_key(key)?).await?;
                    }
                    let failed = held.failed_at.unwrap_or(Utc::now());
                    self.save(&held).await?;
                    store
                        .zadd(
                            &self.dead_key()?,
                            &[(failed.timestamp_millis() as f64, member)],
                        )
                        .await?;
                }
            }
            Ok(())
        })
    }

    fn heartbeat<'a>(&'a self, lease: &'a Lease, extend: Duration) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let store = self.store().await?;
            let Some(mut held) = self.load(lease.job_id()).await? else {
                return Err(reclaimed(lease.job_id()));
            };
            if held.token.as_deref() != Some(lease.token()) {
                return Err(reclaimed(lease.job_id()));
            }
            let until = Utc::now() + chrono::Duration::from_std(extend).unwrap_or_default();
            held.job.locked_until = Some(until);
            // The serial slot expires with the lease it was taken for, so a job
            // that outlives one lease has to renew both or a second instance
            // starts underneath it.
            if self.is_serial(&held.job.name) {
                store
                    .expire(
                        &self.serial_key(&held.job.name)?,
                        extend.max(Duration::from_secs(1)),
                    )
                    .await?;
            }
            let queue = held.job.queue.clone();
            self.save(&held).await?;
            store
                .zadd(
                    &self.leased_key(&queue)?,
                    &[(
                        until.timestamp_millis() as f64,
                        Self::member(lease.job_id()),
                    )],
                )
                .await?;
            Ok(())
        })
    }

    fn reclaim<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let store = self.store().await?;
            let now = Utc::now();
            let expired = now.timestamp_millis() as f64;
            let mut reclaimed = 0;

            for queue in queues {
                let leased = self.leased_key(queue)?;
                let members = store
                    .zrange_by_score(&leased, f64::NEG_INFINITY, expired, 1_000)
                    .await?;
                for member in members {
                    if store.zrem(&leased, core::slice::from_ref(&member)).await? != 1 {
                        continue;
                    }
                    let Some(id) = Self::member_id(&member) else {
                        continue;
                    };
                    let Some(mut held) = self.load(id).await? else {
                        continue;
                    };
                    held.job.state = JobState::Ready;
                    held.job.locked_by = None;
                    held.job.locked_until = None;
                    // Dropping the token is what makes the dead worker's `ack`
                    // fail rather than complete somebody else's run.
                    held.token = None;
                    held.job.run_at = now;
                    let score = Self::ready_score(&held.job);
                    // The dead worker's serial slot would otherwise sit there
                    // until its own TTL, blocking the chain it no longer runs.
                    self.release_serial(&held).await?;
                    self.save(&held).await?;
                    store
                        .zadd(&self.ready_key(queue)?, &[(score, member)])
                        .await?;
                    reclaimed += 1;
                }
            }
            Ok(reclaimed)
        })
    }

    fn stats<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<Vec<QueueStats>>> {
        Box::pin(async move {
            let store = self.store().await?;
            let now = Utc::now();
            let mut stats = Vec::with_capacity(queues.len());

            for queue in queues {
                let ready_key = self.ready_key(queue)?;
                let ready_members = store
                    .zrange_by_score(&ready_key, f64::NEG_INFINITY, f64::INFINITY, u32::MAX)
                    .await?;
                let leased = self.members(&self.leased_key(queue)?, u32::MAX).await?;

                let mut ready = 0;
                let mut retrying = 0;
                let mut oldest: Option<Duration> = None;
                for member in &ready_members {
                    let Some(id) = Self::member_id(member) else {
                        continue;
                    };
                    let Some(held) = self.load(id).await? else {
                        continue;
                    };
                    match held.job.state {
                        JobState::Retrying => retrying += 1,
                        _ => ready += 1,
                    }
                    if held.job.run_at <= now
                        && let Ok(waited) = (now - held.job.run_at).to_std()
                    {
                        oldest = Some(oldest.map_or(waited, |current| current.max(waited)));
                    }
                }

                let dead = self
                    .members(&self.dead_key()?, u32::MAX)
                    .await?
                    .len()
                    .try_into()
                    .unwrap_or(u64::MAX);

                stats.push(QueueStats {
                    queue: queue.clone(),
                    ready,
                    running: leased.len().try_into().unwrap_or(u64::MAX),
                    retrying,
                    dead,
                    oldest_ready: oldest,
                });
            }
            Ok(stats)
        })
    }

    fn cancel<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let store = self.store().await?;
            let Some(mut held) = self.load(id).await? else {
                return Ok(false);
            };
            if !held.job.state.is_active() {
                return Ok(false);
            }
            let member = Self::member(id);
            store
                .zrem(
                    &self.ready_key(&held.job.queue)?,
                    std::slice::from_ref(&member),
                )
                .await?;
            store
                .zrem(&self.leased_key(&held.job.queue)?, &[member])
                .await?;
            self.release_serial(&held).await?;
            held.job.state = JobState::Cancelled;
            // Dropping the token stops the worker that is running it from
            // acknowledging, which is what makes cancelling mean something.
            held.token = None;
            self.save(&held).await?;
            Ok(true)
        })
    }

    fn find<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move { Ok(self.load(id).await?.map(|held| held.job)) })
    }

    fn record_schedule_run<'a>(&'a self, run: &'a crate::ScheduleRun) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let store = self.store().await?;
            let key = self.schedules_key()?;
            let mut runs = self.load_schedule_runs().await?;
            runs.insert(run.schedule.as_str().to_owned(), run.clone());
            let encoded: Vec<crate::ScheduleRun> = runs.into_values().collect();
            store
                .set(&key, serde_json::to_vec(&encoded)?.into(), SetOpts::new())
                .await?;
            Ok(())
        })
    }

    fn schedule_runs(&self) -> BoxFuture<'_, Result<Vec<crate::ScheduleRun>>> {
        Box::pin(async move { Ok(self.load_schedule_runs().await?.into_values().collect()) })
    }

    fn probe(&self) -> BoxFuture<'_, Result> {
        Box::pin(async move {
            let store = self.store().await?;
            let status = store.health().await;
            if status.is_up() {
                Ok(())
            } else {
                Err(crate::Error::unavailable("redis", status.render()))
            }
        })
    }
}

impl DeadLetterQueue for RedisQueue {
    fn list<'a>(
        &'a self,
        filter: &'a DlqFilter,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<DeadLetter>, Option<String>)>> {
        Box::pin(async move {
            let mut letters = Vec::new();
            for id in self.members(&self.dead_key()?, u32::MAX).await? {
                if let Some(held) = self.load(id).await? {
                    letters.push(dead_letter(&held));
                }
            }
            letters.sort_by_key(|letter| std::cmp::Reverse(letter.failed_at));
            letters.retain(|letter| filter.matches(letter));

            let start = cursor.and_then(|c| c.parse::<usize>().ok()).unwrap_or(0);
            let end = start.saturating_add(limit as usize).min(letters.len());
            let page = letters.get(start..end).unwrap_or_default().to_vec();
            let next = (end < letters.len()).then(|| end.to_string());
            Ok((page, next))
        })
    }

    fn get<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<DeadLetter>>> {
        Box::pin(async move {
            Ok(self
                .load(id)
                .await?
                .filter(|held| held.job.state == JobState::Dead)
                .as_ref()
                .map(dead_letter))
        })
    }

    fn retry<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let store = self.store().await?;
            let (letters, _) = self.list(filter, None, limit).await?;
            let mut retried = 0;
            for letter in letters {
                let member = Self::member(letter.id);
                if store
                    .zrem(&self.dead_key()?, std::slice::from_ref(&member))
                    .await?
                    != 1
                {
                    continue;
                }
                let Some(mut held) = self.load(letter.id).await? else {
                    continue;
                };
                held.job.state = JobState::Ready;
                held.job.attempt = 1;
                held.job.run_at = Utc::now();
                held.job.last_error = None;
                held.failed_at = None;
                held.token = None;
                let score = Self::ready_score(&held.job);
                let queue = held.job.queue.clone();
                self.save(&held).await?;
                store
                    .zadd(&self.ready_key(&queue)?, &[(score, member)])
                    .await?;
                retried += 1;
            }
            Ok(retried)
        })
    }

    fn discard<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let store = self.store().await?;
            let (letters, _) = self.list(filter, None, limit).await?;
            let mut discarded = 0;
            for letter in letters {
                if store
                    .zrem(&self.dead_key()?, &[Self::member(letter.id)])
                    .await?
                    == 1
                {
                    store.delete(&self.job_key(letter.id)?).await?;
                    discarded += 1;
                }
            }
            Ok(discarded)
        })
    }

    fn stats(&self) -> BoxFuture<'_, Result<DlqStats>> {
        Box::pin(async move {
            let mut by_job: std::collections::BTreeMap<String, u64> =
                std::collections::BTreeMap::new();
            let mut oldest: Option<DateTime<Utc>> = None;
            let mut total = 0;

            for id in self.members(&self.dead_key()?, u32::MAX).await? {
                let Some(held) = self.load(id).await? else {
                    continue;
                };
                total += 1;
                *by_job.entry(held.job.name.clone()).or_default() += 1;
                let failed = held.failed_at.unwrap_or(held.job.enqueued_at);
                oldest = Some(oldest.map_or(failed, |current: DateTime<Utc>| current.min(failed)));
            }

            let mut by_job: Vec<(String, u64)> = by_job.into_iter().collect();
            by_job.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
            Ok(DlqStats {
                total,
                by_job,
                oldest,
            })
        })
    }
}

/// The dead-letter view of one held job.
fn dead_letter(held: &Held) -> DeadLetter {
    DeadLetter {
        id: held.job.id,
        name: held.job.name.clone(),
        queue: held.job.queue.clone(),
        payload: held.job.payload.clone(),
        attempts: held.attempts.max(held.job.attempt),
        last_error: held
            .job
            .last_error
            .clone()
            .unwrap_or_else(|| "no error was recorded".to_owned()),
        enqueued_at: held.job.enqueued_at,
        failed_at: held.failed_at.unwrap_or(held.job.enqueued_at),
        trace_parent: held.job.trace_parent.clone(),
        worker: held.job.locked_by.clone(),
        actor: held.job.actor.clone(),
    }
}

/// The error a worker gets when its lease was taken from under it.
fn reclaimed(id: JobId) -> crate::Error {
    crate::Error::permanent(format!(
        "the lease on job {id} was reclaimed; another worker is running it now"
    ))
}
