//! Backend choice, worker sizing, and the health probe — as configuration.

use std::time::Duration;

use moso_core::config::SecretString;

use crate::{DrainMode, Queue, Result};

/// Which backend a process uses.
///
/// ```
/// use moso_jobs::JobsBackendKind;
///
/// assert_eq!(JobsBackendKind::default(), JobsBackendKind::Postgres);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum JobsBackendKind {
    /// A table, with transactional enqueue. The default.
    #[default]
    Postgres,
    /// Redis, for throughput.
    Redis,
    /// In this process. Tests and `moso dev`.
    Memory,
}

impl JobsBackendKind {
    /// Parse the value of a `JOBS_BACKEND` variable.
    ///
    /// ```
    /// use moso_jobs::JobsBackendKind;
    ///
    /// assert_eq!(JobsBackendKind::parse("redis"), Some(JobsBackendKind::Redis));
    /// assert_eq!(JobsBackendKind::parse("  Postgres "), Some(JobsBackendKind::Postgres));
    /// assert_eq!(JobsBackendKind::parse("mysql"), None);
    /// ```
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            // `pg` and `postgresql` because a `DATABASE_URL` next to it spells
            // it both ways, and refusing one of them is a five-minute outage
            // for no reason.
            "postgres" | "postgresql" | "pg" => Some(Self::Postgres),
            "redis" => Some(Self::Redis),
            "memory" | "in-memory" | "inmemory" => Some(Self::Memory),
            _ => None,
        }
    }

    /// The cargo feature this backend needs.
    ///
    /// ```
    /// use moso_jobs::JobsBackendKind;
    ///
    /// assert_eq!(JobsBackendKind::Redis.feature(), "jobs-redis");
    /// ```
    #[must_use]
    pub const fn feature(self) -> &'static str {
        match self {
            Self::Postgres => "jobs-pg",
            Self::Redis => "jobs-redis",
            Self::Memory => "jobs-memory",
        }
    }

    /// Whether this build has the backend compiled in.
    ///
    /// ```
    /// use moso_jobs::JobsBackendKind;
    ///
    /// assert_eq!(
    ///     JobsBackendKind::Postgres.is_compiled_in(),
    ///     cfg!(feature = "jobs-pg"),
    /// );
    /// ```
    #[must_use]
    pub const fn is_compiled_in(self) -> bool {
        match self {
            Self::Postgres => cfg!(feature = "jobs-pg"),
            Self::Redis => cfg!(feature = "jobs-redis"),
            Self::Memory => cfg!(feature = "jobs-memory"),
        }
    }

    /// The name this parses from.
    ///
    /// ```
    /// use moso_jobs::JobsBackendKind;
    ///
    /// assert_eq!(JobsBackendKind::Postgres.as_str(), "postgres");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Postgres => "postgres",
            Self::Redis => "redis",
            Self::Memory => "memory",
        }
    }
}

/// Everything a process needs to enqueue and run jobs.
///
/// ```no_run
/// use moso_jobs::{JobsBackendKind, JobsConfig};
///
/// let config = JobsConfig::new(JobsBackendKind::Postgres).concurrency(16);
/// config.validate()?;
/// # Ok::<(), moso_jobs::Error>(())
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct JobsConfig {
    /// Which backend.
    pub backend: JobsBackendKind,
    /// Where it is. Only Redis needs one; the Postgres backend uses the
    /// application's own `Db`.
    pub url: Option<SecretString>,
    /// The table name prefix for the Postgres backend.
    pub table_prefix: String,
    /// How many jobs one worker runs at once. `None` means "one per core".
    pub concurrency: Option<usize>,
    /// Which queues a worker listens to. Empty means every queue in the
    /// registry.
    pub queues: Vec<String>,
    /// How long a lease is taken for.
    pub lease: Duration,
    /// How long to wait for in-flight jobs at shutdown.
    pub grace: Duration,
    /// What to do with what is left when the grace expires.
    pub drain: DrainMode,
    /// Pause pulling low-priority jobs above this queue depth.
    pub backpressure: Option<u64>,
    /// Whether this process runs the scheduler. A worker-only pod sets it
    /// false, and something has to set it true or nothing runs on a clock.
    pub scheduler: bool,
    /// Whether to serve the standalone dashboard at `/_jobs`.
    pub dashboard: bool,
}

impl JobsConfig {
    /// A configuration with the documented defaults.
    ///
    /// ```no_run
    /// use moso_jobs::{JobsBackendKind, JobsConfig};
    ///
    /// let _ = JobsConfig::new(JobsBackendKind::Memory);
    /// ```
    #[must_use]
    pub fn new(backend: JobsBackendKind) -> Self {
        Self {
            backend,
            url: None,
            table_prefix: "moso_jobs".to_owned(),
            concurrency: None,
            queues: Vec::new(),
            lease: crate::worker::DEFAULT_LEASE,
            grace: crate::worker::DEFAULT_GRACE,
            drain: DrainMode::Requeue,
            backpressure: None,
            // On, because something has to run the scheduler and a default of
            // off means an application whose nightly job never fires and whose
            // logs say nothing at all.
            scheduler: true,
            // Off, because the routes show payloads and a payload carries
            // identifiers, addresses and occasionally tokens.
            dashboard: false,
        }
    }

    /// Where the backend is. Only Redis needs one.
    ///
    /// ```
    /// # use moso_jobs::{JobsBackendKind, JobsConfig};
    /// let _ = JobsConfig::new(JobsBackendKind::Redis).url("redis://localhost:6379");
    /// ```
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(SecretString::new(url.into()));
        self
    }

    /// The table name prefix for the Postgres backend.
    ///
    /// ```
    /// # use moso_jobs::{JobsBackendKind, JobsConfig};
    /// let _ = JobsConfig::new(JobsBackendKind::Postgres).table_prefix("shop_jobs");
    /// ```
    #[must_use]
    pub fn table_prefix(mut self, prefix: impl Into<String>) -> Self {
        self.table_prefix = prefix.into();
        self
    }

    /// Whether this process runs the scheduler.
    ///
    /// ```
    /// # use moso_jobs::{JobsBackendKind, JobsConfig};
    /// let _ = JobsConfig::new(JobsBackendKind::Memory).scheduler(false);
    /// ```
    #[must_use]
    pub fn scheduler(mut self, scheduler: bool) -> Self {
        self.scheduler = scheduler;
        self
    }

    /// Whether to serve the standalone dashboard at `/_jobs`.
    ///
    /// ```
    /// # use moso_jobs::{JobsBackendKind, JobsConfig};
    /// let _ = JobsConfig::new(JobsBackendKind::Memory).dashboard(true);
    /// ```
    #[must_use]
    pub fn dashboard(mut self, dashboard: bool) -> Self {
        self.dashboard = dashboard;
        self
    }

    /// How long a lease is taken for.
    ///
    /// ```
    /// # use moso_jobs::{JobsBackendKind, JobsConfig};
    /// let _ = JobsConfig::new(JobsBackendKind::Memory).lease(std::time::Duration::from_secs(120));
    /// ```
    #[must_use]
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// How long to wait for in-flight jobs at shutdown.
    ///
    /// ```
    /// # use moso_jobs::{JobsBackendKind, JobsConfig};
    /// let _ = JobsConfig::new(JobsBackendKind::Memory).grace(std::time::Duration::from_secs(10));
    /// ```
    #[must_use]
    pub fn grace(mut self, grace: Duration) -> Self {
        self.grace = grace;
        self
    }

    /// Read the configuration out of the environment.
    ///
    /// `JOBS_BACKEND`, `JOBS_URL`, `JOBS_CONCURRENCY`, `JOBS_QUEUES`,
    /// `JOBS_SCHEDULER` and `JOBS_DASHBOARD`. Everything unset keeps its
    /// default, which is what makes `moso dev` need no `.env` at all.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) naming the variable and what it
    /// accepts.
    ///
    /// ```
    /// # use moso_jobs::JobsConfig;
    /// let _ = JobsConfig::from_env();
    /// ```
    pub fn from_env() -> Result<Self> {
        let backend = match std::env::var("JOBS_BACKEND") {
            Ok(value) => JobsBackendKind::parse(&value).ok_or_else(|| {
                crate::Error::config(format!(
                    "`JOBS_BACKEND={value}` is not a backend\n\
                     help: one of `postgres`, `redis` or `memory`"
                ))
            })?,
            Err(_) => JobsBackendKind::default(),
        };
        let mut config = Self::new(backend);

        if let Ok(url) = std::env::var("JOBS_URL") {
            config = config.url(url);
        }
        if let Ok(value) = std::env::var("JOBS_CONCURRENCY") {
            let concurrency = value.trim().parse::<usize>().map_err(|_| {
                crate::Error::config(format!(
                    "`JOBS_CONCURRENCY={value}` is not a whole number\n\
                     help: leave it unset for one worker per core"
                ))
            })?;
            config = config.concurrency(concurrency);
        }
        if let Ok(value) = std::env::var("JOBS_QUEUES") {
            config = config.queues(value.split(',').map(|queue| queue.trim().to_owned()));
        }
        if let Ok(value) = std::env::var("JOBS_SCHEDULER") {
            config = config.scheduler(is_true(&value));
        }
        if let Ok(value) = std::env::var("JOBS_DASHBOARD") {
            config = config.dashboard(is_true(&value));
        }
        Ok(config)
    }

    /// How many jobs one worker runs at once.
    ///
    /// ```no_run
    /// # use moso_jobs::JobsConfig;
    /// # fn f(c: JobsConfig) { let _ = c.concurrency(32); }
    /// ```
    #[must_use]
    pub fn concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = Some(concurrency);
        self
    }

    /// Which queues a worker listens to.
    ///
    /// ```no_run
    /// # use moso_jobs::JobsConfig;
    /// # fn f(c: JobsConfig) { let _ = c.queues(["default", "mail"]); }
    /// ```
    #[must_use]
    pub fn queues<S: Into<String>>(mut self, queues: impl IntoIterator<Item = S>) -> Self {
        self.queues = queues
            .into_iter()
            .map(Into::into)
            .filter(|queue| !queue.is_empty())
            .collect();
        self
    }

    /// Check for contradictions before anything tries to connect.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) naming the field and the fix: a
    /// Redis backend with no URL, a grace longer than the lease, a concurrency
    /// of zero, a queue name that no registered job uses.
    ///
    /// ```no_run
    /// # use moso_jobs::JobsConfig;
    /// # fn f(c: &JobsConfig) -> moso_jobs::Result { c.validate() }
    /// ```
    pub fn validate(&self) -> Result {
        if !self.backend.is_compiled_in() {
            return Err(crate::Error::config(format!(
                "the `{}` jobs backend is selected and its cargo feature is off\n\
                 help: add `features = [\"{}\"]` to the `moso-jobs` dependency, or set \
                 `JOBS_BACKEND` to a backend this build has",
                self.backend.as_str(),
                self.backend.feature(),
            )));
        }

        if self.backend == JobsBackendKind::Redis && self.url.is_none() {
            return Err(crate::Error::config(
                "the Redis jobs backend needs a URL and none is set\n\
                 help: set `JOBS_URL=redis://…`, or use `JobsConfig::url(..)`",
            ));
        }

        if self.concurrency == Some(0) {
            return Err(crate::Error::config(
                "`jobs.concurrency` is zero, so this worker would run nothing\n\
                 help: leave it unset for one job per core, or set it to at least 1",
            ));
        }

        if self.grace > self.lease {
            return Err(crate::Error::config(format!(
                "the shutdown grace ({grace}) is longer than the lease ({lease})\n\
                 help: a job still running when its lease expires is reclaimed and run again \
                 by another worker *while this one is finishing it*; raise the lease above \
                 the grace, or lower the grace",
                grace = humantime::format_duration(self.grace),
                lease = humantime::format_duration(self.lease),
            )));
        }

        if self.lease.is_zero() {
            return Err(crate::Error::config(
                "`jobs.lease` is zero, so every job would be reclaimed the instant it started",
            ));
        }

        Ok(())
    }

    /// Check the configuration against the registry that will run on it.
    ///
    /// Catches the one class of problem the configuration cannot see on its
    /// own: a `--queues` list naming a queue no registered job uses, which is a
    /// worker that listens forever and never runs anything.
    ///
    /// # Errors
    ///
    /// Everything [`validate`](JobsConfig::validate) reports, plus the
    /// unmatched queue names.
    ///
    /// ```
    /// # use moso_jobs::{JobRegistry, JobsBackendKind, JobsConfig};
    /// let config = JobsConfig::new(JobsBackendKind::Memory);
    /// config.validate_against(&JobRegistry::new())?;
    /// # Ok::<(), moso_jobs::Error>(())
    /// ```
    pub fn validate_against(&self, registry: &crate::JobRegistry) -> Result {
        self.validate()?;

        let known = registry.queues();
        let unknown: Vec<&String> = self
            .queues
            .iter()
            .filter(|queue| !known.contains(queue))
            .collect();
        if !unknown.is_empty() {
            return Err(crate::Error::config(format!(
                "this worker listens to {unknown:?}, and no registered job runs on \
                 {plural}\n\
                 help: the registry knows {known:?}; a worker that listens to a queue nothing \
                 uses looks exactly like one that is broken",
                plural = if unknown.len() == 1 { "it" } else { "them" },
            )));
        }
        Ok(())
    }

    /// Build the queue this configuration describes.
    ///
    /// `db` is required for the Postgres backend and for the Redis backend's
    /// outbox; a memory-backed process passes `None`.
    ///
    /// # Errors
    ///
    /// Everything [`validate`](JobsConfig::validate) reports, plus
    /// [`Error::Config`](crate::Error::Config) when the chosen backend's cargo
    /// feature is off — with the feature name in the message.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobsConfig, Queue};
    /// # use moso_orm::Db;
    /// # fn f(c: &JobsConfig, db: Option<Db>) -> moso_jobs::Result<()> {
    /// let queue: Arc<dyn Queue> = c.build(db)?;
    /// let _ = queue.name();
    /// # Ok(()) }
    /// ```
    pub fn build(&self, db: Option<moso_orm::Db>) -> Result<std::sync::Arc<dyn Queue>> {
        self.validate()?;
        match self.backend {
            JobsBackendKind::Postgres => {
                #[cfg(feature = "jobs-pg")]
                {
                    let db = db.ok_or_else(|| {
                        crate::Error::config(
                            "the Postgres jobs backend uses the application\'s own `Db` and \
                             none was passed\n\
                             help: `config.build(Some(db))` — the queue lives in the same \
                             database as the work, which is what makes `tx.enqueue(..)` \
                             transactional",
                        )
                    })?;
                    Ok(std::sync::Arc::new(
                        crate::backend::PgQueue::new(db).table_prefix(&self.table_prefix),
                    ))
                }
                #[cfg(not(feature = "jobs-pg"))]
                {
                    let _ = db;
                    Err(self.feature_off())
                }
            }
            JobsBackendKind::Redis => {
                #[cfg(feature = "jobs-redis")]
                {
                    let url = self.url.as_ref().ok_or_else(|| {
                        crate::Error::config("the Redis jobs backend needs `JOBS_URL`")
                    })?;
                    let redis = std::sync::Arc::new(
                        crate::backend::RedisQueue::new(url.expose()).prefix(&self.table_prefix),
                    );
                    match db {
                        // With a database in reach, the outbox turns
                        // `tx.enqueue(..)` into the same guarantee the Postgres
                        // backend gives natively — at the cost of a table, a
                        // relay and one relay interval of latency. The boot log
                        // says so, because that is information an operator
                        // needs.
                        #[cfg(feature = "jobs-pg")]
                        Some(db) => {
                            tracing::info!(
                                target: "moso::jobs",
                                "the Redis queue is wrapped in a transactional outbox; \
                                 `tx.enqueue(..)` writes to a table and a relay moves it, so a \
                                 transactionally enqueued job starts one relay interval late"
                            );
                            Ok(std::sync::Arc::new(
                                crate::backend::Outbox::new(db, redis.clone())
                                    .with_dead_letters(redis),
                            ))
                        }
                        #[cfg(not(feature = "jobs-pg"))]
                        Some(_) => Ok(redis),
                        None => {
                            tracing::warn!(
                                target: "moso::jobs",
                                "the Redis queue has no database behind it, so `tx.enqueue(..)` \
                                 is not transactional — a rolled-back transaction will still \
                                 have enqueued its job"
                            );
                            Ok(redis)
                        }
                    }
                }
                #[cfg(not(feature = "jobs-redis"))]
                {
                    let _ = db;
                    Err(self.feature_off())
                }
            }
            JobsBackendKind::Memory => {
                #[cfg(feature = "jobs-memory")]
                {
                    let _ = db;
                    Ok(std::sync::Arc::new(crate::backend::MemoryQueue::new()))
                }
                #[cfg(not(feature = "jobs-memory"))]
                {
                    let _ = db;
                    Err(self.feature_off())
                }
            }
        }
    }

    /// The message for a backend whose cargo feature is off.
    #[allow(
        dead_code,
        reason = "\
        only reachable in a build with a backend feature turned off, which is a \
        configuration `cargo check --all-features` never produces"
    )]
    fn feature_off(&self) -> crate::Error {
        crate::Error::config(format!(
            "the `{}` jobs backend is selected and this build does not have it\n\
             help: add `features = [\"{}\"]` to the `moso-jobs` dependency",
            self.backend.as_str(),
            self.backend.feature(),
        ))
    }

    /// Build a [`Worker`](crate::Worker) this configuration describes.
    ///
    /// ```no_run
    /// # use moso_jobs::{Jobs, JobsConfig, JobsBackendKind};
    /// # fn f(jobs: Jobs) {
    /// let worker = JobsConfig::new(JobsBackendKind::Memory).worker(jobs);
    /// # let _ = worker;
    /// # }
    /// ```
    #[must_use]
    pub fn worker(&self, jobs: crate::Jobs) -> crate::Worker {
        let registry = jobs.shared_registry();
        let mut worker = crate::Worker::new(jobs, registry)
            .lease(self.lease)
            .grace(self.grace)
            .drain_mode(self.drain)
            .backpressure(self.backpressure);
        if let Some(concurrency) = self.concurrency {
            worker = worker.concurrency(concurrency);
        }
        if !self.queues.is_empty() {
            worker = worker.queues(self.queues.clone());
        }
        worker
    }
}

/// The `/readyz` probe for the queue.
///
/// Critical by default, unlike the storage one: a process that cannot reach its
/// queue cannot enqueue, and an endpoint that silently drops the welcome email
/// is worse than one that returns 503.
///
/// ```no_run
/// # use std::sync::Arc;
/// # use moso_jobs::{JobsHealthCheck, Queue};
/// # fn f(q: Arc<dyn Queue>) { let _ = JobsHealthCheck::new(q); }
/// ```
#[derive(Clone)]
pub struct JobsHealthCheck {
    /// What to probe.
    queue: std::sync::Arc<dyn Queue>,
    /// Whether a failure makes the instance unready.
    critical: bool,
    /// Report `Degraded` when any queue is deeper than this.
    depth_warning: Option<u64>,
    /// Which queues to measure the depth of.
    queues: Vec<String>,
    /// Whether the scheduler has finished electing, when this process runs one.
    scheduler: Option<crate::schedule::SchedulerReadiness>,
}

impl JobsHealthCheck {
    /// A critical probe of `queue`.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobsHealthCheck, Queue};
    /// # fn f(q: Arc<dyn Queue>) { let _ = JobsHealthCheck::new(q); }
    /// ```
    #[must_use]
    pub fn new(queue: std::sync::Arc<dyn Queue>) -> Self {
        Self {
            queue,
            critical: true,
            depth_warning: None,
            queues: Vec::new(),
            scheduler: None,
        }
    }

    /// Measure the depth of these queues.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobsHealthCheck, Queue};
    /// # fn f(q: Arc<dyn Queue>) { let _ = JobsHealthCheck::new(q).queues(["default"]); }
    /// ```
    #[must_use]
    pub fn queues<S: Into<String>>(mut self, queues: impl IntoIterator<Item = S>) -> Self {
        self.queues = queues.into_iter().map(Into::into).collect();
        self
    }

    /// Report `Degraded` until the scheduler has elected a leader.
    ///
    /// Gate `/readyz` on this in a process that runs the scheduler, so a
    /// rolling deploy never reports every pod ready while none of them has
    /// established who runs the nightly job.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobsHealthCheck, Queue, Scheduler};
    /// # fn f(q: Arc<dyn Queue>, s: &Scheduler) {
    /// let _ = JobsHealthCheck::new(q).scheduler(s.readiness());
    /// # }
    /// ```
    #[must_use]
    pub fn scheduler(mut self, readiness: crate::schedule::SchedulerReadiness) -> Self {
        self.scheduler = Some(readiness);
        self
    }

    /// Whether a failure takes the instance out of rotation.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobsHealthCheck, Queue};
    /// # fn f(q: Arc<dyn Queue>) { let _ = JobsHealthCheck::new(q).critical(false); }
    /// ```
    #[must_use]
    pub fn critical(mut self, critical: bool) -> Self {
        self.critical = critical;
        self
    }

    /// Report degraded — not down — above this queue depth.
    ///
    /// Degraded shows in the report without pulling the instance out of
    /// rotation, which is right for a backlog: taking the pods away is exactly
    /// how a backlog becomes an outage.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobsHealthCheck, Queue};
    /// # fn f(q: Arc<dyn Queue>) { let _ = JobsHealthCheck::new(q).depth_warning(Some(10_000)); }
    /// ```
    #[must_use]
    pub fn depth_warning(mut self, depth: Option<u64>) -> Self {
        self.depth_warning = depth;
        self
    }
}

impl core::fmt::Debug for JobsHealthCheck {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JobsHealthCheck")
            .field("backend", &self.queue.name())
            .field("critical", &self.critical)
            .finish()
    }
}

impl moso_core::HealthCheck for JobsHealthCheck {
    fn check<'a>(
        &'a self,
        resolver: &'a moso_core::Resolver,
    ) -> moso_core::BoxFuture<'a, moso_core::health::HealthStatus> {
        let _ = resolver;
        Box::pin(async move {
            if let Err(error) = self.queue.probe().await {
                return moso_core::health::HealthStatus::Down(error.chain());
            }

            // A scheduler that has not finished electing is not ready. Without
            // this, a rolling deploy can report every pod healthy at the moment
            // when none of them is running the nightly job.
            if let Some(readiness) = &self.scheduler
                && !readiness.is_resolved()
            {
                return moso_core::health::HealthStatus::Degraded(
                    "the scheduler has not finished electing a leader".to_owned(),
                );
            }

            let Some(threshold) = self.depth_warning else {
                return moso_core::health::HealthStatus::Up;
            };
            match self.queue.stats(&self.queues).await {
                Ok(stats) => {
                    let deepest = stats.iter().max_by_key(|one| one.ready);
                    match deepest {
                        Some(one) if one.ready > threshold => {
                            // Degraded and not down: taking the pods out of
                            // rotation is exactly how a backlog becomes an
                            // outage.
                            moso_core::health::HealthStatus::Degraded(format!(
                                "queue `{}` is {} deep, over the {threshold} threshold",
                                one.queue, one.ready
                            ))
                        }
                        _ => moso_core::health::HealthStatus::Up,
                    }
                }
                Err(error) => moso_core::health::HealthStatus::Degraded(format!(
                    "could not read queue depth: {}",
                    error.chain()
                )),
            }
        })
    }

    fn critical(&self) -> bool {
        self.critical
    }
}

/// Whether an environment variable reads as true.
fn is_true(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}
