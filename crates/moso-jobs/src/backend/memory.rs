//! The in-memory queue: tests and `moso dev`.

use std::sync::RwLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;

use crate::{
    DeadLetter, DeadLetterQueue, DlqFilter, DlqStats, JobId, JobState, Lease, Queue,
    QueueCapabilities, QueueStats, QueuedJob, Result, WorkerId,
};

/// A queue in memory. Tests and `moso dev`.
///
/// The same semantics as the durable backends — leases, retries, the
/// dead-letter queue, unique keys — with none of the durability, which is
/// exactly what makes it a usable test double rather than a mock.
///
/// ```
/// use moso_jobs::backend::MemoryQueue;
///
/// let queue = MemoryQueue::new();
/// assert_eq!(queue.len(), 0);
/// ```
#[derive(Debug)]
pub struct MemoryQueue {
    /// Everything queued, by identifier.
    jobs: RwLock<std::collections::BTreeMap<JobId, Entry>>,
    /// A cap, so a runaway test fails loudly instead of exhausting the machine.
    max_jobs: usize,
    /// Wakes a worker parked in `wait_for_work`, so an in-process test does not
    /// wait out a poll interval for a job that is already there.
    notify: tokio::sync::Notify,
    /// The wire names that must never have two instances leased at once.
    serial: RwLock<std::collections::BTreeSet<String>>,
    /// The last recorded run of each schedule, by its key.
    schedules: RwLock<std::collections::BTreeMap<crate::ScheduleId, crate::ScheduleRun>>,
}

/// One held job, plus the lease token that owns it.
#[derive(Clone, Debug)]
struct Entry {
    /// The row itself.
    job: QueuedJob,
    /// The token the current lease holder presented.
    token: Option<String>,
    /// How many attempts have been made, for the dead-letter record.
    attempts: u32,
    /// When it gave up, for the dead-letter record.
    failed_at: Option<DateTime<Utc>>,
}

/// The documented cap.
const DEFAULT_MAX_JOBS: usize = 100_000;

impl Default for MemoryQueue {
    /// The same queue [`MemoryQueue::new`] builds.
    ///
    /// Written out rather than derived: a derived `Default` would give
    /// `max_jobs = 0`, and a queue that refuses the first job it is handed is
    /// exactly the plausible-looking wrong value a test would spend an
    /// afternoon on.
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryQueue {
    /// An empty queue, capped at 100,000 jobs.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// assert!(MemoryQueue::new().is_empty());
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: RwLock::new(std::collections::BTreeMap::new()),
            max_jobs: DEFAULT_MAX_JOBS,
            notify: tokio::sync::Notify::new(),
            serial: RwLock::new(std::collections::BTreeSet::new()),
            schedules: RwLock::new(std::collections::BTreeMap::new()),
        }
    }

    /// Cap the queue at `max_jobs`.
    ///
    /// A test that enqueues in a loop should fail with a message, not with the
    /// machine swapping.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// let _ = MemoryQueue::new().max_jobs(10);
    /// ```
    #[must_use]
    pub fn max_jobs(mut self, max_jobs: usize) -> Self {
        self.max_jobs = max_jobs;
        self
    }

    /// How many jobs are held, in any state.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// assert_eq!(MemoryQueue::new().len(), 0);
    /// ```
    #[must_use]
    pub fn len(&self) -> usize {
        self.read().len()
    }

    /// Whether nothing is held.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// assert!(MemoryQueue::new().is_empty());
    /// ```
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Everything enqueued for one job's wire name.
    ///
    /// What `app.jobs().assert_enqueued::<SendWelcomeEmail>(1)` reads.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// assert!(MemoryQueue::new().enqueued("send_welcome_email").is_empty());
    /// ```
    #[must_use]
    pub fn enqueued(&self, name: &str) -> Vec<QueuedJob> {
        self.read()
            .values()
            .filter(|entry| entry.job.name == name)
            .map(|entry| entry.job.clone())
            .collect()
    }

    /// Every job held, in any state.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// assert!(MemoryQueue::new().all().is_empty());
    /// ```
    #[must_use]
    pub fn all(&self) -> Vec<QueuedJob> {
        self.read()
            .values()
            .map(|entry| entry.job.clone())
            .collect()
    }

    /// Forget everything.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// let queue = MemoryQueue::new();
    /// queue.clear();
    /// assert!(queue.is_empty());
    /// ```
    pub fn clear(&self) {
        self.write().clear();
    }

    /// Move the queue's clock forward, so delayed and scheduled jobs become
    /// ready without anything sleeping.
    ///
    /// What `app.advance_time(1.hour())` drives. A test for a job delayed by an
    /// hour should take microseconds.
    ///
    /// ```
    /// # use moso_jobs::backend::MemoryQueue;
    /// MemoryQueue::new().advance(std::time::Duration::from_secs(3600));
    /// ```
    pub fn advance(&self, by: Duration) {
        let shift = chrono::Duration::from_std(by).unwrap_or(chrono::Duration::MAX);
        {
            let mut guard = self.write();
            for entry in guard.values_mut() {
                entry.job.run_at -= shift;
                // Leases move too, or advancing time past a lease would leave a
                // job leased forever in a test that was trying to expire it.
                if let Some(until) = entry.job.locked_until {
                    entry.job.locked_until = Some(until - shift);
                }
            }
        }
        self.notify.notify_waiters();
    }

    /// The lock, without the poison ceremony at every call site.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, std::collections::BTreeMap<JobId, Entry>> {
        self.jobs
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// As [`read`](MemoryQueue::read), for writing.
    fn write(&self) -> std::sync::RwLockWriteGuard<'_, std::collections::BTreeMap<JobId, Entry>> {
        self.jobs
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The rows a worker could lease right now, in pull order.
    ///
    /// Shared by `pull` and `wait_for_work` on purpose: a `wait_for_work` that
    /// reports work a `pull` will then refuse turns the worker's loop into a
    /// spin, and a serial job with a queue full of its own rows would spin it
    /// for as long as the first instance runs.
    fn leasable(
        jobs: &std::collections::BTreeMap<JobId, Entry>,
        serial: &std::collections::BTreeSet<String>,
        queues: &[String],
        now: DateTime<Utc>,
    ) -> Vec<JobId> {
        // A unique key already running blocks the rest of its chain: the
        // per-payload ordering guarantee.
        let busy_chains: std::collections::BTreeSet<&str> = jobs
            .values()
            .filter(|entry| entry.job.state == JobState::Running)
            .filter_map(|entry| entry.job.unique_key.as_deref())
            .collect();

        // `Job::SERIAL` is the per-*type* one: a leased row of a serial job
        // blocks every other row of that job, whatever its payload.
        let mut busy_serial: std::collections::BTreeSet<String> = jobs
            .values()
            .filter(|entry| {
                entry.job.state == JobState::Running
                    && entry.job.locked_until.is_some_and(|until| until > now)
                    && serial.contains(&entry.job.name)
            })
            .map(|entry| entry.job.name.clone())
            .collect();

        let mut ready: Vec<JobId> = jobs
            .values()
            .filter(|entry| {
                matches!(entry.job.state, JobState::Ready | JobState::Retrying)
                    && entry.job.run_at <= now
                    && queues.contains(&entry.job.queue)
            })
            .filter(|entry| match &entry.job.unique_key {
                Some(key) => !busy_chains.contains(key.as_str()),
                None => true,
            })
            .map(|entry| entry.job.id)
            .collect();

        ready.sort_by_key(|id| crate::queue::pull_order(&jobs[id].job));
        // The serial filter runs *after* the sort, so the row a serial job gets
        // is the one the pull order chose rather than whichever the map happened
        // to iterate first.
        ready.retain(|id| {
            let name = &jobs[id].job.name;
            !serial.contains(name) || busy_serial.insert(name.clone())
        });
        ready
    }

    /// The names that may only be leased once at a time.
    fn serial_names(&self) -> std::collections::BTreeSet<String> {
        self.serial
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl Queue for MemoryQueue {
    fn name(&self) -> &'static str {
        "memory"
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
            // No transaction to join: `push_tx` refuses, and the boot log says
            // so rather than pretending the guarantee is there.
            transactional_enqueue: false,
            push_notify: true,
            unique_keys: true,
            serial_chains: true,
            durable: false,
            cancel: true,
            min_delay: Duration::ZERO,
        }
    }

    fn push<'a>(&'a self, job: QueuedJob) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let mut guard = self.write();
            if guard.len() >= self.max_jobs {
                return Err(crate::Error::unavailable(
                    "memory",
                    format!(
                        "the in-memory queue is capped at {} jobs\n\
                         help: this cap exists so a runaway test fails with a message instead \
                         of exhausting the machine; raise it with \
                         `MemoryQueue::new().max_jobs(..)` if the test really needs more",
                        self.max_jobs
                    ),
                ));
            }

            // Deduplication is a successful no-op, not an error: that is what
            // deduplication means.
            if let Some(key) = &job.unique_key
                && guard.values().any(|entry| {
                    entry.job.state.is_active() && entry.job.unique_key.as_ref() == Some(key)
                })
            {
                return Ok(());
            }

            guard.insert(
                job.id,
                Entry {
                    job,
                    token: None,
                    attempts: 0,
                    failed_at: None,
                },
            );
            drop(guard);
            self.notify.notify_waiters();
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
            let now = Utc::now();
            let until = now + chrono::Duration::from_std(lease).unwrap_or_default();
            let serial = self.serial_names();
            let mut guard = self.write();

            let mut ready = Self::leasable(&guard, &serial, queues, now);
            ready.truncate(limit as usize);

            let mut leased = Vec::with_capacity(ready.len());
            for id in ready {
                let token = uuid::Uuid::new_v4().to_string();
                let entry = guard.get_mut(&id).expect("just selected");
                entry.job.state = JobState::Running;
                entry.job.locked_by = Some(worker.clone());
                entry.job.locked_until = Some(until);
                entry.token = Some(token.clone());
                entry.attempts = entry.job.attempt;
                leased.push((entry.job.clone(), Lease::new(id, token, until)));
            }
            Ok(leased)
        })
    }

    fn ack<'a>(&'a self, lease: Lease) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let mut guard = self.write();
            let Some(entry) = guard.get_mut(&lease.job_id()) else {
                return Ok(());
            };
            if entry.token.as_deref() != Some(lease.token()) {
                return Err(reclaimed(lease.job_id()));
            }
            entry.job.state = JobState::Done;
            entry.job.locked_by = None;
            entry.job.locked_until = None;
            entry.token = None;
            drop(guard);
            // A finished job is what frees a serial chain or a unique-key
            // chain, so anything parked in `wait_for_work` wants to know now
            // rather than at the end of its poll interval.
            self.notify.notify_waiters();
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
            let mut guard = self.write();
            let Some(entry) = guard.get_mut(&lease.job_id()) else {
                return Ok(());
            };
            if entry.token.as_deref() != Some(lease.token()) {
                return Err(reclaimed(lease.job_id()));
            }
            entry.job.last_error = Some(error.to_owned());
            entry.token = None;
            entry.job.locked_until = None;
            match run_at {
                Some(at) => {
                    entry.job.state = JobState::Retrying;
                    entry.job.attempt += 1;
                    entry.job.run_at = at;
                }
                None => {
                    entry.job.state = JobState::Dead;
                    entry.failed_at = Some(Utc::now());
                }
            }
            drop(guard);
            self.notify.notify_waiters();
            Ok(())
        })
    }

    fn heartbeat<'a>(&'a self, lease: &'a Lease, extend: Duration) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            let mut guard = self.write();
            let Some(entry) = guard.get_mut(&lease.job_id()) else {
                return Err(reclaimed(lease.job_id()));
            };
            if entry.token.as_deref() != Some(lease.token()) {
                return Err(reclaimed(lease.job_id()));
            }
            entry.job.locked_until =
                Some(Utc::now() + chrono::Duration::from_std(extend).unwrap_or_default());
            Ok(())
        })
    }

    fn reclaim<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let now = Utc::now();
            let mut reclaimed = 0;
            let mut guard = self.write();
            for entry in guard.values_mut() {
                if entry.job.state == JobState::Running
                    && queues.contains(&entry.job.queue)
                    && entry.job.locked_until.is_some_and(|until| until < now)
                {
                    entry.job.state = JobState::Ready;
                    entry.job.locked_by = None;
                    entry.job.locked_until = None;
                    // Invalidating the token is what makes the old worker's
                    // `ack` fail instead of silently completing a job another
                    // worker is now running.
                    entry.token = None;
                    entry.job.run_at = now;
                    reclaimed += 1;
                }
            }
            drop(guard);
            if reclaimed > 0 {
                self.notify.notify_waiters();
            }
            Ok(reclaimed)
        })
    }

    fn stats<'a>(&'a self, queues: &'a [String]) -> BoxFuture<'a, Result<Vec<QueueStats>>> {
        Box::pin(async move {
            let now = Utc::now();
            let guard = self.read();
            let mut stats: std::collections::BTreeMap<&str, QueueStats> = queues
                .iter()
                .map(|queue| {
                    (
                        queue.as_str(),
                        QueueStats {
                            queue: queue.clone(),
                            ..QueueStats::default()
                        },
                    )
                })
                .collect();

            for entry in guard.values() {
                let Some(one) = stats.get_mut(entry.job.queue.as_str()) else {
                    continue;
                };
                match entry.job.state {
                    JobState::Ready => {
                        one.ready += 1;
                        if entry.job.run_at <= now {
                            let waited = (now - entry.job.run_at).to_std().unwrap_or_default();
                            one.oldest_ready = Some(
                                one.oldest_ready
                                    .map_or(waited, |current| current.max(waited)),
                            );
                        }
                    }
                    JobState::Running => one.running += 1,
                    JobState::Retrying => one.retrying += 1,
                    JobState::Dead => one.dead += 1,
                    JobState::Done | JobState::Cancelled => {}
                }
            }
            Ok(stats.into_values().collect())
        })
    }

    fn cancel<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async move {
            let mut guard = self.write();
            let Some(entry) = guard.get_mut(&id) else {
                return Ok(false);
            };
            if !entry.job.state.is_active() {
                return Ok(false);
            }
            entry.job.state = JobState::Cancelled;
            // Dropping the token stops the running worker from acknowledging
            // it, which is what makes cancelling a *running* job mean anything.
            entry.token = None;
            entry.job.locked_until = None;
            Ok(true)
        })
    }

    fn find<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<QueuedJob>>> {
        Box::pin(async move { Ok(self.read().get(&id).map(|entry| entry.job.clone())) })
    }

    fn record_schedule_run<'a>(&'a self, run: &'a crate::ScheduleRun) -> BoxFuture<'a, Result> {
        Box::pin(async move {
            self.schedules
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(run.schedule.clone(), run.clone());
            Ok(())
        })
    }

    fn schedule_runs(&self) -> BoxFuture<'_, Result<Vec<crate::ScheduleRun>>> {
        Box::pin(async move {
            Ok(self
                .schedules
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .values()
                .cloned()
                .collect())
        })
    }

    fn wait_for_work<'a>(&'a self, queues: &'a [String], max_wait: Duration) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            // The same eligibility `pull` applies, not "is there a ready row":
            // a row this worker cannot lease — because a serial job of the same
            // name is running, or its unique-key chain is busy — is not work,
            // and reporting it as work spins the worker's loop.
            let serial = self.serial_names();
            let has_work = !Self::leasable(&self.read(), &serial, queues, Utc::now()).is_empty();
            if has_work {
                return;
            }
            let _ = tokio::time::timeout(max_wait, self.notify.notified()).await;
        })
    }
}

impl DeadLetterQueue for MemoryQueue {
    fn list<'a>(
        &'a self,
        filter: &'a DlqFilter,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<DeadLetter>, Option<String>)>> {
        Box::pin(async move {
            let mut dead: Vec<DeadLetter> = self
                .read()
                .values()
                .filter(|entry| entry.job.state == JobState::Dead)
                .map(dead_letter)
                .filter(|letter| filter.matches(letter))
                .collect();
            // Newest failure first, as the trait documents.
            dead.sort_by_key(|letter| core::cmp::Reverse(letter.failed_at));

            let start = cursor
                .and_then(|cursor| cursor.parse::<usize>().ok())
                .unwrap_or(0);
            let end = start.saturating_add(limit as usize).min(dead.len());
            let page: Vec<DeadLetter> = dead.get(start..end).unwrap_or_default().to_vec();
            let next = (end < dead.len()).then(|| end.to_string());
            Ok((page, next))
        })
    }

    fn get<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<DeadLetter>>> {
        Box::pin(async move {
            Ok(self
                .read()
                .get(&id)
                .filter(|entry| entry.job.state == JobState::Dead)
                .map(dead_letter))
        })
    }

    fn retry<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let now = Utc::now();
            let mut retried = 0;
            let mut guard = self.write();
            for entry in guard.values_mut() {
                if retried >= u64::from(limit) {
                    break;
                }
                if entry.job.state != JobState::Dead || !filter.matches(&dead_letter(entry)) {
                    continue;
                }
                entry.job.state = JobState::Ready;
                entry.job.attempt = 1;
                entry.job.run_at = now;
                entry.job.last_error = None;
                entry.failed_at = None;
                retried += 1;
            }
            drop(guard);
            if retried > 0 {
                self.notify.notify_waiters();
            }
            Ok(retried)
        })
    }

    fn discard<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>> {
        Box::pin(async move {
            let mut guard = self.write();
            let doomed: Vec<JobId> = guard
                .values()
                .filter(|entry| entry.job.state == JobState::Dead)
                .filter(|entry| filter.matches(&dead_letter(entry)))
                .take(limit as usize)
                .map(|entry| entry.job.id)
                .collect();
            for id in &doomed {
                guard.remove(id);
            }
            Ok(doomed.len() as u64)
        })
    }

    fn stats(&self) -> BoxFuture<'_, Result<DlqStats>> {
        Box::pin(async move {
            let guard = self.read();
            let mut by_job: std::collections::BTreeMap<String, u64> =
                std::collections::BTreeMap::new();
            let mut oldest: Option<DateTime<Utc>> = None;
            let mut total = 0;

            for entry in guard.values().filter(|e| e.job.state == JobState::Dead) {
                total += 1;
                *by_job.entry(entry.job.name.clone()).or_default() += 1;
                let failed = entry.failed_at.unwrap_or(entry.job.enqueued_at);
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

/// The dead-letter view of one held entry.
fn dead_letter(entry: &Entry) -> DeadLetter {
    DeadLetter {
        id: entry.job.id,
        name: entry.job.name.clone(),
        queue: entry.job.queue.clone(),
        payload: entry.job.payload.clone(),
        attempts: entry.attempts.max(entry.job.attempt),
        last_error: entry
            .job
            .last_error
            .clone()
            .unwrap_or_else(|| "no error was recorded".to_owned()),
        enqueued_at: entry.job.enqueued_at,
        failed_at: entry.failed_at.unwrap_or(entry.job.enqueued_at),
        trace_parent: entry.job.trace_parent.clone(),
        worker: entry.job.locked_by.clone(),
        actor: entry.job.actor.clone(),
    }
}

/// The error a worker gets when its lease was taken from under it.
///
/// Permanent, and deliberately so: this worker must stop, because another one is
/// already running the job.
fn reclaimed(id: JobId) -> crate::Error {
    crate::Error::permanent(format!(
        "the lease on job {id} was reclaimed; another worker is running it now"
    ))
}
