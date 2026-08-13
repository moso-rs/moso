//! The [`Queue`] backend trait, the row it moves, and what a backend can do.
//!
//! Dyn-compatible (decision D4): which backend a process uses is configuration,
//! and the enqueue path must not be generic over it.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::{JobId, Priority, Result, RetryPolicy};

/// Where a job is in its life.
///
/// ```
/// use moso_jobs::JobState;
///
/// assert!(JobState::Running.is_active());
/// assert!(!JobState::Dead.is_active());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum JobState {
    /// Waiting to be picked up, now or at `run_at`.
    Ready,
    /// Leased by a worker.
    Running,
    /// Failed and waiting for its backoff to elapse.
    Retrying,
    /// Finished. Kept briefly for the dashboard, then swept.
    Done,
    /// Out of retries, or failed permanently. In the dead-letter queue.
    Dead,
    /// Cancelled by an operator before it ran.
    Cancelled,
}

impl JobState {
    /// Whether the job still has work ahead of it.
    ///
    /// ```
    /// use moso_jobs::JobState;
    ///
    /// assert!(JobState::Retrying.is_active());
    /// ```
    #[must_use]
    pub const fn is_active(self) -> bool {
        matches!(self, Self::Ready | Self::Running | Self::Retrying)
    }

    /// The name used in the queue table and in metric labels.
    ///
    /// ```
    /// use moso_jobs::JobState;
    ///
    /// assert_eq!(JobState::Dead.as_str(), "dead");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Retrying => "retrying",
            Self::Done => "done",
            Self::Dead => "dead",
            Self::Cancelled => "cancelled",
        }
    }
}

/// One job as the queue stores it.
///
/// The wire form, so the same struct describes a Postgres row, a Redis hash and
/// an in-memory entry — and so a dashboard can render any of them.
///
/// ```no_run
/// use moso_jobs::QueuedJob;
///
/// # fn f(j: &QueuedJob) {
/// let _ = &j.payload;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QueuedJob {
    /// Which row.
    pub id: JobId,
    /// The job's wire name, matched against the registry.
    pub name: String,
    /// Which queue it is on.
    pub queue: String,
    /// The serialised payload. Opaque here — only the registered deserialiser
    /// knows its shape, which is what keeps the queue generic.
    pub payload: serde_json::Value,
    /// Where it is in its life.
    pub state: JobState,
    /// How urgent.
    pub priority: Priority,
    /// Which attempt is next, one-based.
    pub attempt: u32,
    /// The retry policy this row was enqueued with.
    pub retry: RetryPolicy,
    /// Not before this time.
    pub run_at: DateTime<Utc>,
    /// When it was enqueued.
    pub enqueued_at: DateTime<Utc>,
    /// The deduplication key, when one applies.
    pub unique_key: Option<String>,
    /// The trace context of the request that enqueued it.
    pub trace_parent: Option<String>,
    /// The opaque identity of whoever enqueued it, for audit.
    ///
    /// Captured from [`actor::current`](crate::actor::current) at enqueue and
    /// restored when a worker runs the job, so a background action can be
    /// attributed to the subject that scheduled it. It is an **identity**, never
    /// a credential — `moso-authz` writes it with `ActorIdentity::to_wire` and
    /// reads it with `ActorIdentity::from_wire`, and this crate treats it as an
    /// opaque string. `#[serde(default)]` so a row written before this field
    /// existed still decodes.
    #[serde(default)]
    pub actor: Option<String>,
    /// The last failure's error chain, for the dashboard and the dead letter.
    pub last_error: Option<String>,
    /// Which worker holds the lease, when one does.
    pub locked_by: Option<crate::WorkerId>,
    /// When the lease expires.
    pub locked_until: Option<DateTime<Utc>>,
}

impl QueuedJob {
    /// A ready row for `name` on `queue`, carrying `payload`.
    ///
    /// The type is `#[non_exhaustive]` — a field added later must not break a
    /// third-party backend — so this is how anything outside the crate builds
    /// one: a custom [`Queue`] that has to synthesise a row in `pull`, a test
    /// that wants a poison payload on the queue, and the dead-letter retry path
    /// that puts a buried job back.
    ///
    /// Everything else has a documented default: attempt 1, [`JobState::Ready`],
    /// [`Priority::Normal`], the default retry policy, ready now, no
    /// deduplication key and no trace context. The builder methods set the rest.
    ///
    /// ```
    /// use moso_jobs::{JobState, Priority, QueuedJob};
    ///
    /// let row = QueuedJob::new("send_welcome_email", "mail", serde_json::json!({ "id": 7 }));
    /// assert_eq!(row.state, JobState::Ready);
    /// assert_eq!(row.priority, Priority::Normal);
    /// assert_eq!(row.attempt, 1);
    /// ```
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        queue: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: JobId::new(),
            name: name.into(),
            queue: queue.into(),
            payload,
            state: JobState::Ready,
            priority: Priority::Normal,
            attempt: 1,
            retry: RetryPolicy::default(),
            run_at: now,
            enqueued_at: now,
            unique_key: None,
            trace_parent: None,
            actor: None,
            last_error: None,
            locked_by: None,
            locked_until: None,
        }
    }

    /// Give this row a chosen identifier.
    ///
    /// ```
    /// # use moso_jobs::{JobId, QueuedJob};
    /// let id = JobId::new();
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null).with_id(id);
    /// assert_eq!(row.id, id);
    /// ```
    #[must_use]
    pub fn with_id(mut self, id: JobId) -> Self {
        self.id = id;
        self
    }

    /// Set how urgent it is.
    ///
    /// ```
    /// # use moso_jobs::{Priority, QueuedJob};
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null)
    ///     .with_priority(Priority::High);
    /// assert_eq!(row.priority, Priority::High);
    /// ```
    #[must_use]
    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    /// Set the retry policy the row carries.
    ///
    /// Carried on the row rather than read from the type at retry time, so a
    /// policy change applies to jobs enqueued after the deploy and does not
    /// retroactively rewrite what a queued row promised.
    ///
    /// ```
    /// # use moso_jobs::{Backoff, QueuedJob, RetryPolicy};
    /// let policy = RetryPolicy::new(3, Backoff::Immediate);
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null).with_retry(policy);
    /// assert_eq!(row.retry, policy);
    /// ```
    #[must_use]
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Make it ready at `run_at` rather than now.
    ///
    /// ```
    /// # use moso_jobs::QueuedJob;
    /// let later = chrono::Utc::now() + chrono::Duration::hours(1);
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null).with_run_at(later);
    /// assert_eq!(row.run_at, later);
    /// ```
    #[must_use]
    pub fn with_run_at(mut self, run_at: DateTime<Utc>) -> Self {
        self.run_at = run_at;
        self
    }

    /// Set the deduplication key.
    ///
    /// ```
    /// # use moso_jobs::QueuedJob;
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null)
    ///     .with_unique_key("welcome:7");
    /// assert_eq!(row.unique_key.as_deref(), Some("welcome:7"));
    /// ```
    #[must_use]
    pub fn with_unique_key(mut self, key: impl Into<String>) -> Self {
        self.unique_key = Some(key.into());
        self
    }

    /// Carry a W3C trace context onto the row.
    ///
    /// ```
    /// # use moso_jobs::QueuedJob;
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null)
    ///     .with_trace_parent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
    /// assert!(row.trace_parent.is_some());
    /// ```
    #[must_use]
    pub fn with_trace_parent(mut self, traceparent: impl Into<String>) -> Self {
        self.trace_parent = Some(traceparent.into());
        self
    }

    /// Carry the enqueueing actor's opaque identity onto the row.
    ///
    /// The value is `moso-authz`'s `ActorIdentity::to_wire` string — an
    /// identity, never a credential. See [`QueuedJob::actor`].
    ///
    /// ```
    /// # use moso_jobs::QueuedJob;
    /// let row = QueuedJob::new("j", "default", serde_json::Value::Null).with_actor("usr_7");
    /// assert_eq!(row.actor.as_deref(), Some("usr_7"));
    /// ```
    #[must_use]
    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }
}

/// A lease on a job, held while a worker runs it.
///
/// Not `Copy` and not `Clone`: two live leases on the same job is exactly the
/// bug this type exists to make hard to write.
///
/// ```no_run
/// use moso_jobs::Lease;
///
/// # fn f(l: &Lease) {
/// let _ = l.job_id();
/// # }
/// ```
#[derive(Debug)]
pub struct Lease {
    /// Which job.
    job_id: JobId,
    /// The token the backend checks on every heartbeat and acknowledgement, so
    /// a worker whose lease was reclaimed cannot acknowledge somebody else's
    /// run.
    token: String,
    /// When it expires.
    expires_at: DateTime<Utc>,
}

impl Lease {
    /// Build a lease. Called by a backend, not by an application.
    ///
    /// ```no_run
    /// # use chrono::{DateTime, Utc};
    /// # use moso_jobs::{JobId, Lease};
    /// # fn f(id: JobId, at: DateTime<Utc>) { let _ = Lease::new(id, "tok", at); }
    /// ```
    #[must_use]
    pub fn new(job_id: JobId, token: impl Into<String>, expires_at: DateTime<Utc>) -> Self {
        Self {
            job_id,
            token: token.into(),
            expires_at,
        }
    }

    /// Which job.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobId, Lease};
    /// # fn f(l: &Lease) { let _: JobId = l.job_id(); }
    /// ```
    #[must_use]
    pub fn job_id(&self) -> JobId {
        self.job_id
    }

    /// The token a backend checks.
    ///
    /// ```no_run
    /// # use moso_jobs::Lease;
    /// # fn f(l: &Lease) { let _: &str = l.token(); }
    /// ```
    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// When it expires.
    ///
    /// ```no_run
    /// # use chrono::{DateTime, Utc};
    /// # use moso_jobs::Lease;
    /// # fn f(l: &Lease) { let _: DateTime<Utc> = l.expires_at(); }
    /// ```
    #[must_use]
    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }
}

/// One queue's depth and latency, for metrics and the dashboard.
///
/// ```no_run
/// use moso_jobs::QueueStats;
///
/// # fn f(s: &QueueStats) {
/// let _ = s.ready;
/// # }
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct QueueStats {
    /// Which queue.
    pub queue: String,
    /// Waiting to run now.
    pub ready: u64,
    /// Leased by a worker.
    pub running: u64,
    /// Waiting for a backoff.
    pub retrying: u64,
    /// In the dead-letter queue.
    pub dead: u64,
    /// How long the oldest ready job has been waiting — the number that
    /// actually tells an operator whether the queue is keeping up.
    pub oldest_ready: Option<Duration>,
}

/// What a queue backend can do.
///
/// ```
/// use moso_jobs::QueueCapabilities;
///
/// let caps = QueueCapabilities::minimal();
/// assert!(!caps.transactional_enqueue);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct QueueCapabilities {
    /// Whether a job row can be written inside a caller's transaction.
    ///
    /// The headline feature, and only Postgres has it natively. The Redis
    /// backend answers `false` and the outbox relay covers the gap — which the
    /// boot log states plainly, because "we use the outbox pattern" is
    /// information an operator needs.
    pub transactional_enqueue: bool,
    /// Whether the backend pushes rather than polls, so a job starts in
    /// milliseconds instead of at the next poll.
    pub push_notify: bool,
    /// Whether the backend deduplicates on a unique key.
    pub unique_keys: bool,
    /// Whether [`Job::SERIAL`](crate::Job::SERIAL) can be honoured.
    ///
    /// A backend that answers `true` promises two things: that it keeps the
    /// list [`Queue::serial_jobs`] hands it, and that it will not lease a second
    /// row of a named job while one is leased anywhere in the fleet. A backend
    /// that answers `false` is not broken — it simply cannot serialise, and
    /// [`Worker::validate`](crate::Worker::validate) turns "a serial job on a
    /// backend that cannot serialise it" into a boot problem rather than letting
    /// it look like it worked.
    pub serial_chains: bool,
    /// Whether jobs survive a restart of the backend.
    pub durable: bool,
    /// Whether an operator can cancel a running job.
    pub cancel: bool,
    /// The smallest delay the backend can honour.
    pub min_delay: Duration,
}

impl QueueCapabilities {
    /// The conservative set: enqueue, lease, acknowledge, and nothing else.
    ///
    /// ```
    /// use moso_jobs::QueueCapabilities;
    ///
    /// assert!(!QueueCapabilities::minimal().durable);
    /// ```
    #[must_use]
    pub const fn minimal() -> Self {
        Self {
            transactional_enqueue: false,
            push_notify: false,
            unique_keys: false,
            serial_chains: false,
            durable: false,
            cancel: false,
            min_delay: Duration::from_secs(1),
        }
    }
}

impl Default for QueueCapabilities {
    fn default() -> Self {
        Self::minimal()
    }
}

/// Where jobs are stored between being enqueued and being run.
///
/// ```no_run
/// use moso_jobs::{Queue, QueuedJob};
///
/// async fn push(queue: &dyn Queue, job: QueuedJob) -> moso_jobs::Result {
///     queue.push(job).await
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a job queue",
    label = "not a queue backend",
    note = "a queue backend is `Send + Sync + 'static` and implements `name`, `capabilities`, \
            `push`, `pull`, `ack`, `nack`, `heartbeat` and `stats`",
    note = "help: use a shipped backend — `PgQueue` for transactional enqueue, `RedisQueue` for \
            throughput, `MemoryQueue` for tests and `moso dev`",
    note = "help: to write your own, `impl Queue for {Self}` and start from \
            `QueueCapabilities::minimal()`; the optional methods already fail honestly"
)]
pub trait Queue: Send + Sync + 'static {
    /// The backend's name, for logs, metrics and error messages.
    fn name(&self) -> &'static str;

    /// What this backend supports.
    fn capabilities(&self) -> QueueCapabilities;

    /// Learn which job names must never have two instances running at once.
    ///
    /// [`Job::SERIAL`](crate::Job::SERIAL) is a property of the *type* and a
    /// backend sees only rows, so the one place that holds both a queue and a
    /// registry — [`Jobs::new`](crate::Jobs::new) — tells the backend once, at
    /// construction. A backend that keeps the list refuses to lease a second row
    /// of a named job while one is leased **anywhere in the fleet**, which is
    /// what makes `SERIAL` mean something rather than being a constant nothing
    /// reads.
    ///
    /// Serialisation is per **job type**, not per (job type, argument): two
    /// enqueues of the same serial job with different payloads still run one
    /// after the other. Per-argument exclusion is what
    /// [`unique_key`](QueuedJob::unique_key) already does.
    ///
    /// The default keeps nothing, which is the honest behaviour for a backend
    /// whose [`QueueCapabilities::serial_chains`] is false.
    fn serial_jobs(&self, names: &[&str]) {
        let _ = names;
    }

    /// Enqueue one job.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the backend
    /// cannot be reached. A duplicate under an active
    /// [`unique_key`](QueuedJob::unique_key) is **not** an error: it is a
    /// successful no-op, because that is what deduplication means.
    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result>;

    /// Enqueue inside a caller's transaction.
    ///
    /// The single most valuable thing in this crate. With the Postgres backend
    /// the job row is written in the same transaction as the work that caused
    /// it, so it is *impossible* to send a welcome email for a user whose
    /// creation rolled back.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`QueueCapabilities::transactional_enqueue`] is false — which the
    /// Redis backend answers, and which
    /// `backend::Outbox` exists to work around.
    fn push_tx<'a>(&'a self, tx: &'a moso_orm::Tx, job: QueuedJob) -> BoxFuture<'a, Result> {
        let _ = (tx, job);
        let backend = self.name();
        Box::pin(async move {
            Err(crate::Error::Unsupported {
                backend,
                operation: "push_tx",
            })
        })
    }

    /// Lease up to `limit` ready jobs from `queues`.
    ///
    /// Returns the rows and their leases. An empty vector means nothing was
    /// ready, which is not an error — the worker backs off and asks again, or
    /// waits for a push notification when the backend has one.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn pull<'a>(
        &'a self,
        queues: &'a [String],
        limit: u32,
        lease: Duration,
        worker: crate::WorkerId,
    ) -> BoxFuture<'a, Result<Vec<(QueuedJob, Lease)>>>;

    /// Mark a job finished.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable), or
    /// [`Error::permanent`](crate::Error::permanent) when the lease had already
    /// been reclaimed — which means another worker owns the job now.
    fn ack<'a>(&'a self, lease: Lease) -> BoxFuture<'a, Result>;

    /// Mark an attempt failed, scheduling the retry or the dead letter.
    ///
    /// `run_at` is `None` when the retry budget is exhausted, which moves the
    /// job to [`JobState::Dead`] with its payload intact.
    ///
    /// # Errors
    ///
    /// As [`ack`](Queue::ack).
    fn nack<'a>(
        &'a self,
        lease: Lease,
        error: &'a str,
        run_at: Option<DateTime<Utc>>,
    ) -> BoxFuture<'a, Result>;

    /// Extend a lease.
    ///
    /// # Errors
    ///
    /// As [`ack`](Queue::ack).
    fn heartbeat<'a>(&'a self, lease: &'a Lease, extend: Duration) -> BoxFuture<'a, Result>;

    /// Reclaim jobs whose lease expired, so a dead worker's work is retried.
    ///
    /// Returns how many were reclaimed. Called periodically by every worker,
    /// which is why it must be safe to run concurrently.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn reclaim<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<u64>>;

    /// Depth and latency per queue.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn stats<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<Vec<QueueStats>>>;

    /// Ask a running job to stop, and stop a ready one from starting.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when
    /// [`QueueCapabilities::cancel`] is false.
    fn cancel<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<bool>> {
        let _ = id;
        let backend = self.name();
        Box::pin(async move {
            Err(crate::Error::Unsupported {
                backend,
                operation: "cancel",
            })
        })
    }

    /// One row by identifier, in whatever state it is in.
    ///
    /// The question [`stats`](Queue::stats) cannot answer, because it counts a
    /// whole queue: the scheduler needs to know whether *this schedule's own*
    /// previous occurrence is still going, and "something on that queue is
    /// running" is a different question with a different answer.
    ///
    /// `None` means the row is gone — finished and swept, or discarded — which
    /// is indistinguishable from "finished" and is treated as such.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when the backend
    /// cannot look a row up by identifier, and
    /// [`Error::Unavailable`](crate::Error::Unavailable) when it cannot be
    /// reached.
    fn find<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        let _ = id;
        let backend = self.name();
        Box::pin(async move {
            Err(crate::Error::Unsupported {
                backend,
                operation: "find",
            })
        })
    }

    /// Record that a schedule fired, so any process can say when it last ran.
    ///
    /// Leadership is per process and the dashboard is served by whichever
    /// process the request reached, so "when did the nightly job last run" has
    /// no in-process answer. It is written here, in the one store every process
    /// in the fleet already shares.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) when the backend keeps
    /// no schedule state, and
    /// [`Error::Unavailable`](crate::Error::Unavailable) when it cannot be
    /// reached. Neither stops the occurrence: this is bookkeeping for an
    /// operator, not part of the delivery guarantee.
    fn record_schedule_run<'a>(&'a self, run: &'a crate::ScheduleRun) -> BoxFuture<'a, Result> {
        let _ = run;
        let backend = self.name();
        Box::pin(async move {
            Err(crate::Error::Unsupported {
                backend,
                operation: "record_schedule_run",
            })
        })
    }

    /// The last recorded run of every schedule.
    ///
    /// # Errors
    ///
    /// As [`record_schedule_run`](Queue::record_schedule_run).
    fn schedule_runs(&self) -> BoxFuture<'_, Result<Vec<crate::ScheduleRun>>> {
        let backend = self.name();
        Box::pin(async move {
            Err(crate::Error::Unsupported {
                backend,
                operation: "schedule_runs",
            })
        })
    }

    /// Wait until something might be ready on `queues`.
    ///
    /// The push half of the loop: the Postgres backend listens on a channel,
    /// Redis blocks on a list, and the default sleeps for `max_wait`. A backend
    /// that returns early when nothing is ready is correct but wasteful; one
    /// that returns late loses latency.
    fn wait_for_work<'a>(&'a self, queues: &'a [String], max_wait: Duration) -> BoxFuture<'a, ()> {
        let _ = queues;
        Box::pin(tokio::time::sleep(max_wait))
    }

    /// A readiness probe.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the backend
    /// cannot be reached.
    fn probe(&self) -> BoxFuture<'_, Result> {
        // A backend with nothing to reach is always reachable. Overridden by
        // the two that have a socket to lose.
        Box::pin(async { Ok(()) })
    }
}

/// A queue whose job rows carry an explicit sort order.
///
/// The order every backend implements: priority first, then the time the job
/// became ready. Extracted so the memory backend and the tests can sort by
/// exactly what the Postgres backend's index does, rather than by something
/// that happens to agree today.
pub(crate) fn pull_order(job: &QueuedJob) -> (core::cmp::Reverse<i16>, DateTime<Utc>, JobId) {
    (
        core::cmp::Reverse(job.priority.as_i16()),
        job.run_at,
        job.id,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every optional operation has to fail *honestly* — naming the backend and
    /// the operation — rather than panicking or returning a plausible zero.
    #[tokio::test]
    async fn the_optional_operations_refuse_by_name() {
        struct Bare;
        impl Queue for Bare {
            fn name(&self) -> &'static str {
                "bare"
            }
            fn capabilities(&self) -> QueueCapabilities {
                QueueCapabilities::minimal()
            }
            fn push<'a>(&'a self, _job: QueuedJob) -> BoxFuture<'a, Result> {
                Box::pin(async { Ok(()) })
            }
            fn pull<'a>(
                &'a self,
                _queues: &'a [String],
                _limit: u32,
                _lease: Duration,
                _worker: crate::WorkerId,
            ) -> BoxFuture<'a, Result<Vec<(QueuedJob, Lease)>>> {
                Box::pin(async { Ok(Vec::new()) })
            }
            fn ack<'a>(&'a self, _lease: Lease) -> BoxFuture<'a, Result> {
                Box::pin(async { Ok(()) })
            }
            fn nack<'a>(
                &'a self,
                _lease: Lease,
                _error: &'a str,
                _run_at: Option<DateTime<Utc>>,
            ) -> BoxFuture<'a, Result> {
                Box::pin(async { Ok(()) })
            }
            fn heartbeat<'a>(
                &'a self,
                _lease: &'a Lease,
                _extend: Duration,
            ) -> BoxFuture<'a, Result> {
                Box::pin(async { Ok(()) })
            }
            fn reclaim<'a>(&'a self, _queues: &'a [String]) -> BoxFuture<'a, Result<u64>> {
                Box::pin(async { Ok(0) })
            }
            fn stats<'a>(
                &'a self,
                _queues: &'a [String],
            ) -> BoxFuture<'a, Result<Vec<QueueStats>>> {
                Box::pin(async { Ok(Vec::new()) })
            }
        }

        let queue = Bare;
        let cancelled = queue.cancel(JobId::new()).await;
        assert!(matches!(
            cancelled,
            Err(crate::Error::Unsupported {
                backend: "bare",
                operation: "cancel"
            })
        ));

        // A backend with no remote is always reachable.
        queue.probe().await.expect("nothing to reach");
    }

    /// The pull order is what makes `Priority::High` mean anything. High before
    /// normal, and within one priority the job that became ready first.
    #[test]
    fn the_pull_order_is_priority_then_readiness() {
        let base = QueuedJob {
            id: JobId::new(),
            name: "j".to_owned(),
            queue: "default".to_owned(),
            payload: serde_json::Value::Null,
            state: JobState::Ready,
            priority: Priority::Normal,
            attempt: 1,
            retry: RetryPolicy::default(),
            run_at: Utc::now(),
            enqueued_at: Utc::now(),
            unique_key: None,
            trace_parent: None,
            actor: None,
            last_error: None,
            locked_by: None,
            locked_until: None,
        };

        let mut urgent = base.clone();
        urgent.priority = Priority::High;
        urgent.run_at = base.run_at + chrono::Duration::hours(1);

        let mut older = base.clone();
        older.run_at = base.run_at - chrono::Duration::hours(1);

        let mut jobs = [base.clone(), urgent.clone(), older.clone()];
        jobs.sort_by_key(pull_order);

        assert_eq!(jobs[0].priority, Priority::High, "priority wins");
        assert_eq!(jobs[1].run_at, older.run_at, "then the oldest ready");
        assert_eq!(jobs[2].run_at, base.run_at);
    }
}
