//! Putting work on a queue: the [`Jobs`] handle, the [`EnqueueBuilder`], and
//! the [`Enqueue`] extension that makes `tx.enqueue(..)` work.

use std::time::Duration;

use moso_core::ctx::RequestCtx;
use moso_core::di::{Dependency, ProviderReq};
use moso_openapi::OperationBuilder;

use crate::{Job, JobId, JobRegistry, Priority, Queue, Result};

/// The process-wide handle `tx.enqueue(..)` reads.
///
/// [`Enqueue::enqueue`] takes an executor and a job, and nowhere in that
/// signature is there room for a `Jobs`. It has to come from somewhere, and the
/// only two candidates are a link-time registry — which ADR-0004 refuses — and
/// one explicit statement at boot. This is that statement's storage.
static INSTALLED: std::sync::OnceLock<Jobs> = std::sync::OnceLock::new();

/// The application-facing handle to the queue.
///
/// Injected as `Inject<Jobs>`, and held by [`JobCtx`](crate::JobCtx) so a job
/// can enqueue another. Cheap to clone: it is two `Arc`s.
///
/// ```no_run
/// use moso_jobs::{Job, Jobs};
///
/// async fn go<J: Job>(jobs: &Jobs, args: J::Args) -> moso_jobs::Result {
///     J::enqueue(jobs, args).await?;
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct Jobs {
    /// Where jobs go.
    queue: std::sync::Arc<dyn Queue>,
    /// What can run them, so an enqueue of an unregistered job is caught.
    registry: std::sync::Arc<JobRegistry>,
    /// The DI graph an inline [`drain`](Jobs::drain) resolves against.
    ///
    /// `None` outside an application — a bare `Jobs` can still enqueue, it just
    /// cannot run anything inline.
    resolver: Option<moso_core::Resolver>,
    /// Which queues are refusing low-priority work, as the worker last measured
    /// them.
    backpressure: std::sync::Arc<crate::worker::Backpressure>,
    /// The dead-letter view of the same backend, when the caller wired one.
    ///
    /// `Arc<dyn Queue>` carries no way to ask whether the same object is also a
    /// [`DeadLetterQueue`](crate::DeadLetterQueue), so the composition root
    /// that built both says so. Without it the dashboard reports that there is
    /// no dead-letter view — rather than an empty list, which reads as "nothing
    /// has failed".
    dead: Option<std::sync::Arc<dyn crate::DeadLetterQueue>>,
}

impl Jobs {
    /// A handle over `queue`, validated against `registry`.
    ///
    /// This is also where the backend learns which jobs are
    /// [`SERIAL`](crate::Job::SERIAL): `SERIAL` is a property of the *type*, a
    /// backend sees only rows, and this constructor is the one place in the
    /// crate that holds both. A second `Jobs` over the same queue replaces the
    /// list rather than adding to it, so two applications sharing one backend
    /// see the registry of whichever was built last — which is the same
    /// already-documented mistake as two applications sharing one
    /// [`install`](Jobs::install).
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobRegistry, Jobs, Queue};
    /// # fn f(q: Arc<dyn Queue>, r: Arc<JobRegistry>) { let _ = Jobs::new(q, r); }
    /// ```
    #[must_use]
    pub fn new(queue: std::sync::Arc<dyn Queue>, registry: std::sync::Arc<JobRegistry>) -> Self {
        let serial: Vec<&'static str> = registry
            .all()
            .filter(|job| job.serial())
            .map(crate::RegisteredJob::name)
            .collect();
        queue.serial_jobs(&serial);
        Self {
            queue,
            registry,
            resolver: None,
            backpressure: std::sync::Arc::default(),
            dead: None,
        }
    }

    /// Wire the backend's dead-letter view onto this handle.
    ///
    /// Every shipped backend implements both traits, so this is the same
    /// `Arc` twice — but `Arc<dyn Queue>` cannot be *asked* whether it is also
    /// a [`DeadLetterQueue`](crate::DeadLetterQueue), so the composition root
    /// says.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{DeadLetterQueue, Jobs};
    /// # fn f(jobs: Jobs, dead: Arc<dyn DeadLetterQueue>) -> Jobs {
    /// jobs.with_dead_letters(dead)
    /// # }
    /// ```
    #[must_use]
    pub fn with_dead_letters(mut self, dead: std::sync::Arc<dyn crate::DeadLetterQueue>) -> Self {
        self.dead = Some(dead);
        self
    }

    /// The dead-letter view, when one is wired.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// # fn f(jobs: &Jobs) { let _ = jobs.dead_letters().is_some(); }
    /// ```
    #[must_use]
    pub fn dead_letters(&self) -> Option<&std::sync::Arc<dyn crate::DeadLetterQueue>> {
        self.dead.as_ref()
    }

    /// Mark a queue as over — or back under — its depth threshold.
    ///
    /// Called by [`Worker`](crate::Worker) on every reclaim tick. Returns
    /// whether the state changed, so the worker only logs the transition.
    pub(crate) fn set_backpressure(&self, queue: &str, active: bool) -> bool {
        self.backpressure.set(queue, active)
    }

    /// Whether `queue` is currently refusing low-priority work.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// # fn f(j: &Jobs) { let _: bool = j.backpressure_active("bulk"); }
    /// ```
    #[must_use]
    pub fn backpressure_active(&self, queue: &str) -> bool {
        self.backpressure.is_active(queue)
    }

    /// Attach the dependency graph an inline [`drain`](Jobs::drain) resolves
    /// against.
    ///
    /// The same graph a worker uses, which is what makes acceptance criterion 8
    /// — "`drain()` executes jobs with the same DI graph as a real worker" —
    /// true by construction rather than by two code paths agreeing.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// # fn f(jobs: Jobs, resolver: moso_core::Resolver) -> Jobs {
    /// jobs.with_resolver(resolver)
    /// # }
    /// ```
    #[must_use]
    pub fn with_resolver(mut self, resolver: moso_core::Resolver) -> Self {
        self.resolver = Some(resolver);
        self
    }

    /// Make this the handle `tx.enqueue(..)` uses.
    ///
    /// One statement, at boot, in the composition root. `tx.enqueue(Job, args)`
    /// has no parameter to carry a `Jobs` through, and the alternative to this
    /// is a link-time registry — which ADR-0004 refuses, for the same reason it
    /// refuses `inventory` for routes.
    ///
    /// Returns whether it was installed: a second call is a no-op, because two
    /// applications in one process sharing one queue is a mistake worth
    /// reporting rather than a race worth losing.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// # fn f(jobs: &Jobs) { assert!(jobs.install()); }
    /// ```
    pub fn install(&self) -> bool {
        INSTALLED.set(self.clone()).is_ok()
    }

    /// The installed handle, if there is one.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// let _: Option<&Jobs> = Jobs::installed();
    /// ```
    #[must_use]
    pub fn installed() -> Option<&'static Self> {
        INSTALLED.get()
    }

    /// The backend.
    ///
    /// ```no_run
    /// # use moso_jobs::{Jobs, Queue};
    /// # fn f(j: &Jobs) { let _: &dyn Queue = j.queue(); }
    /// ```
    #[must_use]
    pub fn queue(&self) -> &dyn Queue {
        self.queue.as_ref()
    }

    /// The backend, shared.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{Jobs, Queue};
    /// # fn f(j: &Jobs) { let _: Arc<dyn Queue> = j.shared_queue(); }
    /// ```
    #[must_use]
    pub fn shared_queue(&self) -> std::sync::Arc<dyn Queue> {
        std::sync::Arc::clone(&self.queue)
    }

    /// What can run.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobRegistry, Jobs};
    /// # fn f(j: &Jobs) { let _: &JobRegistry = j.registry(); }
    /// ```
    #[must_use]
    pub fn registry(&self) -> &JobRegistry {
        &self.registry
    }

    /// What can run, shared.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobRegistry, Jobs};
    /// # fn f(j: &Jobs) { let _: Arc<JobRegistry> = j.shared_registry(); }
    /// ```
    #[must_use]
    pub fn shared_registry(&self) -> std::sync::Arc<JobRegistry> {
        std::sync::Arc::clone(&self.registry)
    }

    /// The dependency graph, when one is attached.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// # fn f(j: &Jobs) { let _ = j.resolver().is_some(); }
    /// ```
    #[must_use]
    pub fn resolver(&self) -> Option<&moso_core::Resolver> {
        self.resolver.as_ref()
    }

    /// Depth and latency for every queue the registry knows about.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    ///
    /// ```no_run
    /// # use moso_jobs::{Jobs, QueueStats};
    /// # async fn f(j: &Jobs) -> moso_jobs::Result<Vec<QueueStats>> { j.stats().await }
    /// ```
    pub async fn stats(&self) -> Result<Vec<crate::QueueStats>> {
        let queues = self.registry.queues();
        self.queue.stats(&queues).await
    }

    /// Cancel a job that has not finished.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`](crate::Error::Unsupported) on a backend whose
    /// [`QueueCapabilities::cancel`](crate::QueueCapabilities::cancel) is
    /// false.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobId, Jobs};
    /// # async fn f(j: &Jobs, id: JobId) -> moso_jobs::Result<bool> { j.cancel(id).await }
    /// ```
    pub async fn cancel(&self, id: JobId) -> Result<bool> {
        self.queue.cancel(id).await
    }

    /// Run every ready job inline, in this process, until the queues are empty.
    ///
    /// The test harness's `app.jobs().drain()`. Uses the same DI graph a real
    /// worker does, so job bodies are covered by integration tests without a
    /// worker process — and a job that only works because the worker happened to
    /// have a provider registered fails here too.
    ///
    /// Returns how many jobs ran.
    ///
    /// # Errors
    ///
    /// The first job failure, so a test fails with the job's own error rather
    /// than with an empty queue.
    ///
    /// ```no_run
    /// # use moso_jobs::Jobs;
    /// # async fn f(j: &Jobs) -> moso_jobs::Result<u64> { j.drain().await }
    /// ```
    pub async fn drain(&self) -> Result<u64> {
        crate::Worker::new(self.clone(), self.shared_registry())
            .concurrency(1)
            .drain_inline()
            .await
    }
}

impl core::fmt::Debug for Jobs {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Jobs")
            .field("queue", &self.queue.name())
            .field("registered", &self.registry.len())
            .finish_non_exhaustive()
    }
}

impl Dependency for Jobs {
    const PROVIDER_REQ: &'static [ProviderReq] = &[ProviderReq::of::<Jobs>()];

    fn describe(op: &mut OperationBuilder) {
        let _ = op;
    }

    async fn resolve(ctx: &RequestCtx) -> moso_core::Result<Self> {
        // `App::build()` proved the provider exists, which is why this clone of
        // two `Arc`s is the whole of the runtime cost.
        let jobs = ctx.provider::<Jobs>()?;
        Ok((*jobs).clone())
    }
}

/// One enqueue in progress.
///
/// A future *and* a builder: `J::enqueue(&jobs, args).await?` is the whole of
/// the common case, and `.delay(..).priority(..).spawn().await?` is the
/// configured one. Both spellings work because this implements `IntoFuture`.
///
/// ```no_run
/// use moso_jobs::{Job, Jobs};
/// use std::time::Duration;
///
/// async fn later<J: Job>(jobs: &Jobs, args: J::Args) -> moso_jobs::Result {
///     J::enqueue(jobs, args)
///         .delay(Duration::from_secs(60))
///         .priority(moso_jobs::Priority::High)
///         .spawn()
///         .await?;
///     Ok(())
/// }
/// ```
pub struct EnqueueBuilder<'a, J: Job> {
    /// Where it goes.
    jobs: &'a Jobs,
    /// What to run it with.
    args: J::Args,
    /// Not before this long from now.
    delay: Option<Duration>,
    /// How urgent, overriding `J::PRIORITY`.
    priority: Option<Priority>,
    /// Which queue, overriding `J::QUEUE`.
    queue: Option<String>,
    /// The deduplication key, overriding the payload hash.
    unique_key: Option<String>,
    /// The retry budget, overriding `J::RETRIES`.
    retries: Option<u32>,
    /// The trace context to carry onto the row.
    trace_parent: Option<String>,
    /// The enqueueing actor's opaque identity to carry onto the row.
    actor: Option<String>,
    /// Where this enqueue was written, for the unregistered-job message.
    site: &'static core::panic::Location<'static>,
}

impl<'a, J: Job> EnqueueBuilder<'a, J> {
    /// Start an enqueue. What [`Job::enqueue`] calls.
    ///
    /// `#[track_caller]` so that the site recorded is the application's line and
    /// not this one — which is what lets an unregistered job name the file to
    /// change.
    #[track_caller]
    pub(crate) fn new(jobs: &'a Jobs, args: J::Args) -> Self {
        Self {
            jobs,
            args,
            delay: None,
            priority: None,
            queue: None,
            unique_key: None,
            retries: None,
            trace_parent: crate::trace::current_traceparent(),
            actor: crate::actor::current(),
            site: core::panic::Location::caller(),
        }
    }

    /// Wait this long before the job becomes ready.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>) {
    /// let _ = b.delay(std::time::Duration::from_secs(60));
    /// # }
    /// ```
    #[must_use]
    pub fn delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    /// Run no earlier than an absolute time.
    ///
    /// ```no_run
    /// # use chrono::{DateTime, Utc};
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>, at: DateTime<Utc>) { let _ = b.at(at); }
    /// ```
    #[must_use]
    pub fn at(self, when: chrono::DateTime<chrono::Utc>) -> Self {
        let delay = (when - chrono::Utc::now())
            .to_std()
            // A time already past means "now", not "an enormous delay".
            .unwrap_or(Duration::ZERO);
        self.delay(delay)
    }

    /// Override the job's priority.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job, Priority};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>) { let _ = b.priority(Priority::High); }
    /// ```
    #[must_use]
    pub fn priority(mut self, priority: Priority) -> Self {
        self.priority = Some(priority);
        self
    }

    /// Override the job's queue.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>) { let _ = b.queue("mail-urgent"); }
    /// ```
    #[must_use]
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    /// Set the deduplication key.
    ///
    /// Without one, `J::UNIQUE_FOR` deduplicates on a hash of the payload — which
    /// is right when the payload *is* the identity and wrong when it carries a
    /// timestamp. This is the override for the second case.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>) { let _ = b.unique_key("welcome:usr_1"); }
    /// ```
    #[must_use]
    pub fn unique_key(mut self, key: impl Into<String>) -> Self {
        self.unique_key = Some(key.into());
        self
    }

    /// Override the retry budget for this one enqueue.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>) { let _ = b.retries(0); }
    /// ```
    #[must_use]
    pub fn retries(mut self, retries: u32) -> Self {
        self.retries = Some(retries);
        self
    }

    /// Carry an explicit W3C trace context onto the row.
    ///
    /// The enqueueing request's context is picked up automatically; this is for
    /// the case where the job is enqueued from somewhere with no ambient span
    /// and the caller knows the trace it belongs to.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>, tp: String) { let _ = b.trace_parent(tp); }
    /// ```
    #[must_use]
    pub fn trace_parent(mut self, traceparent: impl Into<String>) -> Self {
        self.trace_parent = Some(traceparent.into());
        self
    }

    /// Carry an explicit enqueueing actor identity onto the row.
    ///
    /// The ambient identity from [`actor::scope`](crate::actor::scope) is picked
    /// up automatically; this is for the case where the job is enqueued from
    /// somewhere with no ambient actor and the caller knows, and can name, whom
    /// it should be attributed to. The value is `moso-authz`'s
    /// `ActorIdentity::to_wire` string — an identity, never a credential.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job};
    /// # fn f<J: Job>(b: EnqueueBuilder<'_, J>, identity: String) { let _ = b.actor(identity); }
    /// ```
    #[must_use]
    pub fn actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    /// Build the row this enqueue describes, checking the registry first.
    fn row(&self, args: &J::Args) -> Result<crate::QueuedJob> {
        let registered =
            self.jobs
                .registry
                .get(J::NAME)
                .ok_or_else(|| crate::Error::Unregistered {
                    name: J::NAME.to_owned(),
                    suggestion: self.jobs.registry.suggest(J::NAME),
                    site: Some(format!("{}:{}", self.site.file(), self.site.line())),
                })?;

        let payload = serde_json::to_value(args)?;
        let queue = self
            .queue
            .clone()
            .unwrap_or_else(|| registered.queue().to_owned());

        let priority = self.priority.unwrap_or_else(|| registered.priority());
        if priority < Priority::Normal && self.jobs.backpressure_active(&queue) {
            // Refused rather than dropped, and retryable rather than permanent:
            // the caller is meant to come back when the backlog clears, and a
            // silently discarded job is the worst of the three outcomes.
            return Err(crate::Error::retry(format!(
                "queue `{queue}` is over its backpressure threshold, so low-priority work is \
                 being refused\n\
                 help: raise `Worker::backpressure`, add workers, or enqueue this at \
                 `Priority::Normal` if it is not actually bulk work"
            )));
        }
        let unique_key = self.unique_key.clone().or_else(|| {
            // Only fingerprint when the job asked for deduplication: hashing
            // every payload would make `unique_key` a column with a value that
            // means nothing, and a partial index that never helps.
            J::UNIQUE_FOR.map(|_| crate::job::payload_fingerprint(J::NAME, &payload))
        });
        let retry = match self.retries {
            Some(retries) => crate::RetryPolicy::new(retries, J::BACKOFF),
            None => registered.retry(),
        };
        let now = chrono::Utc::now();

        Ok(crate::QueuedJob {
            id: JobId::new(),
            name: J::NAME.to_owned(),
            queue,
            payload,
            state: crate::JobState::Ready,
            priority,
            attempt: 1,
            retry,
            run_at: now
                + self
                    .delay
                    .and_then(|delay| chrono::Duration::from_std(delay).ok())
                    .unwrap_or_default(),
            enqueued_at: now,
            unique_key,
            trace_parent: self.trace_parent.clone(),
            actor: self.actor.clone(),
            last_error: None,
            locked_by: None,
            locked_until: None,
        })
    }

    /// Enqueue, and hand back the identifier.
    ///
    /// # Errors
    ///
    /// [`Error::Unregistered`](crate::Error::Unregistered) when `J` is not in
    /// the registry — which `App::build()` should have caught, so the message
    /// says where to add the registration.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job, JobId};
    /// # async fn f<J: Job>(b: EnqueueBuilder<'_, J>) -> moso_jobs::Result<JobId> {
    /// b.spawn().await
    /// # }
    /// ```
    pub async fn spawn(self) -> Result<JobId> {
        let row = self.row(&self.args)?;
        let id = row.id;
        let queue = row.queue.clone();
        crate::metrics::enqueued(J::NAME, &queue);
        self.jobs.queue.push(row).await?;
        tracing::debug!(
            target: "moso::jobs",
            job = J::NAME,
            %queue,
            %id,
            "enqueued"
        );
        Ok(id)
    }

    /// Enqueue inside a transaction.
    ///
    /// Equivalent to [`Enqueue::enqueue`] and available here so a builder that
    /// was configured before the transaction opened can still be committed with
    /// it.
    ///
    /// # Errors
    ///
    /// As [`spawn`](EnqueueBuilder::spawn), plus
    /// [`Error::Unsupported`](crate::Error::Unsupported) when the backend
    /// cannot enqueue transactionally.
    ///
    /// ```no_run
    /// # use moso_jobs::{EnqueueBuilder, Job, JobId};
    /// # use moso_orm::Tx;
    /// # async fn f<J: Job>(b: EnqueueBuilder<'_, J>, tx: &Tx) -> moso_jobs::Result<JobId> {
    /// b.spawn_in(tx).await
    /// # }
    /// ```
    pub async fn spawn_in(self, tx: &moso_orm::Tx) -> Result<JobId> {
        let row = self.row(&self.args)?;
        let id = row.id;
        let queue = row.queue.clone();
        crate::metrics::enqueued(J::NAME, &queue);
        self.jobs.queue.push_tx(tx, row).await?;
        tracing::debug!(
            target: "moso::jobs",
            job = J::NAME,
            %queue,
            %id,
            "enqueued in the caller's transaction"
        );
        Ok(id)
    }
}

impl<J: Job> core::fmt::Debug for EnqueueBuilder<'_, J> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EnqueueBuilder")
            .field("job", &J::NAME)
            .field("delay", &self.delay)
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl<'a, J: Job> IntoFuture for EnqueueBuilder<'a, J> {
    type Output = Result<JobId>;
    type IntoFuture = moso_core::BoxFuture<'a, Result<JobId>>;

    /// So that `J::enqueue(&jobs, args).await?` works without `.spawn()`.
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.spawn())
    }
}

/// `tx.enqueue(Job, args)` — transactional enqueue on any executor.
///
/// The headline feature of `docs/03-batteries/32-jobs.md`, and the reason this
/// crate depends on `moso-orm`. Inside a transaction the job row is written with
/// the work that caused it, so the most common bug in every Rails, Celery and
/// Sidekiq application — a welcome email for a user whose creation rolled back —
/// is not expressible.
///
/// ```text
/// db.transaction(|tx| async move {
///     let user = User::insert(new).fetch_one(tx).await?;
///     tx.enqueue(SendWelcomeEmail, SendWelcome { user_id: user.id }).await?;
///     Ok(user)
/// }).await?;
/// ```
///
/// # Where the `Jobs` comes from
///
/// This signature has no room for one, so it reads the handle
/// [`Jobs::install`] put there at boot. That is one statement in the
/// composition root, not a link-time registry: ADR-0004 refuses `inventory` for
/// routes and the same argument applies here. Calling `enqueue` before
/// `install` is an [`Error::Config`](crate::Error::Config) that says exactly
/// which line to add.
///
/// # On `&Db`
///
/// It works, and it is not transactional — there is no transaction to join. The
/// method is on [`Executor`](moso_orm::Executor) rather than on `Tx` alone so
/// that a function generic over its executor can enqueue, which is how a service
/// method ends up usable both inside and outside a transaction. The docs are
/// explicit that the guarantee comes from the transaction and not from the
/// method.
#[diagnostic::on_unimplemented(
    message = "`{Self}` cannot enqueue a job",
    label = "not an executor",
    note = "`enqueue` is available on `&Db`, `&Tx` and `&mut Tx` — the same three executors a \
            query runs on",
    note = "help: inside `db.transaction(|tx| …)`, use the `tx` the closure gives you; that is \
            what makes the enqueue roll back with the work",
    note = "help: outside a transaction, `Job::enqueue(&jobs, args)` is the direct spelling"
)]
pub trait Enqueue<'e>: moso_orm::Executor<'e> {
    /// Enqueue `job` with `args`, in this executor's transaction when it has one.
    ///
    /// The first parameter is the job *value* — the unit struct `#[job]`
    /// generates — so the call site reads `tx.enqueue(SendWelcomeEmail, args)`
    /// and needs no turbofish.
    ///
    /// # Errors
    ///
    /// As [`EnqueueBuilder::spawn_in`].
    #[track_caller]
    fn enqueue<J: Job>(self, job: J, args: J::Args) -> impl Future<Output = Result<JobId>> + Send;
}

// A blanket impl over the three executors, because `Executor` is sealed and
// those three are all there will ever be. `do_not_recommend` keeps a failed
// `Executor` bound from being reported as "consider implementing `Enqueue`".
#[diagnostic::do_not_recommend]
impl<'e, E: moso_orm::Executor<'e> + Send> Enqueue<'e> for E {
    #[track_caller]
    fn enqueue<J: Job>(self, job: J, args: J::Args) -> impl Future<Output = Result<JobId>> + Send {
        let _ = job;
        let site = core::panic::Location::caller();
        let handle = self.handle();
        async move {
            let jobs = Jobs::installed().ok_or_else(not_installed)?;
            let mut builder = EnqueueBuilder::<J>::new(jobs, args);
            builder.site = site;

            match handle.transaction() {
                // The whole point: the row is written on the caller's
                // transaction, so it commits and rolls back with the work.
                Some(tx) => builder.spawn_in(tx).await,
                // `&Db` has no transaction to join. Documented as such rather
                // than silently pretending, because a guarantee that is
                // sometimes there is worse than one that never is — and said
                // out loud at `DEBUG`, naming the line, because the two spellings
                // are one character apart and only one of them is transactional.
                //
                // `DEBUG` and not `WARN`: `db.enqueue(..)` is a legitimate call
                // in a service that genuinely has no transaction, and a warning
                // an application cannot act on is a warning it learns to ignore.
                None => {
                    tracing::debug!(
                        target: "moso::jobs",
                        job = J::NAME,
                        site = %format_args!("{}:{}", site.file(), site.line()),
                        "enqueued outside a transaction; this row commits on its own and will \
                         not roll back with the caller's work — the receiver has to be a `Tx` \
                         for that"
                    );
                    builder.spawn().await
                }
            }
        }
    }
}

/// The message for `tx.enqueue(..)` before `Jobs::install`.
fn not_installed() -> crate::Error {
    crate::Error::config(
        "`tx.enqueue(..)` needs the queue handle and nothing installed one\n\
         help: add `jobs.install();` in the composition root, next to where you build the \
         `Jobs`\n\
         note: `Job::enqueue(&jobs, args)` takes the handle directly and needs no install; the \
         extension method exists so a transaction can carry the enqueue, and a method on \
         `Executor` has nowhere to put a third argument",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JobCtx;
    use crate::backend::MemoryQueue;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct Args {
        user_id: u64,
    }

    struct Welcome;
    impl Job for Welcome {
        type Args = Args;
        const NAME: &'static str = "send_welcome_email";
        const QUEUE: &'static str = "mail";
        const RETRIES: u32 = 5;
        const UNIQUE_FOR: Option<Duration> = Some(Duration::from_secs(600));
        const PRIORITY: Priority = Priority::High;
        async fn run(_args: Args, _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    struct Unregistered;
    impl Job for Unregistered {
        type Args = ();
        const NAME: &'static str = "send_welcome_emai";
        async fn run(_args: (), _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    /// A handle over a memory queue the test keeps a reference to, so it can
    /// read back exactly what was written.
    fn jobs() -> (Jobs, std::sync::Arc<MemoryQueue>) {
        let queue = std::sync::Arc::new(MemoryQueue::new());
        let jobs = Jobs::new(
            std::sync::Arc::clone(&queue) as std::sync::Arc<dyn Queue>,
            std::sync::Arc::new(JobRegistry::new().register::<Welcome>()),
        );
        (jobs, queue)
    }

    /// The row an enqueue produces is what every backend stores, so its
    /// defaults come from the job's constants and its overrides from the
    /// builder.
    #[tokio::test]
    async fn the_row_takes_its_defaults_from_the_job_and_its_overrides_from_the_builder() {
        let (jobs, queue) = jobs();
        Welcome::enqueue(&jobs, Args { user_id: 7 })
            .await
            .expect("enqueued");

        let rows = queue.enqueued("send_welcome_email");
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.queue, "mail");
        assert_eq!(row.priority, Priority::High);
        assert_eq!(row.retry.max_attempts(), 5);
        assert_eq!(row.attempt, 1);
        assert_eq!(row.state, crate::JobState::Ready);
        assert!(row.unique_key.is_some(), "UNIQUE_FOR asks for a key");
        assert_eq!(row.payload["user_id"], serde_json::json!(7));

        queue.clear();
        Welcome::enqueue(&jobs, Args { user_id: 9 })
            .delay(Duration::from_secs(60))
            .priority(Priority::Low)
            .queue("mail-urgent")
            .unique_key("welcome:usr_9")
            .retries(2)
            .spawn()
            .await
            .expect("enqueued");
        let rows = queue.enqueued("send_welcome_email");
        let row = &rows[0];
        assert_eq!(row.queue, "mail-urgent");
        assert_eq!(row.priority, Priority::Low);
        assert_eq!(row.retry.max_attempts(), 2);
        assert_eq!(row.unique_key.as_deref(), Some("welcome:usr_9"));
        assert!(row.run_at > row.enqueued_at, "the delay pushed it out");
    }

    /// Acceptance criterion 3, at runtime: the message names the file and line
    /// of the enqueue, because that is the line somebody has to change.
    #[tokio::test]
    async fn enqueuing_an_unregistered_job_names_the_enqueue_site() {
        let (jobs, _queue) = jobs();
        let error = Unregistered::enqueue(&jobs, ())
            .await
            .expect_err("nothing registered it");

        let rendered = error.to_string();
        assert!(
            rendered.contains("is enqueued but not registered"),
            "{rendered}"
        );
        assert!(rendered.contains("enqueued at  "), "{rendered}");
        assert!(rendered.contains("enqueue.rs:"), "{rendered}");
        // One character away from a registered name, so the suggestion fires.
        assert!(
            rendered.contains("did you mean `send_welcome_email`?"),
            "{rendered}"
        );
        assert!(error.skips_retries());
    }

    /// A job with no `UNIQUE_FOR` must not get a dedup key it never asked for:
    /// a column full of hashes nothing reads is a partial index that never
    /// helps.
    #[tokio::test]
    async fn a_job_without_a_dedup_window_gets_no_key() {
        struct Plain;
        impl Job for Plain {
            type Args = ();
            const NAME: &'static str = "plain";
            async fn run(_args: (), _ctx: JobCtx) -> Result {
                Ok(())
            }
        }

        let queue = std::sync::Arc::new(MemoryQueue::new());
        let jobs = Jobs::new(
            std::sync::Arc::clone(&queue) as std::sync::Arc<dyn Queue>,
            std::sync::Arc::new(JobRegistry::new().register::<Plain>()),
        );
        Plain::enqueue(&jobs, ()).await.expect("enqueued");
        assert!(queue.enqueued("plain")[0].unique_key.is_none());
    }

    /// `at()` in the past is "now", not an enormous saturating delay.
    #[tokio::test]
    async fn an_absolute_time_already_past_means_now() {
        let (jobs, queue) = jobs();
        Welcome::enqueue(&jobs, Args { user_id: 1 })
            .at(chrono::Utc::now() - chrono::Duration::hours(1))
            .spawn()
            .await
            .expect("enqueued");
        let rows = queue.enqueued("send_welcome_email");
        assert!(rows[0].run_at <= chrono::Utc::now() + chrono::Duration::seconds(1));
    }

    /// The dedup key defaults to the payload's fingerprint, so two identical
    /// enqueues collapse and two different ones do not.
    #[tokio::test]
    async fn identical_payloads_share_a_dedup_key_and_different_ones_do_not() {
        let (jobs, queue) = jobs();

        Welcome::enqueue(&jobs, Args { user_id: 3 })
            .await
            .expect("first");
        let first = queue.enqueued("send_welcome_email")[0].unique_key.clone();

        queue.clear();
        Welcome::enqueue(&jobs, Args { user_id: 3 })
            .await
            .expect("same");
        let same = queue.enqueued("send_welcome_email")[0].unique_key.clone();

        queue.clear();
        Welcome::enqueue(&jobs, Args { user_id: 4 })
            .await
            .expect("other");
        let other = queue.enqueued("send_welcome_email")[0].unique_key.clone();

        assert_eq!(first, same);
        assert_ne!(first, other);
    }

    /// Deduplication is a *successful no-op*, not an error: two enqueues of the
    /// same payload inside the window produce one row and two `Ok`s.
    #[tokio::test]
    async fn a_duplicate_enqueue_succeeds_and_produces_one_row() {
        let (jobs, queue) = jobs();
        let first = Welcome::enqueue(&jobs, Args { user_id: 11 })
            .await
            .expect("first");
        let second = Welcome::enqueue(&jobs, Args { user_id: 11 })
            .await
            .expect("the duplicate is not an error");

        assert_ne!(first, second, "each call gets its own identifier");
        assert_eq!(
            queue.enqueued("send_welcome_email").len(),
            1,
            "but only one row exists"
        );
    }

    /// The trace context of the enqueueing request travels on the row. That is
    /// the whole mechanism behind `request -> job -> outbound call`.
    #[tokio::test]
    async fn an_explicit_trace_context_travels_on_the_row() {
        let (jobs, queue) = jobs();
        Welcome::enqueue(&jobs, Args { user_id: 5 })
            .trace_parent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
            .spawn()
            .await
            .expect("enqueued");
        assert_eq!(
            queue.enqueued("send_welcome_email")[0]
                .trace_parent
                .as_deref(),
            Some("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01")
        );
    }

    /// The enqueueing actor's identity is captured from the ambient scope onto
    /// the row, the same way the trace context is. That is what lets a worker
    /// attribute the job to whoever scheduled it.
    #[tokio::test]
    async fn the_ambient_actor_identity_is_captured_onto_the_row() {
        let (jobs, queue) = jobs();
        crate::actor::scope("usr_7".to_owned(), async {
            Welcome::enqueue(&jobs, Args { user_id: 5 })
                .await
                .expect("enqueued");
        })
        .await;
        assert_eq!(
            queue.enqueued("send_welcome_email")[0].actor.as_deref(),
            Some("usr_7"),
        );
    }

    /// With no ambient actor the row carries none, and an explicit `.actor(..)`
    /// names one regardless — for an enqueue from a context that has no scope.
    #[tokio::test]
    async fn an_explicit_actor_identity_overrides_the_absent_ambient_one() {
        let (jobs, queue) = jobs();
        Welcome::enqueue(&jobs, Args { user_id: 1 })
            .await
            .expect("enqueued");
        assert!(
            queue.enqueued("send_welcome_email")[0].actor.is_none(),
            "no scope, no actor",
        );

        queue.clear();
        Welcome::enqueue(&jobs, Args { user_id: 2 })
            .actor("svc_ci")
            .spawn()
            .await
            .expect("enqueued");
        assert_eq!(
            queue.enqueued("send_welcome_email")[0].actor.as_deref(),
            Some("svc_ci"),
        );
    }

    /// The message for `tx.enqueue` before `install` has to name the statement
    /// to add, because there is no way to guess it.
    #[test]
    fn the_uninstalled_message_names_the_statement_to_add() {
        let rendered = not_installed().to_string();
        assert!(rendered.contains("jobs.install();"), "{rendered}");
        assert!(rendered.contains("Job::enqueue(&jobs, args)"), "{rendered}");
    }

    /// A `Jobs` prints what an operator needs and nothing that would leak a
    /// payload into a log.
    #[test]
    fn the_debug_output_names_the_backend_and_no_payloads() {
        let (jobs, _queue) = jobs();
        let rendered = format!("{jobs:?}");
        assert!(rendered.contains("memory"), "{rendered}");
        assert!(rendered.contains("registered: 1"), "{rendered}");
    }
}
