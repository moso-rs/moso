//! The SQL-table queue: `SKIP LOCKED`, `LISTEN`/`NOTIFY`, transactional
//! enqueue, and a hot table bounded by state.
//!
//! # What "partitioned by state" means here
//!
//! Not declarative partitioning — that would put the state in the primary key
//! and rewrite the row on every transition. Three cheaper things that add up to
//! the same property, a hot table whose size is bounded by *work in flight*
//! rather than by work ever done:
//!
//! 1. A **partial index** over the active states, so the index the pull path
//!    walks holds only rows that could be pulled.
//! 2. Dead letters live in **their own table**. A job that gave up keeps its
//!    payload for as long as somebody might retry it, and none of that weight is
//!    in the way of the queue.
//! 3. Finished rows are **swept** after [`PgQueue::keep_done`], on the same tick
//!    the worker already reclaims expired leases.
//!
//! # SQLite
//!
//! The same statements run on SQLite, minus `FOR UPDATE SKIP LOCKED` — which
//! SQLite does not need, because it serialises writers — and minus
//! `LISTEN`/`NOTIFY`, so a worker polls instead. That is not a second
//! implementation: it is the same SQL with two clauses chosen by dialect, which
//! is what keeps `moso dev` and a test suite honest against the code that runs
//! in production.

use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use moso_orm::{Backend, Db, Executor as _, RawQuery, Tx};

use super::wire;
use crate::{
    DeadLetter, DeadLetterQueue, DlqFilter, DlqStats, JobId, JobState, Lease, Queue,
    QueueCapabilities, QueueStats, QueuedJob, Result, WorkerId,
};

/// The `NOTIFY` channel every queue shares.
///
/// One channel with the queue name as the payload, rather than one channel per
/// queue: `LISTEN` needs an identifier and a queue name is user input, so a
/// per-queue channel would mean quoting attacker-controlled text into DDL-shaped
/// SQL. The payload is data, and a worker that wakes for another queue's job
/// goes straight back to sleep.
const CHANNEL: &str = "moso_jobs";

/// PostgreSQL, with `SKIP LOCKED` and `LISTEN`/`NOTIFY`.
///
/// The default, and the only backend with native transactional enqueue. It
/// handles low thousands of jobs per second, which is more than the
/// overwhelming majority of applications will ever need — and it needs no
/// service the application does not already run.
///
/// ```no_run
/// use moso_jobs::backend::PgQueue;
/// use moso_orm::Db;
///
/// # fn f(db: Db) {
/// let _ = PgQueue::new(db);
/// # }
/// ```
#[derive(Debug)]
pub struct PgQueue {
    /// Where the tables are.
    db: Db,
    /// The table holding queued jobs.
    table: String,
    /// The table holding dead letters.
    dlq_table: String,
    /// The table holding one row per schedule's last occurrence.
    schedule_table: String,
    /// The wire names that must never have two instances leased at once.
    ///
    /// Handed over by [`Jobs::new`](crate::Jobs::new), which is the one place
    /// that holds both a queue and a registry. Empty in a process that has no
    /// serial jobs, and the pull statement then carries no serial clause at all.
    serial: std::sync::RwLock<Vec<String>>,
    /// How long a finished row is kept for the dashboard before being swept.
    keep_done: Duration,
    /// How often the sweeper runs. `Duration::ZERO` turns it off, for a
    /// deployment that runs it from `cron` instead.
    sweep_interval: Duration,
    /// When the sweeper last ran, so `reclaim` can carry it without a task.
    last_sweep: std::sync::Mutex<Option<std::time::Instant>>,
}

impl PgQueue {
    /// A queue on `db`, using the default table names.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = PgQueue::new(db); }
    /// ```
    #[must_use]
    pub fn new(db: Db) -> Self {
        Self {
            db,
            table: "moso_jobs".to_owned(),
            dlq_table: "moso_jobs_dead".to_owned(),
            schedule_table: "moso_jobs_schedules".to_owned(),
            serial: std::sync::RwLock::new(Vec::new()),
            keep_done: Duration::from_secs(3600),
            sweep_interval: Duration::from_secs(300),
            last_sweep: std::sync::Mutex::new(None),
        }
    }

    /// Use a different table name prefix.
    ///
    /// For an application that shares a database with something else, or one
    /// whose migrations put framework tables in their own schema.
    ///
    /// The prefix is reduced to `[A-Za-z0-9_]` before it reaches any SQL: it is
    /// the one value in this backend that becomes an *identifier* rather than a
    /// bound parameter, so it is the one value that has to be proved safe.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = PgQueue::new(db).table_prefix("shop_jobs"); }
    /// ```
    #[must_use]
    pub fn table_prefix(mut self, prefix: &str) -> Self {
        let safe: String = prefix
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(48)
            .collect();
        let safe = if safe.is_empty() {
            "moso_jobs".to_owned()
        } else {
            safe
        };
        self.dlq_table = format!("{safe}_dead");
        self.schedule_table = format!("{safe}_schedules");
        self.table = safe;
        self
    }

    /// How long finished rows are kept before being swept.
    ///
    /// Also the window in which a completed job still deduplicates a new one
    /// with the same key — see the note on [`Queue::push`] for this backend.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = PgQueue::new(db).keep_done(std::time::Duration::from_secs(600)); }
    /// ```
    #[must_use]
    pub fn keep_done(mut self, keep: Duration) -> Self {
        self.keep_done = keep;
        self
    }

    /// How often finished rows are swept. `Duration::ZERO` turns it off.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { let _ = PgQueue::new(db).sweep_interval(std::time::Duration::ZERO); }
    /// ```
    #[must_use]
    pub fn sweep_interval(mut self, interval: Duration) -> Self {
        self.sweep_interval = interval;
        self
    }

    /// The queue table's name.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { assert_eq!(PgQueue::new(db).table(), "moso_jobs"); }
    /// ```
    #[must_use]
    pub fn table(&self) -> &str {
        &self.table
    }

    /// The dead-letter table's name.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { assert_eq!(PgQueue::new(db).dead_table(), "moso_jobs_dead"); }
    /// ```
    #[must_use]
    pub fn dead_table(&self) -> &str {
        &self.dlq_table
    }

    /// The schedule-state table's name.
    ///
    /// One row per schedule, holding who fired it last and when — the durable
    /// answer `GET /_jobs/schedules` gives from any process in the fleet.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # use moso_orm::Db;
    /// # fn f(db: Db) { assert_eq!(PgQueue::new(db).schedule_table(), "moso_jobs_schedules"); }
    /// ```
    #[must_use]
    pub fn schedule_table(&self) -> &str {
        &self.schedule_table
    }

    /// The wire names this queue will not lease twice at once.
    fn serial_names(&self) -> Vec<String> {
        self.serial
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Create the tables and indexes, if they are not there.
    ///
    /// Idempotent, cheap after the first call, and deliberately **not** a
    /// migration: this schema has no application-visible shape and no foreign
    /// keys, and making every application generate a migration for the
    /// framework's own queue would be ceremony for its own sake. Call it at
    /// boot; `push` and `pull` call it too, so a test does not have to.
    ///
    /// # Errors
    ///
    /// Whatever the database said.
    ///
    /// ```no_run
    /// # use moso_jobs::backend::PgQueue;
    /// # async fn f(q: &PgQueue) -> moso_jobs::Result { q.migrate().await }
    /// ```
    pub async fn migrate(&self) -> Result {
        static DONE: std::sync::Mutex<Option<std::collections::BTreeSet<String>>> =
            std::sync::Mutex::new(None);

        let key = format!("{}::{}", self.db.backend().as_str(), self.table);
        {
            let guard = DONE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if guard.as_ref().is_some_and(|done| done.contains(&key)) {
                return Ok(());
            }
        }

        for statement in self.ddl() {
            RawQuery::new(statement).execute(&self.db).await?;
        }

        DONE.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(Default::default)
            .insert(key);
        Ok(())
    }

    /// Every DDL statement, in order.
    fn ddl(&self) -> Vec<String> {
        let table = &self.table;
        let dead = &self.dlq_table;
        let timestamp = match self.db.backend() {
            Backend::Postgres => "timestamptz",
            // SQLite has type affinity rather than types; `text` is what
            // `chrono` round-trips through on it.
            _ => "text",
        };

        vec![
            format!(
                "create table if not exists {table} (\
                 id text primary key, \
                 name text not null, \
                 queue text not null, \
                 payload text not null, \
                 state text not null, \
                 priority smallint not null, \
                 attempt bigint not null, \
                 max_attempts bigint not null, \
                 backoff text not null, \
                 run_at {timestamp} not null, \
                 enqueued_at {timestamp} not null, \
                 unique_key text, \
                 trace_parent text, \
                 last_error text, \
                 locked_by text, \
                 locked_until {timestamp}, \
                 lock_token text, \
                 actor text, \
                 finished_at {timestamp})"
            ),
            // The pull path's index, and the reason the hot table stays small:
            // it holds only rows that could be pulled.
            format!(
                "create index if not exists {table}_ready_idx on {table} \
                 (queue, priority desc, run_at) \
                 where state in ('ready', 'retrying')"
            ),
            // At most one *active* row per deduplication key. This is what makes
            // `unique_for` mean something, and what makes a `unique_key` chain
            // serial without a lock.
            format!(
                "create unique index if not exists {table}_unique_active_idx on {table} \
                 (unique_key) \
                 where unique_key is not null and state in ('ready', 'running', 'retrying')"
            ),
            // The reclaim path.
            format!(
                "create index if not exists {table}_locked_idx on {table} (locked_until) \
                 where state = 'running'"
            ),
            // The sweeper's.
            format!(
                "create index if not exists {table}_finished_idx on {table} (finished_at) \
                 where state = 'done'"
            ),
            format!(
                "create table if not exists {dead} (\
                 id text primary key, \
                 name text not null, \
                 queue text not null, \
                 payload text not null, \
                 attempts bigint not null, \
                 last_error text not null, \
                 enqueued_at {timestamp} not null, \
                 failed_at {timestamp} not null, \
                 trace_parent text, \
                 worker text, \
                 actor text)"
            ),
            format!("create index if not exists {dead}_failed_idx on {dead} (failed_at desc)"),
            format!("create index if not exists {dead}_name_idx on {dead} (name)"),
            // One row per schedule. Leadership is per process and the dashboard
            // is served by whichever process the request reached, so "when did
            // the nightly job last run" has no in-process answer — it lives
            // here, where every process can read it.
            format!(
                "create table if not exists {schedules} (\
                 id text primary key, \
                 job text not null, \
                 leader text not null, \
                 ran_at {timestamp} not null)",
                schedules = self.schedule_table,
            ),
        ]
    }

    /// The placeholder for parameter `index`, zero-based, in this dialect.
    ///
    /// PostgreSQL numbers them and SQLite does not, and getting that wrong
    /// silently binds the arguments in the wrong order — which is why it goes
    /// through the dialect rather than through a `format!`.
    fn placeholder(&self, index: usize) -> String {
        let mut out = String::new();
        self.db.dialect().placeholder(index, &mut out);
        out
    }

    /// `($1, $2, …)` for `count` parameters starting at `from`.
    fn placeholders(&self, from: usize, count: usize) -> String {
        let list: Vec<String> = (from..from + count).map(|i| self.placeholder(i)).collect();
        format!("({})", list.join(", "))
    }

    /// The `for update skip locked` clause, where the dialect has one.
    ///
    /// SQLite serialises writers, so it does not need one and does not have
    /// one. Emitting it there would be a syntax error, and skipping it on
    /// PostgreSQL would make two workers fight over the same row.
    fn skip_locked(&self) -> &'static str {
        match self.db.backend() {
            Backend::Postgres => " for update skip locked",
            // SQLite serialises writers, so it does not need one — and
            // emitting it there is a syntax error.
            _ => "",
        }
    }

    /// The `where` fragment that makes [`Job::SERIAL`](crate::Job::SERIAL) true.
    ///
    /// Empty when no registered job is serial, so the overwhelmingly common
    /// deployment pays nothing at all for this.
    ///
    /// # Why three conditions and not one
    ///
    /// The claim on a serial job is the job's own **lease** — there is no second
    /// lock to take, renew, release or leak, and a worker that dies frees the
    /// chain exactly when its lease expires. Three things have to hold for that
    /// to be airtight:
    ///
    /// 1. **No live lease.** `not exists (a running row of this name whose lease
    ///    has not expired)` covers every worker that already started one.
    /// 2. **One row per name per statement.** Without `min(id)`, a single pull
    ///    asking for eight rows could lease eight rows of the same serial job:
    ///    none of them is running yet, so all eight pass condition 1.
    /// 3. **No overlapping statements.** Two workers whose pull statements
    ///    overlap both take their snapshot before either commits, so both would
    ///    pass condition 1. `pg_try_advisory_xact_lock` is held for the
    ///    statement, so exactly one of them gets past it; by the time the loser
    ///    asks again, the winner's row is committed and condition 1 refuses it.
    ///    SQLite needs no equivalent because it serialises writers outright.
    ///
    /// The lock key is the job name hashed to a `bigint` with documented
    /// functions only — `md5` and a `bit(64)` cast — rather than with the
    /// internal `hashtext`, so nothing here depends on a function PostgreSQL
    /// does not promise. A hash collision costs one skipped pull for one round,
    /// never a second instance.
    fn serial_clause(&self, serial: &[String], queues: usize, index: &mut usize) -> String {
        if serial.is_empty() {
            return String::new();
        }
        let p_serial = self.placeholders(*index, serial.len());
        *index += serial.len();
        let p_running_now = self.placeholder(*index);
        *index += 1;
        let p_oldest_now = self.placeholder(*index);
        *index += 1;
        let p_serial_queues = self.placeholders(*index, queues);
        *index += queues;

        let advisory = match self.db.backend() {
            Backend::Postgres => {
                " and pg_try_advisory_xact_lock(\
                 ('x' || substr(md5('moso_jobs:serial:' || j.name), 1, 16))::bit(64)::bigint)"
            }
            _ => "",
        };

        format!(
            " and (j.name not in {p_serial} or (\
             not exists (\
               select 1 from {table} r \
               where r.name = j.name and r.state = 'running' \
                 and r.locked_until is not null and r.locked_until > {p_running_now}\
             ) \
             and j.id = (\
               select min(k.id) from {table} k \
               where k.name = j.name and k.state in ('ready', 'retrying') \
                 and k.run_at <= {p_oldest_now} and k.queue in {p_serial_queues}\
             ){advisory}\
             ))",
            table = self.table,
        )
    }

    /// Turn any database error into this crate's, naming the backend.
    fn fail(error: moso_orm::Error) -> crate::Error {
        let retryable = crate::Error::from(error);
        match retryable {
            crate::Error::Retryable { detail, source } => crate::Error::Unavailable {
                backend: "postgres",
                detail,
                source,
            },
            other => other,
        }
    }

    /// Insert one row on `executor`, returning whether it was new.
    async fn insert(&self, executor: impl moso_orm::Executor<'_>, job: &QueuedJob) -> Result<bool> {
        let columns = wire::COLUMNS;
        // The eighteen columns `COLUMNS` names, plus `finished_at`, which is
        // the sweeper's and is never part of the wire form.
        let values = self.placeholders(0, wire::COLUMN_COUNT + 1);
        // `do nothing` and not an error: a duplicate under an active unique key
        // is a successful no-op, because that is what deduplication means.
        let statement = format!(
            "insert into {table} ({columns}, finished_at) values {values} \
             on conflict do nothing",
            table = self.table,
        );

        let affected = self
            .bind_row(RawQuery::new(statement), job)
            .execute(executor)
            .await
            .map_err(Self::fail)?;
        Ok(affected > 0)
    }

    /// Bind one row's eighteen columns, plus the null `finished_at`.
    fn bind_row(&self, query: RawQuery, job: &QueuedJob) -> RawQuery {
        query
            .bind_text(&job.id.to_string())
            .bind_text(&job.name)
            .bind_text(&job.queue)
            .bind_text(&job.payload.to_string())
            .bind_text(job.state.as_str())
            .bind(job.priority.as_i16())
            .bind(i64::from(job.attempt))
            .bind(i64::from(job.retry.max_attempts()))
            .bind_text(&serde_json::to_string(&job.retry.backoff()).unwrap_or_default())
            .bind(job.run_at)
            .bind(job.enqueued_at)
            .bind(job.unique_key.clone())
            .bind(job.trace_parent.clone())
            .bind(job.last_error.clone())
            .bind(job.locked_by.as_ref().map(|worker| worker.as_str().to_owned()))
            .bind(job.locked_until)
            // `lock_token`: a freshly enqueued row holds none — a typed null
            // rather than an empty string so a `where lock_token = …` can never
            // match one.
            .bind(None::<String>)
            // `actor`: the enqueueing identity, the last wire column.
            .bind(job.actor.clone())
            // `finished_at`: the sweeper's, never part of the wire form.
            .bind(None::<DateTime<Utc>>)
    }

    /// Wake anything parked in `wait_for_work`.
    async fn notify(&self, queue: &str) {
        if self.db.backend() != Backend::Postgres {
            return;
        }
        let statement = format!(
            "select pg_notify({}, {})",
            self.placeholder(0),
            self.placeholder(1)
        );
        if let Err(error) = RawQuery::new(statement)
            .bind_text(CHANNEL)
            .bind_text(queue)
            .execute(&self.db)
            .await
        {
            // A missed notification costs latency, not correctness: the worker
            // falls back to its poll interval.
            tracing::debug!(
                target: "moso::jobs",
                error = %error,
                "could not notify listeners; workers will pick this up on their next poll"
            );
        }
    }

    /// Delete finished rows older than `keep_done`, at most every
    /// `sweep_interval`.
    async fn sweep(&self) {
        if self.sweep_interval.is_zero() {
            return;
        }
        {
            let mut guard = self
                .last_sweep
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let now = std::time::Instant::now();
            if guard.is_some_and(|last| now.duration_since(last) < self.sweep_interval) {
                return;
            }
            *guard = Some(now);
        }

        let cutoff = Utc::now()
            - chrono::Duration::from_std(self.keep_done)
                .unwrap_or_else(|_| chrono::Duration::zero());
        let statement = format!(
            "delete from {table} where state in ('done', 'cancelled') and finished_at < {p}",
            table = self.table,
            p = self.placeholder(0),
        );
        match RawQuery::new(statement)
            .bind(cutoff)
            .execute(&self.db)
            .await
        {
            Ok(0) => {}
            Ok(count) => tracing::debug!(
                target: "moso::jobs",
                count,
                "swept finished job rows"
            ),
            Err(error) => tracing::warn!(
                target: "moso::jobs",
                error = %error,
                "could not sweep finished job rows"
            ),
        }
    }

    /// Move one row into the dead-letter table, in one transaction.
    async fn bury(&self, lease: &Lease, error: &str) -> Result {
        let id = lease.job_id().to_string();
        let token = lease.token().to_owned();
        let table = self.table.clone();
        let dead = self.dlq_table.clone();
        let now = Utc::now();
        let p0 = self.placeholder(0);
        let p1 = self.placeholder(1);
        let p2 = self.placeholder(2);
        let p3 = self.placeholder(3);

        // An explicit `begin`/`commit` rather than `Db::transaction`: the
        // closure that helper takes is a higher-ranked `AsyncFnMut(&Tx)`, and a
        // borrow of `self` across it does not satisfy the `Send` bound a
        // `BoxFuture` needs. Two statements and one commit is clearer anyway.
        let tx = self.db.begin().await.map_err(Self::fail)?;

        let moved = RawQuery::new(format!(
            "insert into {dead} \
             (id, name, queue, payload, attempts, last_error, enqueued_at, \
              failed_at, trace_parent, worker, actor) \
             select id, name, queue, payload, attempt, {p0}, enqueued_at, {p1}, \
              trace_parent, locked_by, actor \
             from {table} where id = {p2} and lock_token = {p3}"
        ))
        .bind_text(error)
        .bind(now)
        .bind_text(&id)
        .bind_text(&token)
        .execute(&tx)
        .await
        .map_err(Self::fail)?;

        if moved > 0 {
            RawQuery::new(format!("delete from {table} where id = {p0}"))
                .bind_text(&id)
                .execute(&tx)
                .await
                .map_err(Self::fail)?;
        }

        tx.commit().await.map_err(Self::fail)?;
        if moved == 0 {
            // The lease was reclaimed between the failure and this call, so
            // another worker owns the job now and this one must not bury it.
            return Err(reclaimed(lease.job_id()));
        }
        Ok(())
    }

    /// One dead letter, decoded.
    fn decode_dead(row: &moso_orm::Row) -> Result<DeadLetter> {
        Ok(DeadLetter {
            id: row.get_string(0)?.parse()?,
            name: row.get_string(1)?,
            queue: row.get_string(2)?,
            payload: serde_json::from_str(&row.get_string(3)?)?,
            attempts: u32::try_from(row.get_i64(4)?).unwrap_or(0),
            last_error: row.get_string(5)?,
            enqueued_at: row.get_timestamp(6)?,
            failed_at: row.get_timestamp(7)?,
            trace_parent: row.get_opt::<String>(8)?,
            worker: row.get_opt::<String>(9)?.map(WorkerId::new),
            actor: row.get_opt::<String>(10)?,
        })
    }

    /// The `where` clause and bound values a [`DlqFilter`] describes.
    fn dlq_where(&self, filter: &DlqFilter, from: usize) -> (String, Vec<Bound>) {
        let mut clauses = Vec::new();
        let mut bound = Vec::new();
        let mut index = from;

        if let Some(job) = filter.job_name() {
            clauses.push(format!("name = {}", self.placeholder(index)));
            bound.push(Bound::Text(job.to_owned()));
            index += 1;
        }
        if let Some(queue) = filter.queue_name() {
            clauses.push(format!("queue = {}", self.placeholder(index)));
            bound.push(Bound::Text(queue.to_owned()));
            index += 1;
        }
        if let Some(since) = filter.since_at() {
            clauses.push(format!("failed_at >= {}", self.placeholder(index)));
            bound.push(Bound::Time(since));
            index += 1;
        }
        if let Some(until) = filter.until_at() {
            clauses.push(format!("failed_at < {}", self.placeholder(index)));
            bound.push(Bound::Time(until));
            index += 1;
        }
        if let Some(needle) = filter.error_needle() {
            // `like` and not string concatenation: the needle is a *value*, and
            // the wildcards are ours. `escape` is what makes a needle that
            // contains one of those wildcards mean itself.
            clauses.push(format!(
                "last_error like {} escape '{ESCAPE}'",
                self.placeholder(index)
            ));
            bound.push(Bound::Text(format!("%{}%", escape_like(needle))));
        }

        let clause = if clauses.is_empty() {
            "1 = 1".to_owned()
        } else {
            clauses.join(" and ")
        };
        (clause, bound)
    }
}

/// One bound value for a dynamically built filter.
enum Bound {
    /// A string.
    Text(String),
    /// A moment.
    Time(DateTime<Utc>),
}

impl Bound {
    /// Bind this onto a query.
    fn apply(&self, query: RawQuery) -> RawQuery {
        match self {
            Self::Text(text) => query.bind_text(text),
            Self::Time(time) => query.bind(*time),
        }
    }
}

/// The character every `like` in this backend escapes its wildcards with.
///
/// A backslash, and it is declared explicitly with `escape '\'` rather than
/// relying on a default: PostgreSQL's default escape *is* the backslash but
/// `standard_conforming_strings` decides how the literal is read, and SQLite has
/// **no** default escape at all — a pattern with a stray backslash there would
/// match the backslash. Saying it once, in both dialects, is the difference
/// between "usually right" and right.
const ESCAPE: char = '\\';

/// Make a needle mean itself inside a `like` pattern.
///
/// `%` and `_` are wildcards, so a needle containing one would match more than
/// the operator asked for. Escaping them — rather than *stripping* them, which
/// is what this used to do — is the difference between `50%` finding the rows
/// that say `50%` and `50%` silently searching for `50`.
///
/// The escape character escapes itself first, or a needle ending in a backslash
/// would escape the `%` this crate appends.
fn escape_like(needle: &str) -> String {
    let mut escaped = String::with_capacity(needle.len());
    for character in needle.chars() {
        if character == ESCAPE || character == '%' || character == '_' {
            escaped.push(ESCAPE);
        }
        escaped.push(character);
    }
    escaped
}

impl Queue for PgQueue {
    fn name(&self) -> &'static str {
        "postgres"
    }

    fn serial_jobs(&self, names: &[&str]) {
        let mut guard = self
            .serial
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.clear();
        guard.extend(names.iter().map(|name| (*name).to_owned()));
        // Sorted so the pull statement's text is stable, which is what lets the
        // database reuse its plan instead of re-planning per pull.
        guard.sort_unstable();
    }

    fn capabilities(&self) -> QueueCapabilities {
        QueueCapabilities {
            transactional_enqueue: true,
            // `LISTEN`/`NOTIFY` on PostgreSQL; SQLite has no equivalent and
            // says so rather than promising latency it cannot deliver.
            push_notify: self.db.backend() == Backend::Postgres,
            unique_keys: true,
            serial_chains: true,
            durable: true,
            cancel: true,
            min_delay: Duration::ZERO,
        }
    }

    /// Enqueue one job.
    ///
    /// # Deduplication, precisely
    ///
    /// The unique index covers the *active* states, so a second row with the
    /// same key is refused while the first is ready, running or retrying — and
    /// accepted once it finishes. A finished row keeps its key until the sweeper
    /// takes it, so `keep_done` is the effective floor on the deduplication
    /// window: set it at or above the longest `unique_for` in the application.
    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            self.migrate().await?;
            let queue = job.queue.clone();
            if self.insert(&self.db, &job).await? {
                self.notify(&queue).await;
            }
            Ok(())
        })
    }

    fn push_tx<'a>(&'a self, tx: &'a Tx, job: QueuedJob) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            // The schema check runs on the pool and not on `tx`: a `create
            // table` inside the caller's transaction would take a lock nobody
            // asked for, and abort *their* work if it failed.
            self.migrate().await?;
            self.insert(tx, &job).await?;
            // Deliberately no `NOTIFY` here. The row is not visible to any
            // other session until the caller commits, and a notification that
            // arrives before the row does sends every worker looking for
            // something that is not there. The poll interval covers the gap.
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
            if queues.is_empty() || limit == 0 {
                return Ok(Vec::new());
            }
            self.migrate().await?;

            let now = Utc::now();
            let until = now + chrono::Duration::from_std(lease).unwrap_or_default();
            let token = uuid::Uuid::new_v4().to_string();

            // Parameter order matters on SQLite, where placeholders are
            // positional: every one below is written in the order it is bound.
            let serial = self.serial_names();
            let mut index = 0;
            let p_token = self.placeholder(index);
            index += 1;
            let p_worker = self.placeholder(index);
            index += 1;
            let p_until = self.placeholder(index);
            index += 1;
            let p_queues = self.placeholders(index, queues.len());
            index += queues.len();
            let p_now = self.placeholder(index);
            index += 1;
            let serial_clause = self.serial_clause(&serial, queues.len(), &mut index);
            let p_limit = self.placeholder(index);

            let statement = format!(
                "update {table} set state = 'running', lock_token = {p_token}, \
                 locked_by = {p_worker}, locked_until = {p_until} \
                 where id in (\
                   select j.id from {table} j \
                   where j.queue in {p_queues} \
                     and j.state in ('ready', 'retrying') \
                     and j.run_at <= {p_now}{serial_clause} \
                   order by j.priority desc, j.run_at \
                   limit {p_limit}{skip}\
                 ) \
                 returning {columns}",
                table = self.table,
                skip = self.skip_locked(),
                columns = wire::COLUMNS,
            );

            let mut query = RawQuery::new(statement)
                .bind_text(&token)
                .bind_text(worker.as_str())
                .bind(until);
            for queue in queues {
                query = query.bind_text(queue);
            }
            query = query.bind(now);
            if !serial.is_empty() {
                for name in &serial {
                    query = query.bind_text(name);
                }
                query = query.bind(now).bind(now);
                for queue in queues {
                    query = query.bind_text(queue);
                }
            }
            query = query.bind(i64::from(limit));

            let rows = (&self.db)
                .handle()
                .fetch_all_sql(query.into_sql())
                .await
                .map_err(Self::fail)?;

            let mut leased = Vec::with_capacity(rows.len());
            for row in &rows {
                let (job, token) = wire::decode(row)?;
                let token = token.unwrap_or_default();
                let id = job.id;
                leased.push((job, Lease::new(id, token, until)));
            }
            Ok(leased)
        })
    }

    fn ack<'a>(&'a self, lease: Lease) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let now = Utc::now();
            let statement = format!(
                "update {table} set state = 'done', locked_by = null, locked_until = null, \
                 lock_token = null, finished_at = {p0} \
                 where id = {p1} and lock_token = {p2}",
                table = self.table,
                p0 = self.placeholder(0),
                p1 = self.placeholder(1),
                p2 = self.placeholder(2),
            );
            let affected = RawQuery::new(statement)
                .bind(now)
                .bind_text(&lease.job_id().to_string())
                .bind_text(lease.token())
                .execute(&self.db)
                .await
                .map_err(Self::fail)?;

            if affected == 0 {
                return Err(reclaimed(lease.job_id()));
            }
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
            let Some(at) = run_at else {
                // Out of retries: the row moves to its own table, payload
                // intact, and stops weighing on the hot one.
                return self.bury(&lease, error).await;
            };

            let statement = format!(
                "update {table} set state = 'retrying', attempt = attempt + 1, run_at = {p0}, \
                 last_error = {p1}, locked_by = null, locked_until = null, lock_token = null \
                 where id = {p2} and lock_token = {p3}",
                table = self.table,
                p0 = self.placeholder(0),
                p1 = self.placeholder(1),
                p2 = self.placeholder(2),
                p3 = self.placeholder(3),
            );
            let affected = RawQuery::new(statement)
                .bind(at)
                .bind_text(error)
                .bind_text(&lease.job_id().to_string())
                .bind_text(lease.token())
                .execute(&self.db)
                .await
                .map_err(Self::fail)?;

            if affected == 0 {
                return Err(reclaimed(lease.job_id()));
            }
            Ok(())
        })
    }

    fn heartbeat<'a>(&'a self, lease: &'a Lease, extend: Duration) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let until = Utc::now() + chrono::Duration::from_std(extend).unwrap_or_default();
            let statement = format!(
                "update {table} set locked_until = {p0} where id = {p1} and lock_token = {p2}",
                table = self.table,
                p0 = self.placeholder(0),
                p1 = self.placeholder(1),
                p2 = self.placeholder(2),
            );
            let affected = RawQuery::new(statement)
                .bind(until)
                .bind_text(&lease.job_id().to_string())
                .bind_text(lease.token())
                .execute(&self.db)
                .await
                .map_err(Self::fail)?;

            if affected == 0 {
                return Err(reclaimed(lease.job_id()));
            }
            Ok(())
        })
    }

    fn reclaim<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            if queues.is_empty() {
                return Ok(0);
            }
            self.migrate().await?;

            let now = Utc::now();
            let p_now = self.placeholder(0);
            let p_expired = self.placeholder(1);
            let p_queues = self.placeholders(2, queues.len());

            let statement = format!(
                "update {table} set state = 'ready', locked_by = null, locked_until = null, \
                 lock_token = null, run_at = {p_now} \
                 where state = 'running' and locked_until < {p_expired} and queue in {p_queues}",
                table = self.table,
            );
            let mut query = RawQuery::new(statement).bind(now).bind(now);
            for queue in queues {
                query = query.bind_text(queue);
            }
            let reclaimed = query.execute(&self.db).await.map_err(Self::fail)?;

            self.sweep().await;
            Ok(reclaimed)
        })
    }

    fn stats<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<Vec<QueueStats>>> {
        Box::pin(async move {
            if queues.is_empty() {
                return Ok(Vec::new());
            }
            self.migrate().await?;

            let now = Utc::now();
            let mut stats: std::collections::BTreeMap<String, QueueStats> = queues
                .iter()
                .map(|queue| {
                    (
                        queue.clone(),
                        QueueStats {
                            queue: queue.clone(),
                            ..QueueStats::default()
                        },
                    )
                })
                .collect();

            let statement = format!(
                "select queue, state, count(*), min(run_at) from {table} \
                 where queue in {p} and state in ('ready', 'running', 'retrying') \
                 group by queue, state",
                table = self.table,
                p = self.placeholders(0, queues.len()),
            );
            let mut query = RawQuery::new(statement);
            for queue in queues {
                query = query.bind_text(queue);
            }
            let rows = (&self.db)
                .handle()
                .fetch_all_sql(query.into_sql())
                .await
                .map_err(Self::fail)?;

            for row in &rows {
                let queue = row.get_string(0)?;
                let Some(one) = stats.get_mut(&queue) else {
                    continue;
                };
                let count = u64::try_from(row.get_i64(2)?).unwrap_or(0);
                match wire::state_from_str(&row.get_string(1)?) {
                    JobState::Ready => {
                        one.ready = count;
                        if let Some(oldest) = row.get_opt::<DateTime<Utc>>(3)?
                            && oldest <= now
                        {
                            one.oldest_ready = (now - oldest).to_std().ok();
                        }
                    }
                    JobState::Running => one.running = count,
                    JobState::Retrying => one.retrying = count,
                    _ => {}
                }
            }

            let statement = format!(
                "select queue, count(*) from {dead} where queue in {p} group by queue",
                dead = self.dlq_table,
                p = self.placeholders(0, queues.len()),
            );
            let mut query = RawQuery::new(statement);
            for queue in queues {
                query = query.bind_text(queue);
            }
            let rows = (&self.db)
                .handle()
                .fetch_all_sql(query.into_sql())
                .await
                .map_err(Self::fail)?;
            for row in &rows {
                if let Some(one) = stats.get_mut(&row.get_string(0)?) {
                    one.dead = u64::try_from(row.get_i64(1)?).unwrap_or(0);
                }
            }

            Ok(stats.into_values().collect())
        })
    }

    fn cancel<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let now = Utc::now();
            let statement = format!(
                "update {table} set state = 'cancelled', lock_token = null, \
                 locked_until = null, finished_at = {p0} \
                 where id = {p1} and state in ('ready', 'running', 'retrying')",
                table = self.table,
                p0 = self.placeholder(0),
                p1 = self.placeholder(1),
            );
            let affected = RawQuery::new(statement)
                .bind(now)
                .bind_text(&id.to_string())
                .execute(&self.db)
                .await
                .map_err(Self::fail)?;
            Ok(affected > 0)
        })
    }

    fn find<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move {
            self.migrate().await?;
            let statement = format!(
                "select {columns} from {table} where id = {p}",
                columns = wire::COLUMNS,
                table = self.table,
                p = self.placeholder(0),
            );
            let row = (&self.db)
                .handle()
                .fetch_optional_sql(
                    RawQuery::new(statement)
                        .bind_text(&id.to_string())
                        .into_sql(),
                )
                .await
                .map_err(Self::fail)?;
            row.as_ref()
                .map(|row| wire::decode(row).map(|(job, _token)| job))
                .transpose()
        })
    }

    fn record_schedule_run<'a>(&'a self, run: &'a crate::ScheduleRun) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            self.migrate().await?;
            // `excluded` rather than four more placeholders: the update wants
            // the same values the insert brought, and repeating them is two
            // places to get the order wrong.
            let statement = format!(
                "insert into {schedules} (id, job, leader, ran_at) values {values} \
                 on conflict (id) do update set job = excluded.job, \
                 leader = excluded.leader, ran_at = excluded.ran_at",
                schedules = self.schedule_table,
                values = self.placeholders(0, 4),
            );
            RawQuery::new(statement)
                .bind_text(run.schedule.as_str())
                .bind_text(&run.job)
                .bind_text(run.leader.as_str())
                .bind(run.ran_at)
                .execute(&self.db)
                .await
                .map_err(Self::fail)?;
            Ok(())
        })
    }

    fn schedule_runs(&self) -> BoxFuture<'_, Result<Vec<crate::ScheduleRun>>> {
        Box::pin(async move {
            self.migrate().await?;
            let rows = (&self.db)
                .handle()
                .fetch_all_sql(
                    RawQuery::new(format!(
                        "select id, job, leader, ran_at from {schedules} order by id",
                        schedules = self.schedule_table,
                    ))
                    .into_sql(),
                )
                .await
                .map_err(Self::fail)?;

            let mut runs = Vec::with_capacity(rows.len());
            for row in &rows {
                runs.push(crate::ScheduleRun::new(
                    crate::ScheduleId::from_key(row.get_string(0)?),
                    row.get_string(1)?,
                    WorkerId::new(row.get_string(2)?),
                    row.get_timestamp(3)?,
                ));
            }
            Ok(runs)
        })
    }

    fn wait_for_work<'a>(&'a self, queues: &'a [String], max_wait: Duration) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            if self.db.backend() != Backend::Postgres {
                // SQLite has no notification channel. Polling is the whole
                // story, and saying so is better than a `wait` that returns
                // instantly and spins.
                tokio::time::sleep(max_wait).await;
                return;
            }
            let Some(pool) = self.db.postgres_pool() else {
                tokio::time::sleep(max_wait).await;
                return;
            };

            let listen = async {
                let mut listener = match sqlx::postgres::PgListener::connect_with(pool).await {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::debug!(
                            target: "moso::jobs",
                            error = %error,
                            "could not open a listener; falling back to polling"
                        );
                        tokio::time::sleep(max_wait).await;
                        return;
                    }
                };
                if let Err(error) = listener.listen(CHANNEL).await {
                    tracing::debug!(
                        target: "moso::jobs",
                        error = %error,
                        "could not listen; falling back to polling"
                    );
                    tokio::time::sleep(max_wait).await;
                    return;
                }
                loop {
                    match listener.recv().await {
                        // The payload is the queue name. A worker woken for
                        // somebody else's queue goes straight back to sleep
                        // rather than issuing a pull that will find nothing.
                        Ok(notification) => {
                            if queues.iter().any(|queue| queue == notification.payload()) {
                                return;
                            }
                        }
                        Err(error) => {
                            tracing::debug!(
                                target: "moso::jobs",
                                error = %error,
                                "the listener dropped; falling back to polling"
                            );
                            return;
                        }
                    }
                }
            };

            let _ = tokio::time::timeout(max_wait, listen).await;
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result> {
        Box::pin(async move {
            self.db.ping().await.map_err(Self::fail)?;
            self.migrate().await
        })
    }
}

impl DeadLetterQueue for PgQueue {
    fn list<'a>(
        &'a self,
        filter: &'a DlqFilter,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<DeadLetter>, Option<String>)>> {
        Box::pin(async move {
            self.migrate().await?;
            let (clause, bound) = self.dlq_where(filter, 0);
            let offset: u64 = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
            let p_limit = self.placeholder(bound.len());
            let p_offset = self.placeholder(bound.len() + 1);

            let statement = format!(
                "select id, name, queue, payload, attempts, last_error, enqueued_at, \
                 failed_at, trace_parent, worker, actor from {dead} \
                 where {clause} order by failed_at desc, id limit {p_limit} offset {p_offset}",
                dead = self.dlq_table,
            );
            let mut query = RawQuery::new(statement);
            for value in &bound {
                query = value.apply(query);
            }
            // One extra row, so "is there a next page" needs no second count.
            query = query
                .bind(i64::from(limit) + 1)
                .bind(i64::try_from(offset).unwrap_or(0));

            let rows = (&self.db)
                .handle()
                .fetch_all_sql(query.into_sql())
                .await
                .map_err(Self::fail)?;

            let mut letters = Vec::with_capacity(rows.len());
            for row in rows.iter().take(limit as usize) {
                letters.push(Self::decode_dead(row)?);
            }
            let next =
                (rows.len() > limit as usize).then(|| (offset + u64::from(limit)).to_string());
            Ok((letters, next))
        })
    }

    fn get<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<DeadLetter>>> {
        Box::pin(async move {
            self.migrate().await?;
            let statement = format!(
                "select id, name, queue, payload, attempts, last_error, enqueued_at, \
                 failed_at, trace_parent, worker, actor from {dead} where id = {p}",
                dead = self.dlq_table,
                p = self.placeholder(0),
            );
            let row = (&self.db)
                .handle()
                .fetch_optional_sql(
                    RawQuery::new(statement)
                        .bind_text(&id.to_string())
                        .into_sql(),
                )
                .await
                .map_err(Self::fail)?;
            row.as_ref().map(Self::decode_dead).transpose()
        })
    }

    fn retry<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            self.migrate().await?;
            let (letters, _) = self.list(filter, None, limit).await?;
            if letters.is_empty() {
                return Ok(0);
            }

            let mut retried = 0;
            for letter in letters {
                // Back onto the queue with a fresh attempt counter and the
                // payload it kept, which is the whole point of holding it.
                let job = QueuedJob {
                    id: letter.id,
                    name: letter.name.clone(),
                    queue: letter.queue.clone(),
                    payload: letter.payload.clone(),
                    state: JobState::Ready,
                    priority: crate::Priority::Normal,
                    attempt: 1,
                    retry: crate::RetryPolicy::default(),
                    run_at: Utc::now(),
                    enqueued_at: Utc::now(),
                    // Deliberately no unique key: the original one may still be
                    // held by a live row, and a bulk retry that silently
                    // dropped half its rows would be worse than one that
                    // duplicates.
                    unique_key: None,
                    trace_parent: letter.trace_parent.clone(),
                    actor: letter.actor.clone(),
                    last_error: None,
                    locked_by: None,
                    locked_until: None,
                };

                let id = letter.id.to_string();
                let table = self.table.clone();
                let dead = self.dlq_table.clone();
                let p0 = self.placeholder(0);
                let values = self.placeholders(0, wire::COLUMN_COUNT + 1);

                // `begin`/`commit` rather than `Db::transaction`, for the
                // same reason `bury` does it: the helper takes a higher-ranked
                // `AsyncFnMut(&Tx)`, and a borrow of `self` across it does not
                // add up to the `Send` future a `BoxFuture` needs.
                let tx = self.db.begin().await.map_err(Self::fail)?;
                let deleted = RawQuery::new(format!("delete from {dead} where id = {p0}"))
                    .bind_text(&id)
                    .execute(&tx)
                    .await
                    .map_err(Self::fail)?;

                let moved = if deleted == 0 {
                    // Somebody else got there first.
                    0
                } else {
                    RawQuery::new(format!(
                        "insert into {table} ({columns}, finished_at) values {values} \
                         on conflict do nothing",
                        columns = wire::COLUMNS,
                    ))
                    .bind_text(&job.id.to_string())
                    .bind_text(&job.name)
                    .bind_text(&job.queue)
                    .bind_text(&job.payload.to_string())
                    .bind_text(job.state.as_str())
                    .bind(job.priority.as_i16())
                    .bind(i64::from(job.attempt))
                    .bind(i64::from(job.retry.max_attempts()))
                    .bind_text(&serde_json::to_string(&job.retry.backoff()).unwrap_or_default())
                    .bind(job.run_at)
                    .bind(job.enqueued_at)
                    .bind(None::<String>)
                    .bind(job.trace_parent.clone())
                    .bind(None::<String>)
                    .bind(None::<String>)
                    .bind(None::<DateTime<Utc>>)
                    .bind(None::<String>)
                    .bind(job.actor.clone())
                    .bind(None::<DateTime<Utc>>)
                    .execute(&tx)
                    .await
                    .map_err(Self::fail)?
                };
                tx.commit().await.map_err(Self::fail)?;

                if moved > 0 {
                    retried += 1;
                    self.notify(&letter.queue).await;
                }
            }
            Ok(retried)
        })
    }

    fn discard<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            self.migrate().await?;
            let (letters, _) = self.list(filter, None, limit).await?;
            if letters.is_empty() {
                return Ok(0);
            }
            let ids: Vec<String> = letters.iter().map(|letter| letter.id.to_string()).collect();
            let statement = format!(
                "delete from {dead} where id in {p}",
                dead = self.dlq_table,
                p = self.placeholders(0, ids.len()),
            );
            let mut query = RawQuery::new(statement);
            for id in &ids {
                query = query.bind_text(id);
            }
            query.execute(&self.db).await.map_err(Self::fail)
        })
    }

    fn stats(&self) -> BoxFuture<'_, Result<DlqStats>> {
        Box::pin(async move {
            self.migrate().await?;
            let rows = (&self.db)
                .handle()
                .fetch_all_sql(
                    RawQuery::new(format!(
                        "select name, count(*), min(failed_at) from {dead} group by name \
                         order by count(*) desc, name",
                        dead = self.dlq_table,
                    ))
                    .into_sql(),
                )
                .await
                .map_err(Self::fail)?;

            let mut total = 0;
            let mut by_job = Vec::with_capacity(rows.len());
            let mut oldest: Option<DateTime<Utc>> = None;
            for row in &rows {
                let count = u64::try_from(row.get_i64(1)?).unwrap_or(0);
                total += count;
                by_job.push((row.get_string(0)?, count));
                if let Some(failed) = row.get_opt::<DateTime<Utc>>(2)? {
                    oldest = Some(oldest.map_or(failed, |current| current.min(failed)));
                }
            }
            Ok(DlqStats {
                total,
                by_job,
                oldest,
            })
        })
    }
}

/// The error a worker gets when its lease was taken from under it.
fn reclaimed(id: JobId) -> crate::Error {
    crate::Error::permanent(format!(
        "the lease on job {id} was reclaimed; another worker is running it now"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The needle is a *value*: `%` and `_` in it mean themselves, and the
    /// escape character escapes itself so a needle ending in one cannot escape
    /// the wildcard this crate appends.
    #[test]
    fn a_like_needle_escapes_the_wildcards_rather_than_losing_them() {
        assert_eq!(escape_like("connection refused"), "connection refused");
        assert_eq!(escape_like("50% full"), r"50\% full");
        assert_eq!(escape_like("full_up"), r"full\_up");
        assert_eq!(escape_like(r"c:\path"), r"c:\\path");
        // The old behaviour deleted these, which turned "search for a percent
        // sign" into "search for something else entirely".
        assert!(escape_like("%_%").contains('%'));
        assert!(escape_like("%_%").contains('_'));
    }
}
