//! The worker: what actually runs jobs.
//!
//! `moso worker --queues default,mail --concurrency 8`, or the application's
//! own binary with the same code path. Concurrency is per-process and
//! per-queue-weighted, so a slow queue cannot starve a fast one — the failure
//! mode where one stuck `mail` job stops every `default` job from running.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::job::Cancellation;
use crate::{JobRegistry, Jobs, Result};

/// The lease a worker takes by default.
///
/// Sixty seconds is the upper bound on how late a dead worker's job is picked
/// up, and it is what [`JobRegistry::validate`](crate::JobRegistry::validate)
/// checks job timeouts against.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_jobs::worker::DEFAULT_LEASE, Duration::from_secs(60));
/// ```
pub const DEFAULT_LEASE: Duration = Duration::from_secs(60);

/// How long a worker waits for in-flight jobs at shutdown, by default.
///
/// Under the 30 seconds an orchestrator typically allows before `SIGKILL`, on
/// purpose: a grace longer than the kill timeout means the process is killed
/// mid-drain, which is the thing the grace existed to prevent.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_jobs::worker::DEFAULT_GRACE, Duration::from_secs(25));
/// ```
pub const DEFAULT_GRACE: Duration = Duration::from_secs(25);

/// How long a worker waits for work before asking again.
///
/// Only reached on a backend with no push notification; the Postgres backend
/// listens on a channel and Redis blocks, so this is the ceiling on latency,
/// not the typical value.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_jobs::worker::DEFAULT_POLL, Duration::from_secs(1));
/// ```
pub const DEFAULT_POLL: Duration = Duration::from_secs(1);

/// Which worker process is which.
///
/// Hostname plus a per-process suffix, so a reclaimed lease and a running job in
/// the dashboard both name a pod an operator can find.
///
/// ```
/// use moso_jobs::WorkerId;
///
/// let id = WorkerId::local();
/// assert!(!id.as_str().is_empty());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkerId(String);

impl WorkerId {
    /// An identifier for this process.
    ///
    /// `HOSTNAME` first, because that is the pod name in Kubernetes and the pod
    /// name is what an operator types into `kubectl logs`. Then `/etc/hostname`
    /// for a plain host, then a bare suffix — a worker with no name is worse
    /// than one with an ugly name.
    ///
    /// ```
    /// use moso_jobs::WorkerId;
    ///
    /// let id = WorkerId::local();
    /// assert!(id.as_str().contains('-'));
    /// ```
    #[must_use]
    pub fn local() -> Self {
        let host = std::env::var("HOSTNAME")
            .ok()
            .filter(|name| !name.trim().is_empty())
            .or_else(|| {
                std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|name| name.trim().to_owned())
                    .filter(|name| !name.is_empty())
            })
            .unwrap_or_else(|| "worker".to_owned());

        // The suffix is not decoration: two pods rolled from the same
        // StatefulSet can share a hostname across a restart, and a reclaimed
        // lease has to name the process that lost it.
        Self(format!("{}-{}", sanitise(&host), crate::rng::hex_suffix()))
    }

    /// Wrap an existing identifier, when reading a row back.
    ///
    /// ```
    /// use moso_jobs::WorkerId;
    ///
    /// assert_eq!(WorkerId::new("pod-7-a1b2").as_str(), "pod-7-a1b2");
    /// ```
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The identifier.
    ///
    /// ```no_run
    /// # use moso_jobs::WorkerId;
    /// # fn f(w: &WorkerId) { let _: &str = w.as_str(); }
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Keep a hostname to what fits in a label and a log line.
fn sanitise(host: &str) -> String {
    host.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '.' || *c == '_')
        .take(48)
        .collect()
}

impl core::fmt::Display for WorkerId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// How a queue is weighted against the others on the same worker.
///
/// ```
/// use moso_jobs::QueueWeight;
///
/// let w = QueueWeight::new("mail", 3);
/// assert_eq!(w.weight(), 3);
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueWeight {
    /// Which queue.
    queue: String,
    /// Its share of each polling round. Higher gets pulled from more often.
    weight: u32,
}

impl QueueWeight {
    /// Weight `queue` at `weight`.
    ///
    /// A weight of zero is raised to one: a queue a worker was asked to listen
    /// to and then never pulls from is a queue that silently never drains.
    ///
    /// ```
    /// use moso_jobs::QueueWeight;
    ///
    /// assert_eq!(QueueWeight::new("default", 0).weight(), 1);
    /// ```
    #[must_use]
    pub fn new(queue: impl Into<String>, weight: u32) -> Self {
        Self {
            queue: queue.into(),
            weight: weight.max(1),
        }
    }

    /// Which queue.
    ///
    /// ```
    /// # use moso_jobs::QueueWeight;
    /// assert_eq!(QueueWeight::new("a", 1).queue(), "a");
    /// ```
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// Its share.
    ///
    /// ```
    /// # use moso_jobs::QueueWeight;
    /// assert_eq!(QueueWeight::new("a", 4).weight(), 4);
    /// ```
    #[must_use]
    pub const fn weight(&self) -> u32 {
        self.weight
    }
}

/// What is left when the grace period expires.
///
/// ```
/// use moso_jobs::DrainMode;
///
/// assert_eq!(DrainMode::default(), DrainMode::Requeue);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DrainMode {
    /// Put unfinished jobs back on the queue. At-least-once, and the docs say
    /// so plainly: a job interrupted this way runs again from the start.
    #[default]
    Requeue,
    /// Let the lease expire instead, so another worker reclaims it. Slower to
    /// recover, but it does not reset the attempt counter.
    Abandon,
}

/// Runs jobs until told to stop.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_jobs::{JobRegistry, Jobs, Worker};
///
/// fn build(jobs: Jobs, registry: Arc<JobRegistry>) -> Worker {
///     Worker::new(jobs, registry).concurrency(8).queues(["default", "mail"])
/// }
/// ```
///
/// # What it guarantees, and what it does not
///
/// - **At-least-once.** A worker that dies mid-job has that job reclaimed after
///   the lease expires and run again. Jobs must be idempotent.
/// - **No starvation.** Queues are polled by weight, so a saturated `mail` queue
///   still leaves room for `default`.
/// - **Bounded shutdown.** Stop fetching, finish in flight up to the grace
///   period, then apply [`DrainMode`].
/// - **No ordering** across jobs, with one opt-in exception:
///   [`Job::SERIAL`](crate::Job::SERIAL) means a job type never has two
///   instances running at once, anywhere in the fleet.
/// - **A registry it can honour.** [`validate`](Worker::validate) runs before
///   anything else and refuses to start on a problem, so a duplicate wire name
///   or a serial job on a backend that cannot serialise it is a boot error and
///   not a production surprise.
pub struct Worker {
    /// Where jobs come from.
    jobs: Jobs,
    /// What can run.
    registry: std::sync::Arc<JobRegistry>,
    /// This process's identifier.
    id: WorkerId,
    /// How many jobs at once.
    concurrency: usize,
    /// Which queues, and how they are weighted.
    queues: Vec<QueueWeight>,
    /// How long a lease is taken for.
    lease: Duration,
    /// How long to wait for in-flight jobs at shutdown.
    grace: Duration,
    /// What to do with what is left.
    drain: DrainMode,
    /// How often to reclaim expired leases.
    reclaim_interval: Duration,
    /// Pause pulling low-priority jobs above this queue depth.
    backpressure: Option<u64>,
    /// How long to wait for work before asking again.
    poll: Duration,
    /// Which polling round this is, so the queue order rotates.
    ///
    /// On the worker rather than in `run`'s stack, so that `run_once` — which a
    /// drain and a test call in a loop — rotates too. A `run_once` that always
    /// started at the first queue would let a saturated one starve the rest of
    /// them in exactly the code path a test exercises.
    round: AtomicUsize,
}

impl Worker {
    /// A worker over `registry`, pulling from `jobs`.
    ///
    /// Defaults: concurrency equal to the number of cores, every queue the
    /// registry knows, a 60-second lease, a 25-second grace, and
    /// [`DrainMode::Requeue`].
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobRegistry, Jobs, Worker};
    /// # fn f(j: Jobs, r: Arc<JobRegistry>) { let _ = Worker::new(j, r); }
    /// ```
    #[must_use]
    pub fn new(jobs: Jobs, registry: std::sync::Arc<JobRegistry>) -> Self {
        let queues = registry
            .queues()
            .into_iter()
            .map(|queue| QueueWeight::new(queue, 1))
            .collect();
        Self {
            jobs,
            registry,
            id: WorkerId::local(),
            concurrency: std::thread::available_parallelism()
                .map_or(4, std::num::NonZeroUsize::get),
            queues,
            lease: DEFAULT_LEASE,
            grace: DEFAULT_GRACE,
            drain: DrainMode::Requeue,
            reclaim_interval: Duration::from_secs(30),
            backpressure: None,
            poll: DEFAULT_POLL,
            round: AtomicUsize::new(0),
        }
    }

    /// The next polling round's index.
    fn next_round(&self) -> usize {
        self.round.fetch_add(1, Ordering::Relaxed)
    }

    /// How many jobs to run at once.
    ///
    /// Zero is raised to one: a worker that runs nothing is a worker that looks
    /// like a queue that is not draining.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.concurrency(32); }
    /// ```
    #[must_use]
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = concurrency.max(1);
        self
    }

    /// Listen to these queues, all weighted equally.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.queues(["default", "mail"]); }
    /// ```
    #[must_use]
    pub fn queues<S: Into<String>>(self, queues: impl IntoIterator<Item = S>) -> Self {
        self.weighted_queues(queues.into_iter().map(|queue| QueueWeight::new(queue, 1)))
    }

    /// Listen to these queues with explicit weights.
    ///
    /// ```no_run
    /// # use moso_jobs::{QueueWeight, Worker};
    /// # fn f(w: Worker) {
    /// let _ = w.weighted_queues([QueueWeight::new("default", 3), QueueWeight::new("bulk", 1)]);
    /// # }
    /// ```
    #[must_use]
    pub fn weighted_queues(mut self, queues: impl IntoIterator<Item = QueueWeight>) -> Self {
        let queues: Vec<QueueWeight> = queues.into_iter().collect();
        if !queues.is_empty() {
            self.queues = queues;
        }
        self
    }

    /// How long a lease is taken for. Default 60 seconds.
    ///
    /// **It does not have to exceed a job's timeout.** The worker renews the
    /// lease automatically at a third of its length for as long as the job
    /// runs, which is what makes a five-minute default timeout safe under a
    /// sixty-second default lease. Two consecutive heartbeats can be lost
    /// before another worker can take the job.
    ///
    /// What the automatic renewal cannot survive is a job body that **blocks
    /// the runtime** — synchronous work with no `.await` — because the
    /// heartbeat task then never gets scheduled. `moso_core::task::blocking` is
    /// the answer to that, and it is the same answer a handler gets.
    ///
    /// The lease *is* the upper bound on how long a dead worker's jobs sit
    /// unclaimed, so lowering it makes recovery faster and heartbeats more
    /// frequent.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.lease(std::time::Duration::from_secs(120)); }
    /// ```
    #[must_use]
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease.max(Duration::from_secs(1));
        self
    }

    /// How long to wait for in-flight jobs at shutdown. Default 25 seconds.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.grace(std::time::Duration::from_secs(60)); }
    /// ```
    #[must_use]
    pub fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// How long to wait for work before asking again. Default one second.
    ///
    /// Only reached on a backend with no push notification.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.poll(std::time::Duration::from_millis(200)); }
    /// ```
    #[must_use]
    pub fn poll(mut self, poll: Duration) -> Self {
        self.poll = poll;
        self
    }

    /// How often expired leases are reclaimed. Default 30 seconds.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.reclaim_interval(std::time::Duration::from_secs(5)); }
    /// ```
    #[must_use]
    pub fn reclaim_interval(mut self, interval: Duration) -> Self {
        self.reclaim_interval = interval;
        self
    }

    /// What to do with jobs still running when the grace expires.
    ///
    /// ```no_run
    /// # use moso_jobs::{DrainMode, Worker};
    /// # fn f(w: Worker) { let _ = w.drain_mode(DrainMode::Abandon); }
    /// ```
    #[must_use]
    pub fn drain_mode(mut self, mode: DrainMode) -> Self {
        self.drain = mode;
        self
    }

    /// Publish backpressure when a queue is deeper than this.
    ///
    /// The worker samples depth on every reclaim tick. Above the threshold it
    /// logs at `WARN`, sets `moso_jobs_backpressure_active{queue}` and marks the
    /// queue on the shared [`Jobs`] handle, which is what makes
    /// [`EnqueueBuilder::spawn`](crate::EnqueueBuilder::spawn) refuse further
    /// [`Priority::Low`](crate::Priority::Low) work for that queue.
    ///
    /// The gate is on the *enqueue* rather than on the pull, because a worker
    /// that stops pulling a deep queue makes it deeper. It is exposed as a
    /// metric because a worker that has quietly stopped taking bulk work looks
    /// identical to one that has nothing to do.
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # fn f(w: Worker) { let _ = w.backpressure(Some(50_000)); }
    /// ```
    #[must_use]
    pub fn backpressure(mut self, threshold: Option<u64>) -> Self {
        self.backpressure = threshold;
        self
    }

    /// This worker's identifier.
    ///
    /// ```no_run
    /// # use moso_jobs::{Worker, WorkerId};
    /// # fn f(w: &Worker) { let _: &WorkerId = w.id(); }
    /// ```
    #[must_use]
    pub fn id(&self) -> &WorkerId {
        &self.id
    }

    /// Override this worker's identifier.
    ///
    /// For a deployment whose pod name is not its hostname, and for a test that
    /// wants two workers with names it chose.
    ///
    /// ```no_run
    /// # use moso_jobs::{Worker, WorkerId};
    /// # fn f(w: Worker) { let _ = w.with_id(WorkerId::new("pod-7")); }
    /// ```
    #[must_use]
    pub fn with_id(mut self, id: WorkerId) -> Self {
        self.id = id;
        self
    }

    /// Every queue this worker listens to, in declaration order.
    ///
    /// ```no_run
    /// # use moso_jobs::{QueueWeight, Worker};
    /// # fn f(w: &Worker) { let _: &[QueueWeight] = w.weights(); }
    /// ```
    #[must_use]
    pub fn weights(&self) -> &[QueueWeight] {
        &self.queues
    }

    /// Everything wrong with this worker, as boot problems.
    ///
    /// [`JobRegistry::validate`](crate::JobRegistry::validate) is the bulk of
    /// it, plus the two questions only a worker can ask: whether the backend can
    /// keep the promises the registry makes, and whether this worker listens to
    /// a queue no registered job uses.
    ///
    /// [`run`](Worker::run) and [`drain_inline`](Worker::drain_inline) call it
    /// and refuse to start on a non-empty report, so a misconfigured registry
    /// cannot reach production through a worker binary. It is public because a
    /// composition root should also call it — `moso-core` does not depend on
    /// this crate, so `App::build()` cannot see a registry, and an application
    /// that enqueues without running a worker would otherwise never check.
    ///
    /// Every problem in one pass, each with its own `fix` line, exactly as
    /// `AppBuilder::build()` reports its own.
    ///
    /// ```
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobRegistry, Jobs, Worker, backend::MemoryQueue};
    /// let jobs = Jobs::new(Arc::new(MemoryQueue::new()), Arc::new(JobRegistry::new()));
    /// let registry = jobs.shared_registry();
    /// assert!(Worker::new(jobs, registry).validate().is_empty());
    /// ```
    #[must_use]
    pub fn validate(&self) -> moso_core::error::BootErrors {
        let mut errors = self.registry.validate();

        if !self.jobs.queue().capabilities().serial_chains {
            for job in self.registry.all().filter(|job| job.serial()) {
                errors.push(moso_core::error::BootError::Other {
                    message: format!(
                        "job `{}` is serial and the `{}` queue cannot serialise it",
                        job.name(),
                        self.jobs.queue().name()
                    ),
                    notes: vec![
                        format!("job          {}", job.type_name()),
                        "`SERIAL` promises that two instances never run at once, and this \
                         backend answers `QueueCapabilities::serial_chains = false` — so the \
                         promise would be a lie rather than a limitation"
                            .to_owned(),
                    ],
                    fix: Some(
                        "run this job on a backend that serialises — `PgQueue`, `RedisQueue` \
                         or `MemoryQueue` all do — or drop `serial` from the `#[job(..)]` \
                         attribute"
                            .to_owned(),
                    ),
                });
            }
        }

        let known = self.registry.queues();
        for weighted in &self.queues {
            if !known.iter().any(|queue| queue == weighted.queue()) {
                errors.push(moso_core::error::BootError::Other {
                    // Headlines are capped at 72 characters by the boot
                    // renderer, so the queue name goes early rather than in a
                    // clause that would be elided away.
                    message: format!("no registered job runs on queue `{}`", weighted.queue()),
                    notes: vec![
                        format!("registered   {known:?}"),
                        "a worker listening to a queue nothing writes to looks exactly like a \
                         worker that is broken"
                            .to_owned(),
                    ],
                    fix: Some(format!(
                        "drop `{}` from `Worker::queues(..)`, or register the job that was \
                         meant to run on it",
                        weighted.queue()
                    )),
                });
            }
        }

        errors
    }

    /// Refuse to start on a registry the worker cannot honour.
    fn checked(&self) -> Result {
        let errors = self.validate();
        if errors.is_empty() {
            return Ok(());
        }
        // One `Error::Config` carrying the whole grouped report, rather than the
        // first problem: an operator who has three of these wants three, not
        // three restarts.
        Err(crate::Error::config(format!(
            "this worker will not start until the job registry is sound\n{report}",
            report = errors.render(false),
        )))
    }

    /// Run until the shutdown signal fires and the grace period elapses.
    ///
    /// Each job gets a tracing span linked to the enqueueing request's trace, so
    /// one distributed trace spans `HTTP request → job → outbound call`. That is
    /// what the `trace_parent` on the queue row is for.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the queue cannot
    /// be reached at startup. Failures during the loop are logged and retried:
    /// a worker that exits on a transient error is a worker that stops working.
    ///
    /// ```no_run
    /// # use moso_core::shutdown::Signal;
    /// # use moso_jobs::Worker;
    /// # async fn f(w: Worker, s: Signal) -> moso_jobs::Result { w.run(s).await }
    /// ```
    pub async fn run(self, shutdown: moso_core::shutdown::Signal) -> Result {
        // Before the socket, because a registry problem is not going to fix
        // itself and every one of them is knowable without touching anything.
        self.checked()?;
        // Fail at startup rather than logging a connection error every second
        // for the lifetime of a pod that will never do any work.
        self.jobs.queue().probe().await?;

        let queue_names: Vec<String> = self
            .queues
            .iter()
            .map(|weighted| weighted.queue().to_owned())
            .collect();

        tracing::info!(
            target: "moso::jobs",
            worker = %self.id,
            backend = self.jobs.queue().name(),
            concurrency = self.concurrency,
            queues = ?queue_names,
            "worker started"
        );

        let running = Arc::new(tokio::sync::Semaphore::new(self.concurrency));
        let inflight = Arc::new(std::sync::Mutex::new(std::collections::BTreeMap::<
            crate::JobId,
            Arc<Cancellation>,
        >::new()));
        let mut reclaim_at = tokio::time::Instant::now();

        while !shutdown.is_shutting_down() {
            if tokio::time::Instant::now() >= reclaim_at {
                self.maintenance(&queue_names).await;
                reclaim_at = tokio::time::Instant::now() + self.reclaim_interval;
            }

            let free = running.available_permits();
            if free == 0 {
                // Every slot is busy. Waiting on a permit rather than sleeping
                // means the next job starts the instant one finishes.
                tokio::select! {
                    _ = running.acquire() => {}
                    () = shutdown.recv() => break,
                }
                continue;
            }

            let batch = self.pull_round(free, self.next_round(), &queue_names).await;

            if batch.is_empty() {
                tokio::select! {
                    () = self.jobs.queue().wait_for_work(&queue_names, self.poll) => {}
                    () = shutdown.recv() => break,
                }
                continue;
            }

            for (row, lease) in batch {
                let permit = Arc::clone(&running)
                    .acquire_owned()
                    .await
                    .expect("the semaphore is never closed");
                let cancel = Arc::new(Cancellation::default());
                inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .insert(row.id, Arc::clone(&cancel));

                let jobs = self.jobs.clone();
                let registry = Arc::clone(&self.registry);
                let id = self.id.clone();
                let lease_duration = self.lease;
                let inflight_handle = Arc::clone(&inflight);
                let job_id = row.id;

                tokio::spawn(async move {
                    execute(&jobs, &registry, &id, row, lease, lease_duration, cancel).await;
                    inflight_handle
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .remove(&job_id);
                    drop(permit);
                });
            }
        }

        self.shut_down(&running, &inflight).await;
        Ok(())
    }

    /// Stop fetching, finish in flight up to the grace, then apply the drain
    /// mode to what is left.
    async fn shut_down(
        &self,
        running: &Arc<tokio::sync::Semaphore>,
        inflight: &Arc<
            std::sync::Mutex<std::collections::BTreeMap<crate::JobId, Arc<Cancellation>>>,
        >,
    ) {
        let outstanding = self.concurrency - running.available_permits();
        if outstanding == 0 {
            tracing::info!(target: "moso::jobs", worker = %self.id, "worker stopped, nothing in flight");
            return;
        }

        tracing::info!(
            target: "moso::jobs",
            worker = %self.id,
            outstanding,
            grace = %humantime::format_duration(self.grace),
            "worker draining"
        );

        // The permits come back one per finished job, so acquiring all of them
        // is exactly "every job finished" — with no polling and no sleeping.
        let drained = tokio::time::timeout(
            self.grace,
            running.acquire_many(u32::try_from(self.concurrency).unwrap_or(u32::MAX)),
        )
        .await;

        if drained.is_ok() {
            tracing::info!(target: "moso::jobs", worker = %self.id, "worker drained cleanly");
            return;
        }

        // The grace expired. Tell the jobs to stop; what happens next is the
        // drain mode's decision, and both options are at-least-once.
        let still_running: Vec<Arc<Cancellation>> = inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(Arc::clone)
            .collect();
        for cancel in &still_running {
            match self.drain {
                // Recorded on the cancellation rather than counted here, so the
                // one retry each of these produces is labelled `requeued`
                // exactly once — by the same path that labels every other one.
                DrainMode::Requeue => cancel.cancel_for_drain(),
                DrainMode::Abandon => cancel.cancel(),
            }
        }

        match self.drain {
            DrainMode::Requeue => tracing::warn!(
                target: "moso::jobs",
                worker = %self.id,
                outstanding = still_running.len(),
                "the grace expired; cancelling and re-queueing — these jobs will run again \
                 from the start"
            ),
            DrainMode::Abandon => tracing::warn!(
                target: "moso::jobs",
                worker = %self.id,
                outstanding = still_running.len(),
                lease = %humantime::format_duration(self.lease),
                "the grace expired; abandoning the leases — another worker picks these up \
                 when they expire"
            ),
        }

        if self.drain == DrainMode::Requeue {
            // Give the cancelled jobs a moment to unwind and nack themselves;
            // whatever is still going loses its lease, which is the same
            // outcome as `Abandon`.
            let _ = tokio::time::timeout(
                Duration::from_secs(2),
                running.acquire_many(u32::try_from(self.concurrency).unwrap_or(u32::MAX)),
            )
            .await;
        }
    }

    /// Reclaim expired leases and sample queue depth.
    async fn maintenance(&self, queues: &[String]) {
        match self.jobs.queue().reclaim(queues).await {
            Ok(0) => {}
            Ok(count) => {
                tracing::warn!(
                    target: "moso::jobs",
                    worker = %self.id,
                    count,
                    "reclaimed jobs whose worker stopped heartbeating"
                );
                for _ in 0..count {
                    crate::metrics::retried("unknown", crate::metrics::RetryReason::Reclaimed);
                }
            }
            Err(error) => tracing::warn!(
                target: "moso::jobs",
                worker = %self.id,
                error = %error.chain(),
                "could not reclaim expired leases; will try again"
            ),
        }

        match self.jobs.queue().stats(queues).await {
            Ok(stats) => {
                for one in &stats {
                    crate::metrics::depth(&one.queue, one.ready);
                    if let Some(threshold) = self.backpressure {
                        let active = one.ready > threshold;
                        crate::metrics::backpressure(&one.queue, active);
                        if self.jobs.set_backpressure(&one.queue, active) && active {
                            tracing::warn!(
                                target: "moso::jobs",
                                queue = %one.queue,
                                depth = one.ready,
                                threshold,
                                "queue is over its backpressure threshold; low-priority \
                                 enqueues are being refused"
                            );
                        }
                    }
                }
            }
            Err(error) => tracing::warn!(
                target: "moso::jobs",
                worker = %self.id,
                error = %error.chain(),
                "could not read queue depth"
            ),
        }
    }

    /// Pull one round's worth of work, one statement per queue.
    ///
    /// Each queue gets its weighted share of the free slots, and the starting
    /// offset rotates every round so the queue that lost the rounding last time
    /// wins it this time. That is what makes "a slow queue cannot starve a fast
    /// one" a property of the schedule rather than a hope about arrival rates.
    async fn pull_round(
        &self,
        free: usize,
        round: usize,
        queue_names: &[String],
    ) -> Vec<(crate::QueuedJob, crate::Lease)> {
        let mut batch = Vec::new();
        let mut remaining = free;

        for share in self.shares(free, round) {
            if remaining == 0 {
                break;
            }
            let limit = u32::try_from(share.limit.min(remaining)).unwrap_or(u32::MAX);
            if limit == 0 {
                continue;
            }
            let names = [share.queue.clone()];
            match self
                .jobs
                .queue()
                .pull(&names, limit, self.lease, self.id.clone())
                .await
            {
                Ok(rows) => {
                    remaining = remaining.saturating_sub(rows.len());
                    batch.extend(rows);
                }
                Err(error) => {
                    tracing::warn!(
                        target: "moso::jobs",
                        worker = %self.id,
                        queue = %share.queue,
                        error = %error.chain(),
                        "could not pull; will try again"
                    );
                    // One queue being unreachable must not stop the others.
                    let _ = queue_names;
                }
            }
        }
        batch
    }

    /// How many jobs each queue may take this round.
    fn shares(&self, free: usize, round: usize) -> Vec<Share> {
        let total: u32 = self.queues.iter().map(QueueWeight::weight).sum();
        let count = self.queues.len();
        (0..count)
            .map(|offset| {
                // Rotating the starting index is the whole anti-starvation
                // mechanism: with three queues and one free slot, each of them
                // is first once every three rounds.
                let weighted = &self.queues[(round + offset) % count];
                let exact = free * weighted.weight() as usize;
                Share {
                    queue: weighted.queue().to_owned(),
                    // At least one, so a queue with a small weight still gets
                    // pulled rather than being rounded out of existence.
                    limit: (exact / total.max(1) as usize).max(1),
                }
            })
            .collect()
    }

    /// Run exactly one batch and return how many jobs ran.
    ///
    /// What `drain()` and the tests are built on: no loop, no sleeping, fully
    /// deterministic.
    ///
    /// # Errors
    ///
    /// As [`run`](Worker::run).
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # async fn f(w: &Worker) -> moso_jobs::Result<u64> { w.run_once().await }
    /// ```
    pub async fn run_once(&self) -> Result<u64> {
        let queue_names: Vec<String> = self
            .queues
            .iter()
            .map(|weighted| weighted.queue().to_owned())
            .collect();
        let batch = self
            .pull_round(self.concurrency, self.next_round(), &queue_names)
            .await;
        let ran = batch.len() as u64;

        let mut running = Vec::with_capacity(batch.len());
        for (row, lease) in batch {
            let jobs = self.jobs.clone();
            let registry = Arc::clone(&self.registry);
            let id = self.id.clone();
            let lease_duration = self.lease;
            running.push(async move {
                execute(
                    &jobs,
                    &registry,
                    &id,
                    row,
                    lease,
                    lease_duration,
                    Arc::new(Cancellation::default()),
                )
                .await;
            });
        }
        futures_util::future::join_all(running).await;
        Ok(ran)
    }

    /// Run everything that is ready, inline, until nothing is.
    ///
    /// What [`Jobs::drain`] is. Bounded by a pass count rather than by "until
    /// empty", because a job that enqueues itself would otherwise never let the
    /// test finish.
    ///
    /// # Errors
    ///
    /// As [`run`](Worker::run).
    ///
    /// ```no_run
    /// # use moso_jobs::Worker;
    /// # async fn f(w: &Worker) -> moso_jobs::Result<u64> { w.drain_inline().await }
    /// ```
    pub async fn drain_inline(&self) -> Result<u64> {
        /// A job that enqueues a job that enqueues a job is a real pattern and
        /// an infinite one is a real bug; a test must fail, not hang.
        const MAX_PASSES: usize = 1_000;

        // A drain is what a test suite runs, and a registry problem found by
        // the test suite is one that never reaches a worker binary.
        self.checked()?;

        let mut total = 0;
        for pass in 0..MAX_PASSES {
            let ran = self.run_once().await?;
            total += ran;
            if ran == 0 {
                return Ok(total);
            }
            if pass + 1 == MAX_PASSES {
                return Err(crate::Error::config(format!(
                    "`drain()` ran {MAX_PASSES} passes and the queue is still not empty\n\
                     help: a job that enqueues itself never lets a drain finish; enqueue the \
                     follow-up outside the drain, or assert on `assert_enqueued` instead"
                )));
            }
        }
        Ok(total)
    }
}

/// One queue's allowance for one round.
struct Share {
    /// Which queue.
    queue: String,
    /// How many jobs it may take.
    limit: usize,
}

/// Run one job: lease, heartbeat, execute under its timeout, acknowledge.
///
/// A free function rather than a method so the spawned task owns everything it
/// needs and the worker is not borrowed across the `await`.
#[allow(
    clippy::too_many_arguments,
    reason = "\
    a spawned task owns each of these outright; bundling them into a struct \
    would move the same seven fields one indirection away"
)]
async fn execute(
    jobs: &Jobs,
    registry: &JobRegistry,
    worker: &WorkerId,
    row: crate::QueuedJob,
    lease: crate::Lease,
    lease_duration: Duration,
    cancel: Arc<Cancellation>,
) {
    let started = std::time::Instant::now();
    let queue = row.queue.clone();
    let name = row.name.clone();

    crate::metrics::started(
        &queue,
        (chrono::Utc::now() - row.enqueued_at)
            .to_std()
            .unwrap_or_default(),
    );
    crate::metrics::running_started();

    let Some(registered) = registry.get(&name) else {
        // A rolling deploy can hand this worker a job the *next* version knows
        // and this one does not. Straight to the dead-letter queue with the
        // payload intact, so it can be retried after the deploy finishes.
        let error = crate::Error::Unregistered {
            name: name.clone(),
            suggestion: registry.suggest(&name),
            site: None,
        };
        tracing::error!(
            target: "moso::jobs",
            job = %name,
            id = %row.id,
            "a job arrived that this build cannot run; dead-lettering it with the payload intact"
        );
        finish(jobs, lease, &row, &error, None).await;
        crate::metrics::running_finished();
        crate::metrics::dead_lettered(&name);
        return;
    };

    let context = crate::trace::context_for_job(row.trace_parent.as_deref());
    let span = tracing::info_span!(
        target: "moso::jobs",
        "job",
        job = %name,
        queue = %queue,
        id = %row.id,
        attempt = row.attempt,
        worker = %worker,
        trace_id = %context.trace_id(),
        span_id = %context.span_id_hex(),
        parent_span_id = context.parent_span_id_hex().unwrap_or_default(),
    );

    let shared_lease = Arc::new(crate::Lease::new(
        lease.job_id(),
        lease.token(),
        lease.expires_at(),
    ));
    let ctx = crate::JobCtx::new(
        row.id,
        registered.name(),
        queue.clone(),
        row.attempt,
        row.retry.max_attempts(),
        row.enqueued_at,
        worker.clone(),
        jobs.resolver().cloned().unwrap_or_else(detached_resolver),
        Arc::new(jobs.clone()),
        row.trace_parent.clone(),
        row.actor.clone(),
        Arc::clone(&cancel),
        Some(Arc::clone(&shared_lease)),
        lease_duration,
    );

    // The lease is renewed at a third of its length, so two heartbeats can be
    // lost before another worker can steal the job.
    // The lease is renewed at a third of its length, so two heartbeats can be
    // lost before another worker can steal the job.
    //
    // A *failed* heartbeat is also how an operator's cancel reaches a running
    // job: `Queue::cancel` drops the lock token, so the next renewal is refused
    // and this task fires the job's cancellation. One mechanism covers both
    // "somebody pressed cancel" and "this worker lost the race", which are the
    // same thing from the job's point of view — stop, do not acknowledge.
    let heartbeat = tokio::spawn({
        let jobs = jobs.clone();
        let lease = Arc::clone(&shared_lease);
        let cancel = Arc::clone(&cancel);
        let every = lease_duration / 3;
        async move {
            let mut ticker = tokio::time::interval(every.max(Duration::from_millis(20)));
            ticker.tick().await;
            loop {
                ticker.tick().await;
                if let Err(error) = jobs.queue().heartbeat(&lease, lease_duration).await {
                    tracing::info!(
                        target: "moso::jobs",
                        error = %error.chain(),
                        "the lease could not be renewed; stopping the job"
                    );
                    cancel.cancel();
                    return;
                }
            }
        }
    });

    let timeout = registered.timeout();
    let payload = row.payload.clone();
    // The body runs inside both scopes: the trace context so its span is a child
    // of the request's, and the enqueueing actor's identity so a further enqueue
    // from within the job is attributed to the same subject.
    let outcome = crate::actor::scope_for_job(
        row.actor.clone(),
        crate::trace::scope(context, async {
            tokio::select! {
                biased;
                // Cancellation wins the race deliberately: a job that has been
                // told to stop and finishes anyway would be acknowledged, and
                // the operator who pressed cancel would see it succeed.
                () = cancel.cancelled() => Err(crate::Error::retry(
                    "the job was cancelled; it will be retried"
                )),
                result = tokio::time::timeout(timeout, registered.run(payload, ctx)) => match result {
                    Ok(result) => result,
                    Err(_elapsed) => Err(crate::Error::Timeout {
                        job: registered.name(),
                        timeout,
                    }),
                },
            }
        }),
    )
    .instrument_with(span)
    .await;

    heartbeat.abort();
    crate::metrics::running_finished();

    let elapsed = started.elapsed();
    match &outcome {
        Ok(()) => {
            crate::metrics::finished(&name, crate::metrics::Outcome::Success, elapsed);
            let lease = crate::Lease::new(
                shared_lease.job_id(),
                shared_lease.token(),
                shared_lease.expires_at(),
            );
            if let Err(error) = jobs.queue().ack(lease).await {
                // The work is done; the acknowledgement is not. The job will be
                // reclaimed and run again, which is exactly why delivery is
                // at-least-once and jobs must be idempotent.
                tracing::error!(
                    target: "moso::jobs",
                    job = %name,
                    id = %row.id,
                    error = %error.chain(),
                    "the job finished but could not be acknowledged; it will run again"
                );
            }
        }
        Err(error) => {
            let retry_at = if error.skips_retries() || !error.retryable() {
                None
            } else {
                row.retry.next_delay(row.attempt).map(|delay| {
                    chrono::Utc::now() + chrono::Duration::from_std(delay).unwrap_or_default()
                })
            };

            if retry_at.is_some() {
                crate::metrics::finished(&name, crate::metrics::Outcome::Retry, elapsed);
                let reason = if cancel.is_draining() {
                    crate::metrics::RetryReason::Requeued
                } else {
                    crate::metrics::RetryReason::of(error)
                };
                crate::metrics::retried(&name, reason);
                tracing::warn!(
                    target: "moso::jobs",
                    job = %name,
                    id = %row.id,
                    attempt = row.attempt,
                    error = %error.chain(),
                    "the job failed and will be retried"
                );
            } else {
                crate::metrics::finished(&name, crate::metrics::Outcome::Dead, elapsed);
                crate::metrics::dead_lettered(&name);
                tracing::error!(
                    target: "moso::jobs",
                    job = %name,
                    id = %row.id,
                    attempt = row.attempt,
                    error = %error.chain(),
                    skipped_retries = error.skips_retries(),
                    "the job gave up; it is in the dead-letter queue with its payload"
                );
            }

            let lease = crate::Lease::new(
                shared_lease.job_id(),
                shared_lease.token(),
                shared_lease.expires_at(),
            );
            finish(jobs, lease, &row, error, retry_at).await;
        }
    }
}

/// Record a failed attempt, and say so if even that failed.
async fn finish(
    jobs: &Jobs,
    lease: crate::Lease,
    row: &crate::QueuedJob,
    error: &crate::Error,
    retry_at: Option<chrono::DateTime<chrono::Utc>>,
) {
    if let Err(failure) = jobs.queue().nack(lease, &error.chain(), retry_at).await {
        tracing::error!(
            target: "moso::jobs",
            job = %row.name,
            id = %row.id,
            error = %failure.chain(),
            "the failure could not be recorded; the lease will expire and the job will be \
             reclaimed"
        );
    }
}

/// A resolver over nothing, for a `Jobs` built outside an application.
///
/// A job that injects anything fails with the message
/// [`JobCtx::inject`](crate::JobCtx::inject) documents, which names the missing
/// provider — rather than panicking, which is what an `unwrap` here would do.
fn detached_resolver() -> moso_core::Resolver {
    moso_core::Resolver::new(Arc::new(moso_core::ProviderMap::default()))
}

/// `tracing`'s `Instrument`, spelled as a method on any future.
///
/// A local extension trait rather than `use tracing::Instrument`, so that the
/// one call site above reads as what it is and the import does not look like an
/// unused one to somebody skimming.
trait InstrumentWith: Sized {
    /// Run this future inside `span`.
    fn instrument_with(self, span: tracing::Span) -> tracing::instrument::Instrumented<Self>;
}

impl<F: Future> InstrumentWith for F {
    fn instrument_with(self, span: tracing::Span) -> tracing::instrument::Instrumented<Self> {
        tracing::Instrument::instrument(self, span)
    }
}

impl core::fmt::Debug for Worker {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Worker")
            .field("id", &self.id)
            .field("concurrency", &self.concurrency)
            .field("queues", &self.queues)
            .finish_non_exhaustive()
    }
}

/// Whether a queue is currently refusing low-priority work.
///
/// Shared between the worker that samples the depth and the [`Jobs`] handle
/// that enforces it, so that in a single-process deployment the enqueue side
/// sees what the worker measured.
#[derive(Debug, Default)]
pub(crate) struct Backpressure {
    /// Whether anything at all is over its threshold — read on every enqueue,
    /// so it must not take a lock in the common case.
    any: AtomicBool,
    /// Which queues are.
    queues: std::sync::RwLock<std::collections::BTreeSet<String>>,
}

impl Backpressure {
    /// Mark `queue`. Returns whether the state changed.
    pub(crate) fn set(&self, queue: &str, active: bool) -> bool {
        let mut guard = self
            .queues
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let changed = if active {
            guard.insert(queue.to_owned())
        } else {
            guard.remove(queue)
        };
        self.any.store(!guard.is_empty(), Ordering::Relaxed);
        changed
    }

    /// Whether `queue` is over its threshold.
    pub(crate) fn is_active(&self, queue: &str) -> bool {
        if !self.any.load(Ordering::Relaxed) {
            return false;
        }
        self.queues
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(queue)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A worker identifier has to name a pod an operator can find, and two
    /// processes on one host must not collide.
    #[test]
    fn a_local_identifier_names_the_host_and_the_process() {
        let first = WorkerId::local();
        let second = WorkerId::local();
        assert!(!first.as_str().is_empty());
        assert_ne!(first, second, "two processes on one host must differ");
        assert!(
            first
                .as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-._".contains(c)),
            "{first} is not safe as a metric label"
        );
    }

    /// A hostname with a slash in it would break a metric label and a log line.
    #[test]
    fn a_hostile_hostname_is_sanitised() {
        let cleaned = sanitise("pod/7 name\"with\nnonsense");
        assert!(!cleaned.contains('/'));
        assert!(!cleaned.contains(' '));
        assert!(!cleaned.contains('"'));
        assert!(cleaned.len() <= 48);
        assert_eq!(sanitise(&"a".repeat(200)).len(), 48);
    }

    /// A queue a worker was told to listen to and then never pulls from is a
    /// queue that silently never drains.
    #[test]
    fn a_zero_weight_is_raised_to_one() {
        assert_eq!(QueueWeight::new("bulk", 0).weight(), 1);
    }

    /// The anti-starvation claim, as arithmetic rather than as a hope about
    /// arrival rates: with three-to-one weighting, `default` gets three times
    /// the slots and `bulk` still gets some.
    #[test]
    fn weights_divide_the_slots_and_never_starve_a_queue() {
        let worker =
            worker_with_queues([QueueWeight::new("default", 3), QueueWeight::new("bulk", 1)]);

        let shares = worker.shares(8, 0);
        let by_queue: std::collections::BTreeMap<&str, usize> = shares
            .iter()
            .map(|share| (share.queue.as_str(), share.limit))
            .collect();
        assert_eq!(by_queue["default"], 6);
        assert_eq!(by_queue["bulk"], 2);

        // One free slot: the small queue is rounded to one rather than to zero.
        let shares = worker.shares(1, 0);
        assert!(shares.iter().all(|share| share.limit >= 1));
    }

    /// With one free slot and three queues, the rotation is what stops the
    /// first-declared queue from taking every round.
    #[test]
    fn the_pull_order_rotates_so_no_queue_is_always_last() {
        let worker = worker_with_queues([
            QueueWeight::new("a", 1),
            QueueWeight::new("b", 1),
            QueueWeight::new("c", 1),
        ]);

        let first: Vec<String> = (0..3)
            .map(|round| worker.shares(1, round)[0].queue.clone())
            .collect();
        let distinct: std::collections::BTreeSet<&String> = first.iter().collect();
        assert_eq!(
            distinct.len(),
            3,
            "each queue must lead one round in three, got {first:?}"
        );
    }

    /// `weighted_queues([])` must not silently make a worker that listens to
    /// nothing.
    #[test]
    fn an_empty_queue_list_is_ignored_rather_than_obeyed() {
        let worker = worker_with_queues([QueueWeight::new("mail", 1)]).weighted_queues([]);
        assert_eq!(worker.weights().len(), 1);
        assert_eq!(worker.weights()[0].queue(), "mail");
    }

    /// Zero concurrency is a worker that looks exactly like a queue that is not
    /// draining, which is the hardest kind of outage to diagnose.
    #[test]
    fn zero_concurrency_is_raised_to_one() {
        let worker = worker_with_queues([QueueWeight::new("mail", 1)]).concurrency(0);
        assert_eq!(worker.concurrency, 1);
    }

    /// Backpressure is read on every enqueue, so the common case must not take
    /// a lock, and the state has to be exact when it is set.
    #[test]
    fn backpressure_tracks_queues_individually() {
        let state = Backpressure::default();
        assert!(!state.is_active("bulk"));

        assert!(state.set("bulk", true));
        assert!(!state.set("bulk", true), "setting it twice is not a change");
        assert!(state.is_active("bulk"));
        assert!(!state.is_active("mail"));

        assert!(state.set("bulk", false));
        assert!(!state.is_active("bulk"));
    }

    /// A backend that answers every question and cannot serialise, which is
    /// what a hand-written `Queue` built from `QueueCapabilities::minimal()`
    /// looks like.
    struct Plain;

    impl crate::Queue for Plain {
        fn name(&self) -> &'static str {
            "plain"
        }
        fn capabilities(&self) -> crate::QueueCapabilities {
            crate::QueueCapabilities::minimal()
        }
        fn push<'a>(&'a self, _job: crate::QueuedJob) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn pull<'a>(
            &'a self,
            _queues: &'a [String],
            _limit: u32,
            _lease: Duration,
            _worker: WorkerId,
        ) -> moso_core::BoxFuture<'a, Result<Vec<(crate::QueuedJob, crate::Lease)>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn ack<'a>(&'a self, _lease: crate::Lease) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn nack<'a>(
            &'a self,
            _lease: crate::Lease,
            _error: &'a str,
            _run_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn heartbeat<'a>(
            &'a self,
            _lease: &'a crate::Lease,
            _extend: Duration,
        ) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn reclaim<'a>(&'a self, _queues: &'a [String]) -> moso_core::BoxFuture<'a, Result<u64>> {
            Box::pin(async { Ok(0) })
        }
        fn stats<'a>(
            &'a self,
            _queues: &'a [String],
        ) -> moso_core::BoxFuture<'a, Result<Vec<crate::QueueStats>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
    }

    /// A serial job on a backend that cannot serialise is a promise that would
    /// not hold, so it is a boot problem naming both halves rather than a
    /// silence somebody discovers in production.
    #[test]
    fn a_serial_job_on_a_backend_that_cannot_serialise_is_a_boot_problem() {
        struct OneAtATime;
        impl crate::Job for OneAtATime {
            type Args = ();
            const NAME: &'static str = "one_at_a_time";
            const SERIAL: bool = true;
            async fn run(_args: (), _ctx: crate::JobCtx) -> Result {
                Ok(())
            }
        }

        let jobs = Jobs::new(
            Arc::new(Plain),
            Arc::new(JobRegistry::new().register::<OneAtATime>()),
        );
        let registry = jobs.shared_registry();
        let rendered = Worker::new(jobs, registry).validate().render(false);
        assert!(rendered.contains("one_at_a_time"), "{rendered}");
        assert!(rendered.contains("cannot serialise it"), "{rendered}");
        assert!(rendered.contains("drop `serial`"), "{rendered}");

        // And the same job on a backend that can is not a problem at all.
        let jobs = Jobs::new(
            Arc::new(crate::backend::MemoryQueue::new()),
            Arc::new(JobRegistry::new().register::<OneAtATime>()),
        );
        let registry = jobs.shared_registry();
        assert!(Worker::new(jobs, registry).validate().is_empty());
    }

    fn worker_with_queues(queues: impl IntoIterator<Item = QueueWeight>) -> Worker {
        let jobs = Jobs::new(
            Arc::new(crate::backend::MemoryQueue::new()),
            Arc::new(JobRegistry::new()),
        );
        let registry = jobs.shared_registry();
        Worker::new(jobs, registry).weighted_queues(queues)
    }
}
