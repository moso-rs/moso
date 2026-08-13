//! Transactional enqueue for a backend that has none: the transactional
//! outbox, and the relay that drains it.
//!
//! # What is actually happening, and what it costs
//!
//! With the Postgres queue, `tx.enqueue(..)` writes the job row in the caller's
//! transaction and there is nothing else to say. With Redis there is no shared
//! transaction to join, so this wrapper writes the job to a **table** inside the
//! caller's transaction and a **relay** moves it to Redis afterwards. The
//! application gets the same guarantee — a rolled-back transaction leaves no job
//! — and pays for it three ways, all of which an operator should know about:
//!
//! 1. **A table and a relay.** One more thing to migrate, one more thing to run.
//! 2. **Latency.** A transactionally enqueued job is not visible to a worker
//!    until the relay has moved it: one relay interval, 50 ms by default.
//! 3. **A lag metric that matters.** `moso_jobs_outbox_lag_seconds` is the one
//!    number whose failure is invisible from the queue's own metrics — the jobs
//!    are sitting in a table nobody is looking at. [`Outbox::lag`] is it.
//!
//! Delivery from the outbox is at-least-once, like everything else here: the
//! relay deletes a row after the inner queue accepted it, so a crash between the
//! two replays one job. That is why every job must be idempotent, and why the
//! relayed row keeps the identifier it was given in the transaction — the inner
//! queue's own deduplication then collapses the replay.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_orm::{Backend, Db, Executor as _, RawQuery, Tx};

use crate::{
    DeadLetter, DeadLetterQueue, DlqFilter, DlqStats, JobId, Lease, Queue, QueueCapabilities,
    QueueStats, QueuedJob, Result, WorkerId,
};

/// Transactional enqueue for a backend that has none.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_jobs::backend::Outbox;
/// use moso_jobs::Queue;
/// use moso_orm::Db;
///
/// # fn f(db: Db, inner: Arc<dyn Queue>) {
/// let _ = Outbox::new(db, inner);
/// # }
/// ```
pub struct Outbox {
    /// Where the outbox table is.
    db: Db,
    /// Where jobs eventually go.
    inner: std::sync::Arc<dyn Queue>,
    /// How often the relay drains the table.
    interval: Duration,
    /// How many rows the relay moves per pass.
    batch: u32,
    /// The table's name.
    table: String,
    /// Whether the table has been created in this process.
    migrated: tokio::sync::OnceCell<()>,
    /// The inner queue's dead-letter view, when the caller handed one over.
    ///
    /// `Arc<dyn Queue>` cannot be asked whether it is also a
    /// [`DeadLetterQueue`], so the caller says. Without it the dead-letter
    /// operations refuse by name rather than reporting an empty queue, which
    /// would read as "nothing has failed".
    dead: Option<std::sync::Arc<dyn DeadLetterQueue>>,
}

impl Outbox {
    /// Wrap `inner`, staging transactional enqueues in `db`.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{backend::Outbox, Queue};
    /// # use moso_orm::Db;
    /// # fn f(db: Db, inner: Arc<dyn Queue>) { let _ = Outbox::new(db, inner); }
    /// ```
    #[must_use]
    pub fn new(db: Db, inner: std::sync::Arc<dyn Queue>) -> Self {
        Self {
            db,
            inner,
            interval: Duration::from_millis(50),
            batch: 100,
            table: "moso_jobs_outbox".to_owned(),
            migrated: tokio::sync::OnceCell::new(),
            dead: None,
        }
    }

    /// Serve the inner queue's dead letters through this wrapper.
    ///
    /// `Arc<dyn Queue>` carries no way to ask whether the same object is also a
    /// [`DeadLetterQueue`], so the composition root that built both says so.
    /// Without this, the dead-letter operations return
    /// [`Error::Unsupported`](crate::Error::Unsupported) — which is the truth,
    /// and better than an empty list that reads as "nothing has failed".
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{backend::Outbox, DeadLetterQueue, Queue};
    /// # use moso_orm::Db;
    /// # fn f(db: Db, inner: Arc<dyn Queue>, dead: Arc<dyn DeadLetterQueue>) {
    /// let _ = Outbox::new(db, inner).with_dead_letters(dead);
    /// # }
    /// ```
    #[must_use]
    pub fn with_dead_letters(mut self, dead: std::sync::Arc<dyn DeadLetterQueue>) -> Self {
        self.dead = Some(dead);
        self
    }

    /// The message for a dead-letter operation with nothing behind it.
    fn no_dead_letters(&self) -> crate::Error {
        crate::Error::Unsupported {
            backend: "outbox",
            operation: "dead letters",
        }
    }

    /// How often the relay drains the table. Default 50 ms.
    ///
    /// This is the added latency of a transactional enqueue, so it is the
    /// number to lower when a job needs to start sooner and the number to raise
    /// when the relay is the busiest query in the database.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{backend::Outbox, Queue};
    /// # use moso_orm::Db;
    /// # fn f(db: Db, inner: Arc<dyn Queue>) {
    /// let _ = Outbox::new(db, inner).interval(std::time::Duration::from_millis(20));
    /// # }
    /// ```
    #[must_use]
    pub fn interval(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// How many rows the relay moves per pass. Default 100.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{backend::Outbox, Queue};
    /// # use moso_orm::Db;
    /// # fn f(db: Db, inner: Arc<dyn Queue>) { let _ = Outbox::new(db, inner).batch(500); }
    /// ```
    #[must_use]
    pub fn batch(mut self, batch: u32) -> Self {
        self.batch = batch.max(1);
        self
    }

    /// Create the outbox table, if it is not there.
    ///
    /// # Errors
    ///
    /// Whatever the database said.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::Outbox;
    /// # async fn f(o: &Outbox) -> moso_jobs::Result { o.migrate().await }
    /// ```
    pub async fn migrate(&self) -> Result {
        self.migrated
            .get_or_try_init(|| async {
                let timestamp = match self.db.backend() {
                    Backend::Postgres => "timestamptz",
                    // SQLite has type affinity rather than types; `text` is
                    // what `chrono` round-trips through on it.
                    _ => "text",
                };
                RawQuery::new(format!(
                    "create table if not exists {table} (\
                     id text primary key, \
                     job text not null, \
                     staged_at {timestamp} not null)",
                    table = self.table,
                ))
                .execute(&self.db)
                .await?;
                RawQuery::new(format!(
                    "create index if not exists {table}_staged_idx on {table} (staged_at)",
                    table = self.table,
                ))
                .execute(&self.db)
                .await?;
                Ok(())
            })
            .await
            .copied()
    }

    /// Run the relay until the shutdown signal fires.
    ///
    /// Exactly one process needs to run it, but running it everywhere is safe:
    /// the drain uses `FOR UPDATE SKIP LOCKED`, so a row is moved once.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the database
    /// cannot be reached at startup.
    ///
    /// ```no_run
    /// # use moso_core::shutdown::Signal;
    /// # use moso_jobs::backend::Outbox;
    /// # async fn f(o: Outbox, s: Signal) -> moso_jobs::Result { o.relay(s).await }
    /// ```
    pub async fn relay(&self, shutdown: moso_core::shutdown::Signal) -> Result {
        self.migrate().await?;
        tracing::info!(
            target: "moso::jobs",
            interval = %humantime::format_duration(self.interval),
            batch = self.batch,
            inner = self.inner.name(),
            "the outbox relay is running; transactionally enqueued jobs are visible after one \
             interval"
        );

        while !shutdown.is_shutting_down() {
            match self.relay_once().await {
                // Nothing to move: wait a tick, or leave if the process is
                // going down.
                Ok(0) => {
                    tokio::select! {
                        () = tokio::time::sleep(self.interval) => {}
                        () = shutdown.recv() => break,
                    }
                }
                // A full batch means there is probably more; go straight round
                // again rather than adding an interval of lag per batch.
                Ok(moved) => {
                    if let Ok(lag) = self.lag().await {
                        crate::metrics::outbox_lag(lag);
                    }
                    if moved < u64::from(self.batch) {
                        tokio::select! {
                            () = tokio::time::sleep(self.interval) => {}
                            () = shutdown.recv() => break,
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        target: "moso::jobs",
                        error = %error.chain(),
                        "the outbox relay could not drain; retrying"
                    );
                    tokio::select! {
                        () = tokio::time::sleep(self.interval) => {}
                        () = shutdown.recv() => break,
                    }
                }
            }
        }

        // One last pass, so a clean shutdown does not leave rows that were
        // committed a millisecond before the signal.
        let _ = self.relay_once().await;
        Ok(())
    }

    /// Move one batch. Returns how many jobs reached the inner queue.
    ///
    /// Public because a test — and a deployment that would rather drive the
    /// relay from its own scheduler — needs a way to move rows without a loop.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    ///
    /// ```no_run
    /// # use moso_jobs::backend::Outbox;
    /// # async fn f(o: &Outbox) -> moso_jobs::Result<u64> { o.relay_once().await }
    /// ```
    pub async fn relay_once(&self) -> Result<u64> {
        self.migrate().await?;

        let skip = match self.db.backend() {
            Backend::Postgres => " for update skip locked",
            _ => "",
        };
        let mut placeholder = String::new();
        self.db.dialect().placeholder(0, &mut placeholder);

        let rows = (&self.db)
            .handle()
            .fetch_all_sql(
                RawQuery::new(format!(
                    "select id, job from {table} order by staged_at limit {placeholder}{skip}",
                    table = self.table,
                ))
                .bind(i64::from(self.batch))
                .into_sql(),
            )
            .await?;

        let mut moved = 0;
        for row in &rows {
            let id = row.get_string(0)?;
            let job: QueuedJob = serde_json::from_str(&row.get_string(1)?)?;

            // Push first, delete second. A crash between the two replays one
            // job, which the inner queue's deduplication collapses — and which
            // is the right way round, because the other order loses work.
            self.inner.push(job).await?;

            let mut placeholder = String::new();
            self.db.dialect().placeholder(0, &mut placeholder);
            RawQuery::new(format!(
                "delete from {table} where id = {placeholder}",
                table = self.table,
            ))
            .bind_text(&id)
            .execute(&self.db)
            .await?;
            moved += 1;
        }

        if moved > 0 {
            tracing::debug!(target: "moso::jobs", moved, "the outbox relay moved jobs");
        }
        Ok(moved)
    }

    /// How far behind the relay is: the age of the oldest unrelayed row.
    ///
    /// Exposed as `moso_jobs_outbox_lag_seconds`, because the outbox is the one
    /// piece whose failure is invisible from the queue's own metrics.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    ///
    /// ```no_run
    /// # use moso_jobs::backend::Outbox;
    /// # async fn f(o: &Outbox) -> moso_jobs::Result<Option<std::time::Duration>> { o.lag().await }
    /// ```
    pub async fn lag(&self) -> Result<Option<Duration>> {
        self.migrate().await?;
        let row = (&self.db)
            .handle()
            .fetch_optional_sql(
                RawQuery::new(format!(
                    "select min(staged_at) from {table}",
                    table = self.table
                ))
                .into_sql(),
            )
            .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let Some(oldest) = row.get_opt::<DateTime<Utc>>(0)? else {
            return Ok(None);
        };
        Ok((Utc::now() - oldest).to_std().ok())
    }

    /// How many rows are waiting to be relayed.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    ///
    /// ```no_run
    /// # use moso_jobs::backend::Outbox;
    /// # async fn f(o: &Outbox) -> moso_jobs::Result<u64> { o.pending().await }
    /// ```
    pub async fn pending(&self) -> Result<u64> {
        self.migrate().await?;
        let row = (&self.db)
            .handle()
            .fetch_optional_sql(
                RawQuery::new(format!("select count(*) from {table}", table = self.table))
                    .into_sql(),
            )
            .await?;
        Ok(row
            .map(|row| row.get_i64(0))
            .transpose()?
            .and_then(|count| u64::try_from(count).ok())
            .unwrap_or(0))
    }
}

impl core::fmt::Debug for Outbox {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Outbox")
            .field("inner", &self.inner.name())
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl Queue for Outbox {
    fn name(&self) -> &'static str {
        self.inner.name()
    }

    fn capabilities(&self) -> QueueCapabilities {
        QueueCapabilities {
            // The whole reason this wrapper exists. Everything else is the
            // inner queue's answer, unchanged.
            transactional_enqueue: true,
            ..self.inner.capabilities()
        }
    }

    fn serial_jobs(&self, names: &[&str]) {
        // The inner queue does the leasing, so it is the one that has to know
        // which names may only be leased once.
        self.inner.serial_jobs(names);
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result> {
        // No transaction to stage for, so no reason to add a relay interval of
        // latency.
        self.inner.push(job)
    }

    fn push_tx<'a>(&'a self, tx: &'a Tx, job: QueuedJob) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            self.migrate().await?;
            let mut p0 = String::new();
            let mut p1 = String::new();
            let mut p2 = String::new();
            self.db.dialect().placeholder(0, &mut p0);
            self.db.dialect().placeholder(1, &mut p1);
            self.db.dialect().placeholder(2, &mut p2);

            RawQuery::new(format!(
                "insert into {table} (id, job, staged_at) values ({p0}, {p1}, {p2}) \
                 on conflict do nothing",
                table = self.table,
            ))
            .bind_text(&job.id.to_string())
            .bind_text(&serde_json::to_string(&job)?)
            .bind(Utc::now())
            .execute(tx)
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
        self.inner.pull(queues, limit, lease, worker)
    }

    fn ack<'a>(&'a self, lease: Lease) -> BoxFuture<'a, Result> {
        self.inner.ack(lease)
    }

    fn nack<'a>(
        &'a self,
        lease: Lease,
        error: &'a str,
        run_at: Option<DateTime<Utc>>,
    ) -> BoxFuture<'a, Result> {
        self.inner.nack(lease, error, run_at)
    }

    fn heartbeat<'a>(&'a self, lease: &'a Lease, extend: Duration) -> BoxFuture<'a, Result> {
        self.inner.heartbeat(lease, extend)
    }

    fn reclaim<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<u64>> {
        self.inner.reclaim(queues)
    }

    fn stats<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<Vec<QueueStats>>> {
        self.inner.stats(queues)
    }

    fn cancel<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<bool>> {
        self.inner.cancel(id)
    }

    fn find<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        // A staged row is not findable, on purpose: it is not on the queue yet,
        // and reporting it as a running job would make the overlap check refuse
        // an occurrence that has not started.
        self.inner.find(id)
    }

    fn record_schedule_run<'a>(&'a self, run: &'a crate::ScheduleRun) -> BoxFuture<'a, Result> {
        self.inner.record_schedule_run(run)
    }

    fn schedule_runs(&self) -> BoxFuture<'_, Result<Vec<crate::ScheduleRun>>> {
        self.inner.schedule_runs()
    }

    fn wait_for_work<'a>(&'a self, queues: &'a [String], max_wait: Duration) -> BoxFuture<'a, ()> {
        self.inner.wait_for_work(queues, max_wait)
    }

    fn probe(&self) -> BoxFuture<'_, Result> {
        Box::pin(async move {
            self.inner.probe().await?;
            self.migrate().await
        })
    }
}

impl DeadLetterQueue for Outbox {
    fn list<'a>(
        &'a self,
        filter: &'a DlqFilter,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<DeadLetter>, Option<String>)>> {
        match &self.dead {
            Some(dead) => dead.list(filter, cursor, limit),
            None => Box::pin(async move { Err(self.no_dead_letters()) }),
        }
    }

    fn get<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<DeadLetter>>> {
        match &self.dead {
            Some(dead) => dead.get(id),
            None => Box::pin(async move { Err(self.no_dead_letters()) }),
        }
    }

    fn retry<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        match &self.dead {
            Some(dead) => dead.retry(filter, limit),
            None => Box::pin(async move { Err(self.no_dead_letters()) }),
        }
    }

    fn discard<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        match &self.dead {
            Some(dead) => dead.discard(filter, limit),
            None => Box::pin(async move { Err(self.no_dead_letters()) }),
        }
    }

    fn stats(&self) -> BoxFuture<'_, Result<DlqStats>> {
        match &self.dead {
            Some(dead) => dead.stats(),
            None => Box::pin(async move { Err(self.no_dead_letters()) }),
        }
    }
}
