//! The [`Job`] trait, its identity, and the [`JobCtx`] a running job holds.
//!
//! The trait is **Moso's**, not a re-export. Whatever executes jobs underneath
//! can change without a single line of application code changing, which is the
//! same argument ADR-0005 makes for the SQL facade.
//!
//! # What `#[job]` generates
//!
//! ```text
//! // written
//! #[job(queue = "mail", retries = 5, backoff = "exponential(30s, max = 1h)",
//!       timeout = "2m", unique_for = "10m")]
//! pub async fn send_welcome_email(
//!     args: SendWelcome,
//!     Inject(db): Inject<Db>,
//!     Inject(mail): Inject<dyn Mailer>,
//!     ctx: JobCtx,
//! ) -> Result<()> { … }
//!
//! // generated
//! #[derive(Clone, Copy, Debug, Default)]
//! pub struct SendWelcomeEmail;
//!
//! impl ::moso::__private::Job for SendWelcomeEmail {
//!     type Args = SendWelcome;
//!     const NAME: &'static str = "send_welcome_email";   // stable wire name
//!     const QUEUE: &'static str = "mail";
//!     const RETRIES: u32 = 5;
//!     const TIMEOUT: Duration = Duration::from_secs(120);
//!     const UNIQUE_FOR: Option<Duration> = Some(Duration::from_secs(600));
//!
//!     fn backoff(attempt: u32) -> Duration { /* the parsed policy */ }
//!
//!     async fn run(args: SendWelcome, ctx: JobCtx) -> Result<()> {
//!         let db = ctx.inject::<Db>()?;             // the `Inject(..)` parameters
//!         let mail = ctx.inject_dyn::<dyn Mailer>()?;
//!         /* the body */
//!     }
//! }
//! ```
//!
//! `NAME` is the **wire** name and is stable across refactors: renaming the Rust
//! function must not orphan 40,000 queued rows, so the macro derives it from the
//! function name once and `#[job(name = "…")]` pins it when the function moves.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::{EnqueueBuilder, Jobs, Priority, Result};

/// One job's identity in the queue.
///
/// A ULID: sortable by creation time, so a queue table's primary-key index does
/// not fragment the way a v4 UUID's does.
///
/// ```
/// use moso_jobs::JobId;
///
/// let id = JobId::new();
/// assert!(!id.to_string().is_empty());
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct JobId(uuid::Uuid);

impl JobId {
    /// A fresh, time-ordered identifier.
    ///
    /// ```
    /// use moso_jobs::JobId;
    ///
    /// let _ = JobId::new();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::now_v7())
    }

    /// The identifier as a UUID, for a column or a trace field.
    ///
    /// ```
    /// # use moso_jobs::JobId;
    /// let id = JobId::new();
    /// let _: uuid::Uuid = id.as_uuid();
    /// ```
    #[must_use]
    pub const fn as_uuid(self) -> uuid::Uuid {
        self.0
    }

    /// Wrap an existing identifier, when reading a row back.
    ///
    /// ```
    /// # use moso_jobs::JobId;
    /// let id = JobId::new();
    /// assert_eq!(JobId::from_uuid(id.as_uuid()), id);
    /// ```
    #[must_use]
    pub const fn from_uuid(id: uuid::Uuid) -> Self {
        Self(id)
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Display for JobId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl core::str::FromStr for JobId {
    type Err = crate::Error;

    /// Parse an identifier out of a URL, for the dashboard's cancel button.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when the text is not a UUID.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        text.parse::<uuid::Uuid>()
            .map(Self)
            .map_err(|error| crate::Error::config(format!("`{text}` is not a job id: {error}")))
    }
}

/// The default queue, for a job that names none.
///
/// ```
/// assert_eq!(moso_jobs::DEFAULT_QUEUE, "default");
/// ```
pub const DEFAULT_QUEUE: &str = "default";

/// The default retry budget.
///
/// Twenty-five attempts under [`Backoff::default_exponential`](crate::Backoff::default_exponential)
/// — 30 seconds doubling to an hour — spans about **eighteen hours**: seven
/// attempts to reach the hourly ceiling, then eighteen more an hour apart.
///
/// That is long enough to sit out a third party's overnight incident and short
/// enough that a genuinely broken job reaches the dead-letter queue while
/// somebody still remembers deploying it. It is deliberately *not* the "about
/// three weeks" a Sidekiq-style uncapped ladder gives: a job still retrying
/// three weeks later is a job nobody is going to look at, and twenty-five rows
/// per broken job sitting in the hot table for three weeks is a queue that
/// stops being fast.
///
/// A job that genuinely needs to survive a weekend says so:
/// `#[job(retries = 60, backoff = "exponential(1m, max = 2h)")]`.
///
/// ```
/// assert_eq!(moso_jobs::DEFAULT_RETRIES, 25);
/// ```
pub const DEFAULT_RETRIES: u32 = 25;

/// The default per-attempt timeout.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_jobs::DEFAULT_TIMEOUT, Duration::from_secs(300));
/// ```
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// Work to be done later.
///
/// ```
/// use moso_jobs::{Job, JobCtx, Result};
/// use serde::{Deserialize, Serialize};
///
/// /// Which account to greet.
/// #[derive(Serialize, Deserialize)]
/// pub struct SendWelcome {
///     /// The new account.
///     pub user_id: u64,
/// }
///
/// /// Greets a new account.
/// #[derive(Clone, Copy, Debug, Default)]
/// pub struct SendWelcomeEmail;
///
/// impl Job for SendWelcomeEmail {
///     type Args = SendWelcome;
///     const NAME: &'static str = "send_welcome_email";
///
///     async fn run(args: SendWelcome, _ctx: JobCtx) -> Result {
///         let _ = args.user_id;
///         Ok(())
///     }
/// }
///
/// assert_eq!(SendWelcomeEmail::QUEUE, "default");
/// ```
///
/// # Delivery is at-least-once
///
/// Said here rather than buried in a table, because it changes how job bodies
/// are written: **jobs must be idempotent**. A worker that dies after doing the
/// work and before acknowledging it will run the job again. Use an idempotency
/// key and a unique constraint, or [`JobCtx::once`], which is that pattern with
/// the table already built.
#[diagnostic::on_unimplemented(
    message = "`{Self}` is not a job",
    label = "not a job",
    note = "a job needs `Args`, a stable `NAME`, and an async `run(args, ctx)`",
    note = "help: write it as a function and let the macro do it — \
            `#[job] async fn send_welcome(args: SendWelcome, ctx: JobCtx) -> Result {{ … }}` \
            generates the type, the impl and the typed enqueue API",
    note = "help: `Args` must be `Serialize + DeserializeOwned`, because the payload crosses a \
            process boundary; a field added with a serde default is a safe change, a renamed \
            field is not"
)]
pub trait Job: Send + Sync + Sized + 'static {
    /// The payload. Crosses a process boundary, so it must round-trip.
    type Args: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// The **wire** name, stable across refactors.
    ///
    /// Not the Rust path: renaming a module must not orphan every queued row.
    /// The macro derives it from the function's name and `#[job(name = "…")]`
    /// pins it when the function moves.
    const NAME: &'static str;

    /// Which queue this runs on.
    const QUEUE: &'static str = DEFAULT_QUEUE;

    /// How many times to retry before the dead-letter queue.
    const RETRIES: u32 = DEFAULT_RETRIES;

    /// How long one attempt may take.
    const TIMEOUT: Duration = DEFAULT_TIMEOUT;

    /// Dedupe window for identical payloads. `None` means no deduplication.
    ///
    /// Two enqueues of the same job with the same
    /// [`unique_key`](crate::EnqueueBuilder::unique_key) inside the window
    /// produce one row. Without an explicit key, the key is a hash of the
    /// serialised payload.
    const UNIQUE_FOR: Option<Duration> = None;

    /// The default priority.
    const PRIORITY: Priority = Priority::Normal;

    /// Whether this job never has two instances running at once.
    ///
    /// Fleet-wide, not per process: a worker will not lease a second row of a
    /// serial job while one is leased anywhere. The claim is the leased row's
    /// own lease, so there is no second lock to renew or leak and a worker that
    /// dies frees the chain exactly when its lease expires.
    ///
    /// # Per job type, not per payload
    ///
    /// Two enqueues of the same serial job with **different** arguments still
    /// run one after the other — the exclusion is on
    /// [`NAME`](Job::NAME). Per-payload exclusion is what
    /// [`UNIQUE_FOR`](Job::UNIQUE_FOR) and
    /// [`unique_key`](crate::EnqueueBuilder::unique_key) already give, and the
    /// two compose: `SERIAL` orders the type, the key collapses the duplicates.
    ///
    /// Opt-in because it costs throughput: a serial job is a queue of one,
    /// however many workers are running.
    ///
    /// A backend that answers
    /// [`QueueCapabilities::serial_chains`](crate::QueueCapabilities::serial_chains)
    /// `false` cannot keep this promise, and
    /// [`Worker::validate`](crate::Worker::validate) reports the pair as a boot
    /// problem rather than letting the promise quietly not hold.
    const SERIAL: bool = false;

    /// The retry ladder, as a value.
    ///
    /// This — not [`backoff`](Job::backoff) — is what travels on the queued row,
    /// because a policy read from the type at *retry* time would retroactively
    /// change what a row enqueued before the deploy had promised.
    ///
    /// The default is 30 seconds doubling to an hour. `#[job(backoff = "…")]`
    /// sets it, and [`Backoff::parse`](crate::Backoff::parse) is the grammar.
    const BACKOFF: crate::Backoff = crate::Backoff::default_exponential();

    /// How long to wait before attempt `attempt` (one-based).
    ///
    /// The function form of [`BACKOFF`](Job::BACKOFF), for a job body or a test
    /// that wants to read the ladder. Overriding *this* without overriding
    /// `BACKOFF` gives a job whose row promises one thing and whose type says
    /// another, so [`JobRegistry::validate`](crate::JobRegistry::validate)
    /// compares the two at boot and reports the disagreement rather than
    /// letting the row quietly win.
    ///
    /// Jitter is not applied here: [`Backoff::delay_jittered`](crate::Backoff::delay_jittered)
    /// is what a worker calls, and jitter is not optional — a thousand jobs
    /// failing together and retrying together is a thundering herd against
    /// whatever failed.
    #[must_use]
    fn backoff(attempt: u32) -> Duration {
        Self::BACKOFF.delay(attempt)
    }

    /// The whole retry policy this job's constants describe.
    ///
    /// Resolved once at registration and then carried on every row it enqueues.
    #[must_use]
    fn retry_policy() -> crate::RetryPolicy {
        crate::RetryPolicy::new(Self::RETRIES, Self::BACKOFF)
    }

    /// Do the work.
    ///
    /// # Errors
    ///
    /// [`Error::retry`](crate::Error::retry) for something worth trying again,
    /// [`Error::permanent`](crate::Error::permanent) for something that will
    /// fail identically next time. `?` on a `moso_orm::Error` or a
    /// `moso_core::Error` picks the right one automatically.
    fn run(args: Self::Args, ctx: JobCtx) -> impl Future<Output = Result> + Send;

    /// Called when an attempt fails, before the retry is scheduled.
    ///
    /// For alerting and compensating actions. It cannot itself fail — a failing
    /// failure hook is a loop — so it returns nothing and logs its own problems.
    fn on_failure(
        args: &Self::Args,
        error: &crate::Error,
        ctx: &JobCtx,
    ) -> impl Future<Output = ()> + Send {
        let _ = (args, error, ctx);
        async {}
    }

    /// Enqueue this job.
    ///
    /// The builder is a future, so the common case is
    /// `SendWelcomeEmail::enqueue(&jobs, args).await?` and the configured case
    /// chains `.delay(..).priority(..)` before the `await`.
    ///
    /// ```no_run
    /// # use moso_jobs::{Job, Jobs};
    /// # async fn f<J: Job>(jobs: &Jobs, args: J::Args) -> moso_jobs::Result {
    /// J::enqueue(jobs, args).await?;
    /// Ok(())
    /// # }
    /// ```
    #[must_use]
    #[track_caller]
    fn enqueue(jobs: &Jobs, args: Self::Args) -> EnqueueBuilder<'_, Self> {
        EnqueueBuilder::new(jobs, args)
    }
}

/// Cancellation, as one word the job body can both await and poll.
///
/// A bare `Notify` cannot answer [`JobCtx::is_cancelled`] — it has no state to
/// read — and a bare flag cannot be awaited. Both, together, in a type nobody
/// outside this crate can build.
#[derive(Debug, Default)]
pub(crate) struct Cancellation {
    /// Whether cancellation has been signalled.
    flagged: AtomicBool,
    /// Whether it was the worker draining rather than an operator cancelling.
    ///
    /// The two are the same thing to the job body — stop, do not acknowledge —
    /// and different things to an operator reading `moso_jobs_retries_total`:
    /// one is a deploy, the other is somebody pressing a button.
    draining: AtomicBool,
    /// Wakes everything currently awaiting it.
    notify: tokio::sync::Notify,
}

impl Cancellation {
    /// Signal every waiter, now and in the future.
    pub(crate) fn cancel(&self) {
        self.flagged.store(true, Ordering::SeqCst);
        self.notify.notify_waiters();
    }

    /// Signal, and record that it was a drain putting the job back.
    pub(crate) fn cancel_for_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
        self.cancel();
    }

    /// Whether the cancellation was a drain.
    pub(crate) fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Whether it has already fired.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.flagged.load(Ordering::SeqCst)
    }

    /// Resolve when it fires, immediately if it already has.
    pub(crate) async fn cancelled(&self) {
        // The order matters: register the waiter *before* re-reading the flag,
        // or a `cancel` between the read and the wait is lost forever.
        let waiter = self.notify.notified();
        if self.is_cancelled() {
            return;
        }
        waiter.await;
    }
}

/// What a running job knows about itself.
///
/// ```no_run
/// use moso_jobs::{JobCtx, Result};
///
/// async fn body(ctx: JobCtx) -> Result {
///     if ctx.attempt() > 1 {
///         // A retry. The previous attempt may have done part of the work.
///     }
///     ctx.heartbeat().await?;
///     Ok(())
/// }
/// ```
#[derive(Clone)]
pub struct JobCtx {
    /// Which job row this is.
    id: JobId,
    /// The job's wire name.
    name: &'static str,
    /// Which queue it came off.
    queue: String,
    /// Which attempt this is, one-based.
    attempt: u32,
    /// How many attempts there will be in total.
    max_attempts: u32,
    /// When it was enqueued.
    enqueued_at: chrono::DateTime<chrono::Utc>,
    /// The worker running it.
    worker: crate::WorkerId,
    /// Where dependencies come from — the same graph a handler uses.
    resolver: moso_core::Resolver,
    /// The queue, so a job can enqueue another.
    jobs: std::sync::Arc<Jobs>,
    /// The trace context the enqueueing request carried, so the job's span is a
    /// child of the request's.
    trace_parent: Option<String>,
    /// The opaque identity of whoever enqueued the job, restored from the row so
    /// the body can attribute the work to them.
    actor: Option<String>,
    /// Set when the worker is draining or an operator cancelled the job.
    cancelled: std::sync::Arc<Cancellation>,
    /// The lease this attempt holds, so [`JobCtx::heartbeat`] can extend it.
    ///
    /// `None` for an inline drain, where nothing can steal the job because
    /// nothing else is running.
    lease: Option<std::sync::Arc<crate::Lease>>,
    /// How far ahead a heartbeat pushes the lease.
    lease_extension: Duration,
}

impl JobCtx {
    /// Assemble a context for one attempt. Called by the worker.
    #[allow(
        clippy::too_many_arguments,
        reason = "\
        every field is genuinely independent and the alternative — a builder for \
        a type only this crate constructs — buys nothing"
    )]
    pub(crate) fn new(
        id: JobId,
        name: &'static str,
        queue: String,
        attempt: u32,
        max_attempts: u32,
        enqueued_at: chrono::DateTime<chrono::Utc>,
        worker: crate::WorkerId,
        resolver: moso_core::Resolver,
        jobs: std::sync::Arc<Jobs>,
        trace_parent: Option<String>,
        actor: Option<String>,
        cancelled: std::sync::Arc<Cancellation>,
        lease: Option<std::sync::Arc<crate::Lease>>,
        lease_extension: Duration,
    ) -> Self {
        Self {
            id,
            name,
            queue,
            attempt,
            max_attempts,
            enqueued_at,
            worker,
            resolver,
            jobs,
            trace_parent,
            actor,
            cancelled,
            lease,
            lease_extension,
        }
    }

    /// Which job row this is.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobCtx, JobId};
    /// # fn f(c: &JobCtx) { let _: JobId = c.id(); }
    /// ```
    #[must_use]
    pub fn id(&self) -> JobId {
        self.id
    }

    /// The job's wire name.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: &'static str = c.name(); }
    /// ```
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.name
    }

    /// Which queue it came off.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: &str = c.queue(); }
    /// ```
    #[must_use]
    pub fn queue(&self) -> &str {
        &self.queue
    }

    /// Which worker is running it.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobCtx, WorkerId};
    /// # fn f(c: &JobCtx) { let _: &WorkerId = c.worker(); }
    /// ```
    #[must_use]
    pub fn worker(&self) -> &crate::WorkerId {
        &self.worker
    }

    /// Which attempt this is, starting at one.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: u32 = c.attempt(); }
    /// ```
    #[must_use]
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Whether this is the last attempt before the dead-letter queue.
    ///
    /// For a job that wants to do something different on the way out — write a
    /// compensating row, notify a human.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: bool = c.is_last_attempt(); }
    /// ```
    #[must_use]
    pub fn is_last_attempt(&self) -> bool {
        self.attempt >= self.max_attempts
    }

    /// How long the job has been waiting since it was enqueued.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: std::time::Duration = c.latency(); }
    /// ```
    #[must_use]
    pub fn latency(&self) -> Duration {
        (chrono::Utc::now() - self.enqueued_at)
            .to_std()
            // A negative latency means the enqueueing process's clock is ahead
            // of this one's. That is a real deployment, and reporting a huge
            // number would be worse than reporting none.
            .unwrap_or(Duration::ZERO)
    }

    /// Resolve a dependency, exactly as a handler's `Inject<T>` would.
    ///
    /// `#[job]` generates one of these per `Inject(..)` parameter, so a job body
    /// reads like a handler body and the DI graph is the same one
    /// `App::build()` validated.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when no provider is registered
    /// — which `App::build()` should have caught, so it names the job and says
    /// so.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) -> moso_jobs::Result<std::sync::Arc<String>> { c.inject::<String>() }
    /// ```
    pub fn inject<T: Send + Sync + 'static>(&self) -> Result<std::sync::Arc<T>> {
        self.resolver
            .get::<T>()
            .map_err(|error| self.missing_provider(core::any::type_name::<T>(), &error))
    }

    /// Resolve a `dyn Trait` dependency.
    ///
    /// # Errors
    ///
    /// As [`inject`](JobCtx::inject).
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # trait Mailer: Send + Sync {}
    /// # fn f(c: &JobCtx) -> moso_jobs::Result<std::sync::Arc<dyn Mailer>> {
    /// c.inject_dyn::<dyn Mailer>()
    /// # }
    /// ```
    pub fn inject_dyn<T: ?Sized + Send + Sync + 'static>(&self) -> Result<std::sync::Arc<T>> {
        self.resolver
            .get_dyn::<T>()
            .map_err(|error| self.missing_provider(core::any::type_name::<T>(), &error))
    }

    /// The resolver behind [`inject`](JobCtx::inject), for a job body that
    /// needs to look something up conditionally.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _ = c.resolver().has::<String>(); }
    /// ```
    #[must_use]
    pub fn resolver(&self) -> &moso_core::Resolver {
        &self.resolver
    }

    /// The message for a dependency the boot check should have caught.
    fn missing_provider(&self, wanted: &'static str, error: &moso_core::Error) -> crate::Error {
        // Type names get long; the diagnostics style guide (docs/04-devex/41)
        // caps a printed type at 80 characters and this is a runtime message
        // held to the same rule.
        let wanted = elide(wanted, 80);
        crate::Error::config(format!(
            "job `{job}` needs `{wanted}` and no provider is registered\n\
             help: add `.provide(..)` for it in the composition root — `App::build()` \
             validates the same graph, so a worker that cannot resolve it means the \
             provider is behind a cargo feature or a profile the worker does not have\n\
             note: {error}",
            job = self.name,
        ))
    }

    /// The queue, so this job can enqueue another.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobCtx, Jobs};
    /// # fn f(c: &JobCtx) { let _: &Jobs = c.jobs(); }
    /// ```
    #[must_use]
    pub fn jobs(&self) -> &Jobs {
        &self.jobs
    }

    /// Extend the lease, for a job that will take longer than its timeout.
    ///
    /// A long job calls this periodically. A worker that dies has its jobs
    /// reclaimed when the lease expires, which is a bounded delay rather than
    /// the fixed timeout a lease-less design would need.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the queue cannot
    /// be reached, and [`Error::permanent`](crate::Error::permanent) when the
    /// lease was already reclaimed — which means another worker is running this
    /// job and this one must stop.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # async fn f(c: &JobCtx) -> moso_jobs::Result { c.heartbeat().await }
    /// ```
    pub async fn heartbeat(&self) -> Result {
        let Some(lease) = self.lease.as_ref() else {
            // An inline drain holds no lease because nothing can steal the job.
            // Succeeding is right: a job body that heartbeats must not need a
            // different code path under `drain()`.
            return Ok(());
        };
        self.jobs
            .queue()
            .heartbeat(lease, self.lease_extension)
            .await
    }

    /// Resolves when the job should stop.
    ///
    /// Fires on worker shutdown and on an operator cancel from the dashboard.
    /// A job that races this against its own work exits cleanly and is retried,
    /// rather than being killed mid-transaction.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # async fn f(c: &JobCtx) { c.cancelled().await; }
    /// ```
    pub async fn cancelled(&self) {
        self.cancelled.cancelled().await;
    }

    /// Whether cancellation has already been signalled.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: bool = c.is_cancelled(); }
    /// ```
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.is_cancelled()
    }

    /// Run `body` at most once for `key`, ever.
    ///
    /// The exactly-once helper. Delivery is at-least-once, so a job that must
    /// not repeat a side effect wraps it in this: a row is claimed under `key`
    /// before the body runs, and a second call with the same key returns the
    /// first call's stored outcome without running anything.
    ///
    /// # What it guarantees, precisely
    ///
    /// The claim and the body are **not** one transaction — `body` is an opaque
    /// future, so there is no transaction to enrol it in. A process killed
    /// between claiming and recording leaves a claim with no outcome, and the
    /// next call **retries the body** after
    /// [`JobCtx::ONCE_ORPHAN_AFTER`]. That is at-least-once for that window and
    /// exactly-once outside it, which is the strongest thing this signature can
    /// promise. A side effect that must be atomic with its claim belongs in a
    /// transaction the caller owns, with a unique constraint.
    ///
    /// Backed by the `moso_job_once` table when a `Db` is registered, and by
    /// the key-value store's compare-and-set otherwise.
    ///
    /// # Errors
    ///
    /// Whatever `body` returns, plus
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the idempotency
    /// table cannot be reached — which is *not* a licence to run the body
    /// again.
    ///
    /// ```no_run
    /// # use moso_jobs::{JobCtx, Result};
    /// # async fn f(ctx: &JobCtx) -> Result {
    /// ctx.once("charge:invoice_42", || async { Ok(()) }).await
    /// # }
    /// ```
    pub async fn once<T, F, Fut>(&self, key: &str, body: F) -> Result<T>
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce() -> Fut + Send,
        Fut: Future<Output = Result<T>> + Send,
    {
        let store = crate::once::Store::from_resolver(&self.resolver)?;
        match store.claim(key, Self::ONCE_ORPHAN_AFTER).await? {
            crate::once::Claim::Recorded(value) => {
                serde_json::from_value(value).map_err(|error| crate::Error::Payload {
                    job: self.name.to_owned(),
                    detail: format!("the recorded outcome of `{key}` no longer decodes: {error}"),
                })
            }
            crate::once::Claim::Mine => {
                let outcome = body().await?;
                let encoded = serde_json::to_value(&outcome)?;
                store.record(key, encoded).await?;
                Ok(outcome)
            }
        }
    }

    /// How long a claim with no recorded outcome is honoured before
    /// [`once`](JobCtx::once) runs the body again.
    ///
    /// Long enough that a slow body is not run twice, short enough that a
    /// process killed mid-body does not block the work for a shift.
    ///
    /// ```
    /// use moso_jobs::JobCtx;
    ///
    /// assert_eq!(JobCtx::ONCE_ORPHAN_AFTER, std::time::Duration::from_secs(3600));
    /// ```
    pub const ONCE_ORPHAN_AFTER: Duration = Duration::from_secs(3600);

    /// The trace context the enqueueing request carried.
    ///
    /// What makes a distributed trace span `HTTP request → job → outbound
    /// call`. Stored on the job row at enqueue time and restored here.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: Option<&str> = c.trace_parent(); }
    /// ```
    #[must_use]
    pub fn trace_parent(&self) -> Option<&str> {
        self.trace_parent.as_deref()
    }

    /// The opaque identity of whoever enqueued this job, when it was attributed.
    ///
    /// Restored from the row the worker leased. It is an **identity for audit**,
    /// not live authority: the job runs as the subject that scheduled it, but a
    /// body that needs to know what that subject may do *now* re-resolves it, so
    /// a permission revoked since enqueue is already gone. Decode it with
    /// `moso-authz`'s `ActorIdentity::from_wire` — this crate keeps it opaque so
    /// it need not depend on an authorization engine.
    ///
    /// ```no_run
    /// # use moso_jobs::JobCtx;
    /// # fn f(c: &JobCtx) { let _: Option<&str> = c.actor_identity(); }
    /// ```
    #[must_use]
    pub fn actor_identity(&self) -> Option<&str> {
        self.actor.as_deref()
    }
}

impl core::fmt::Debug for JobCtx {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("JobCtx")
            .field("id", &self.id)
            .field("name", &self.name)
            .field("attempt", &self.attempt)
            .finish_non_exhaustive()
    }
}

/// Shorten a type name to `limit` characters, keeping the tail.
///
/// The tail is the informative half — `Arc<dyn Mailer>` says more than
/// `alloc::sync::` — and `docs/04-devex/41-diagnostics.md` forbids printing a
/// type longer than 80 characters at all.
pub(crate) fn elide(name: &str, limit: usize) -> String {
    if name.chars().count() <= limit {
        return name.to_owned();
    }
    let tail: String = name
        .chars()
        .rev()
        .take(limit.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{tail}")
}

/// A stable dedup key for a payload with no explicit `unique_key`.
///
/// FNV-1a over the canonical JSON. Not a cryptographic hash and not trying to
/// be: it names a row in one application's own queue, and the failure mode of a
/// collision is one skipped duplicate, not a security hole.
pub(crate) fn payload_fingerprint(name: &str, payload: &serde_json::Value) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut absorb = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    absorb(name.as_bytes());
    absorb(b"\0");
    absorb(payload.to_string().as_bytes());
    format!("{name}:{hash:016x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A v7 identifier sorts by creation time, which is what keeps a queue
    /// table's primary-key index from fragmenting.
    #[test]
    fn identifiers_are_time_ordered_and_round_trip() {
        let first = JobId::new();
        let second = JobId::new();
        assert!(first <= second);
        assert_eq!(JobId::from_uuid(first.as_uuid()), first);
        assert_eq!(first.to_string().parse::<JobId>().unwrap(), first);
        assert!("not-a-uuid".parse::<JobId>().is_err());
    }

    /// Cancellation has to be both awaitable and pollable, and a cancel that
    /// races the await must not be lost — that race is a job that never stops.
    #[tokio::test]
    async fn cancellation_is_never_lost_to_a_race() {
        let cancel = std::sync::Arc::new(Cancellation::default());
        assert!(!cancel.is_cancelled());

        // Already cancelled: `cancelled()` returns without a waker.
        cancel.cancel();
        assert!(cancel.is_cancelled());
        cancel.cancelled().await;

        // A waiter registered before the cancel is still woken.
        let cancel = std::sync::Arc::new(Cancellation::default());
        let waiting = tokio::spawn({
            let cancel = std::sync::Arc::clone(&cancel);
            async move { cancel.cancelled().await }
        });
        tokio::task::yield_now().await;
        cancel.cancel();
        waiting.await.expect("the waiter was woken");
    }

    /// A diagnostic that prints a 200-character type name is a diagnostic
    /// nobody reads; the style guide caps it at 80.
    #[test]
    fn a_long_type_name_is_elided_from_the_tail() {
        assert_eq!(elide("String", 80), "String");
        let long = "a".repeat(120);
        let short = elide(&long, 80);
        assert_eq!(short.chars().count(), 80);
        assert!(short.starts_with('…'));
    }

    /// The fingerprint is what deduplicates a payload with no explicit key, so
    /// two equal payloads must agree and two different ones must not.
    #[test]
    fn the_payload_fingerprint_is_stable_and_discriminating() {
        let one = serde_json::json!({ "user_id": 7 });
        let same = serde_json::json!({ "user_id": 7 });
        let other = serde_json::json!({ "user_id": 8 });

        assert_eq!(
            payload_fingerprint("welcome", &one),
            payload_fingerprint("welcome", &same)
        );
        assert_ne!(
            payload_fingerprint("welcome", &one),
            payload_fingerprint("welcome", &other)
        );
        // Two jobs with the same payload must not dedupe against each other.
        assert_ne!(
            payload_fingerprint("welcome", &one),
            payload_fingerprint("goodbye", &one)
        );
        assert!(payload_fingerprint("welcome", &one).starts_with("welcome:"));
    }

    /// A job that overrides nothing gets the documented defaults, and
    /// `retry_policy` reads them back without the author writing it out.
    #[test]
    fn the_default_retry_policy_is_read_off_the_constants() {
        struct Plain;
        impl Job for Plain {
            type Args = ();
            const NAME: &'static str = "plain";
            async fn run(_args: (), _ctx: JobCtx) -> Result {
                Ok(())
            }
        }

        let policy = Plain::retry_policy();
        assert_eq!(policy.max_attempts(), DEFAULT_RETRIES);
        assert_eq!(policy.backoff(), crate::Backoff::default_exponential());
    }

    /// `BACKOFF` is the ladder that travels on the row, and the function form
    /// follows it without the author writing the delegation out.
    #[test]
    fn the_declared_ladder_drives_both_the_row_and_the_function() {
        struct Fast;
        impl Job for Fast {
            type Args = ();
            const NAME: &'static str = "fast";
            const RETRIES: u32 = 3;
            const BACKOFF: crate::Backoff = crate::Backoff::Linear {
                base: Duration::from_secs(5),
                max: Duration::from_secs(20),
            };
            async fn run(_args: (), _ctx: JobCtx) -> Result {
                Ok(())
            }
        }

        let policy = Fast::retry_policy();
        assert_eq!(policy.max_attempts(), 3);
        assert_eq!(policy.backoff().delay(1), Duration::from_secs(5));
        assert_eq!(policy.backoff().delay(3), Duration::from_secs(15));
        assert_eq!(policy.backoff().delay(9), Duration::from_secs(20));

        assert_eq!(Fast::backoff(2), Duration::from_secs(10));
        assert_eq!(Fast::backoff(2), Fast::BACKOFF.delay(2));
    }
}
