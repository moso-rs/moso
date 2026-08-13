//! What a worker actually does, end to end, against the in-memory queue.
//!
//! Every acceptance criterion in `docs/03-batteries/32-jobs.md` that does not
//! need a database is asserted here: retries and their backoff, the dead-letter
//! queue and its bulk retry, the poison-payload guard, cancellation, graceful
//! shutdown, lease reclamation, queue weighting, backpressure, trace propagation
//! and the inline drain.
//!
//! The in-memory backend is not a mock. It implements the same leases, the same
//! unique keys and the same dead-letter semantics as the durable ones, which is
//! what makes a test written against it worth anything.

#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use moso_jobs::backend::MemoryQueue;
use moso_jobs::{
    DeadLetterQueue, DlqFilter, DrainMode, Error, Job, JobCtx, JobRegistry, Jobs, Priority, Queue,
    QueueWeight, Result, Worker, WorkerId,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// How many times a body has run, and what trace it saw — **per test**.
///
/// A single global would be shared by every test in this binary, and `cargo
/// test` runs them concurrently in one process. So the counter travels through
/// the dependency graph instead, which is also the path a real worker resolves
/// on: the test asserts on effects *and* on the DI wiring at the same time.
#[derive(Debug, Default)]
struct Runs {
    /// How many bodies ran.
    count: AtomicU32,
    /// The `traceparent` the last body saw.
    trace: std::sync::Mutex<Option<String>>,
    /// The enqueueing actor identity the last body saw.
    actor: std::sync::Mutex<Option<String>>,
}

impl Runs {
    /// Record one run, if this job was given a counter.
    fn hit(ctx: &JobCtx) {
        if let Ok(runs) = ctx.inject::<Runs>() {
            runs.count.fetch_add(1, Ordering::SeqCst);
            *runs
                .trace
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                moso_jobs::trace::current().map(|context| context.to_traceparent());
            *runs
                .actor
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                ctx.actor_identity().map(str::to_owned);
        }
    }

    /// How many bodies ran.
    fn count(&self) -> u32 {
        self.count.load(Ordering::SeqCst)
    }

    /// Start counting again.
    fn reset(&self) {
        self.count.store(0, Ordering::SeqCst);
    }

    /// The `traceparent` the last body saw.
    fn trace(&self) -> Option<String> {
        self.trace
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// The enqueueing actor identity the last body saw.
    fn actor(&self) -> Option<String> {
        self.actor
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Payload {
    user_id: u64,
}

/// Always succeeds.
struct Succeeds;
impl Job for Succeeds {
    type Args = Payload;
    const NAME: &'static str = "succeeds";
    async fn run(_args: Payload, ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        let _ = ctx.attempt();
        Ok(())
    }
}

/// Fails with a retryable error, every time.
struct AlwaysRetries;
impl Job for AlwaysRetries {
    type Args = ();
    const NAME: &'static str = "always_retries";
    const RETRIES: u32 = 3;
    const BACKOFF: moso_jobs::Backoff = moso_jobs::Backoff::Immediate;
    async fn run(_args: (), ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Err(Error::retry("the upstream is down"))
    }
}

/// Fails permanently, so it must not spend the retry budget.
struct FailsPermanently;
impl Job for FailsPermanently {
    type Args = ();
    const NAME: &'static str = "fails_permanently";
    const RETRIES: u32 = 25;
    async fn run(_args: (), ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Err(Error::permanent("the row no longer exists"))
    }
}

/// Needs a payload with a `user_id`, so a row without one is poison.
struct NeedsPayload;
impl Job for NeedsPayload {
    type Args = Payload;
    const NAME: &'static str = "needs_payload";
    const RETRIES: u32 = 25;
    async fn run(_args: Payload, ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Ok(())
    }
}

/// Runs until it is cancelled, or for a very long time.
struct RunsForever;
impl Job for RunsForever {
    type Args = ();
    const NAME: &'static str = "runs_forever";
    const TIMEOUT: Duration = Duration::from_secs(600);
    async fn run(_args: (), ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        tokio::select! {
            () = ctx.cancelled() => Err(Error::retry("cancelled")),
            () = tokio::time::sleep(Duration::from_secs(600)) => Ok(()),
        }
    }
}

/// Takes longer than its own timeout.
struct TooSlow;
impl Job for TooSlow {
    type Args = ();
    const NAME: &'static str = "too_slow";
    const TIMEOUT: Duration = Duration::from_millis(50);
    const RETRIES: u32 = 1;
    async fn run(_args: (), ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        tokio::time::sleep(Duration::from_secs(30)).await;
        Ok(())
    }
}

/// Needs a `String` provider, so a drain with no DI graph fails the way a
/// worker would.
struct NeedsInjection;
impl Job for NeedsInjection {
    type Args = ();
    const NAME: &'static str = "needs_injection";
    const RETRIES: u32 = 1;
    async fn run(_args: (), ctx: JobCtx) -> Result {
        let greeting = ctx.inject::<String>()?;
        assert_eq!(greeting.as_str(), "hello");
        Runs::hit(&ctx);
        Ok(())
    }
}

/// A gate a job body waits on, so a test decides when an instance finishes.
///
/// Travels through the dependency graph like [`Runs`] does, for the same
/// reason: every test in this binary shares one process.
#[derive(Debug, Default)]
struct Gate {
    /// How many bodies have started.
    started: AtomicU32,
    /// How many have finished.
    finished: AtomicU32,
    /// Whether a started body may finish.
    open: std::sync::atomic::AtomicBool,
}

impl Gate {
    /// Block in the body until the test opens the gate.
    async fn wait(ctx: &JobCtx) -> Result {
        let gate = ctx.inject::<Gate>()?;
        gate.started.fetch_add(1, Ordering::SeqCst);
        while !gate.open.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        gate.finished.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    /// Let every waiting body through.
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    /// How many bodies have started.
    fn started(&self) -> u32 {
        self.started.load(Ordering::SeqCst)
    }

    /// How many have finished.
    fn finished(&self) -> u32 {
        self.finished.load(Ordering::SeqCst)
    }

    /// Spin until `predicate` holds, or give up after two seconds.
    async fn settle(&self, predicate: impl Fn(&Self) -> bool) {
        for _ in 0..400 {
            if predicate(self) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }
}

/// `Job::SERIAL`: one instance at a time across the whole fleet.
struct Serialised;
impl Job for Serialised {
    type Args = u64;
    const NAME: &'static str = "serialised";
    const QUEUE: &'static str = "serial";
    const SERIAL: bool = true;
    async fn run(_args: u64, ctx: JobCtx) -> Result {
        Gate::wait(&ctx).await
    }
}

/// The control: the same body without `SERIAL`, which must run in parallel.
struct Concurrent;
impl Job for Concurrent {
    type Args = u64;
    const NAME: &'static str = "concurrent";
    const QUEUE: &'static str = "serial";
    async fn run(_args: u64, ctx: JobCtx) -> Result {
        Gate::wait(&ctx).await
    }
}

/// Bulk work, so backpressure has something low-priority to refuse.
struct Bulk;
impl Job for Bulk {
    type Args = ();
    const NAME: &'static str = "bulk";
    const QUEUE: &'static str = "bulk";
    const PRIORITY: Priority = Priority::Low;
    async fn run(_args: (), _ctx: JobCtx) -> Result {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

fn registry() -> JobRegistry {
    JobRegistry::new()
        .register::<Succeeds>()
        .register::<AlwaysRetries>()
        .register::<FailsPermanently>()
        .register::<NeedsPayload>()
        .register::<RunsForever>()
        .register::<TooSlow>()
        .register::<NeedsInjection>()
        .register::<Bulk>()
}

/// The serialisation tests get their own registry: adding a third queue to the
/// shared one would change how every other test's worker divides its slots.
fn serial_registry() -> JobRegistry {
    JobRegistry::new()
        .register::<Serialised>()
        .register::<Concurrent>()
}

/// A harness whose graph also carries a [`Gate`], for the serialisation tests.
fn gated_harness() -> (Jobs, Arc<MemoryQueue>, Arc<Gate>) {
    let queue = Arc::new(MemoryQueue::new());
    let gate = Arc::new(Gate::default());

    let mut providers = moso_core::di::ProviderMapBuilder::new();
    providers.insert_arc(Arc::clone(&gate));

    let jobs = Jobs::new(
        Arc::clone(&queue) as Arc<dyn Queue>,
        Arc::new(serial_registry()),
    )
    .with_resolver(moso_core::Resolver::new(providers.build()));
    (jobs, queue, gate)
}

fn harness() -> (Jobs, Arc<MemoryQueue>, Arc<Runs>) {
    harness_with(None)
}

/// A harness whose dependency graph also carries `extra`, for the one test that
/// needs a second provider.
fn harness_with(extra: Option<String>) -> (Jobs, Arc<MemoryQueue>, Arc<Runs>) {
    let queue = Arc::new(MemoryQueue::new());
    let runs = Arc::new(Runs::default());

    let mut providers = moso_core::di::ProviderMapBuilder::new();
    providers.insert_arc(Arc::clone(&runs));
    if let Some(value) = extra {
        providers.insert(value);
    }

    let jobs = Jobs::new(Arc::clone(&queue) as Arc<dyn Queue>, Arc::new(registry()))
        .with_dead_letters(Arc::clone(&queue) as Arc<dyn DeadLetterQueue>)
        .with_resolver(moso_core::Resolver::new(providers.build()));
    (jobs, queue, runs)
}

/// Run everything that is ready, however many rounds it takes.
async fn drain(worker: &Worker) -> u64 {
    let mut total = 0;
    for _ in 0..64 {
        let ran = worker.run_once().await.expect("a batch");
        total += ran;
        if ran == 0 {
            return total;
        }
    }
    total
}

fn worker(jobs: &Jobs) -> Worker {
    Worker::new(jobs.clone(), jobs.shared_registry())
        .with_id(WorkerId::new("test-worker"))
        .concurrency(4)
        .lease(Duration::from_secs(30))
        .poll(Duration::from_millis(10))
}

// ---------------------------------------------------------------------------
// The happy path
// ---------------------------------------------------------------------------

/// The baseline: a job enqueued is a job run, exactly once, and acknowledged.
#[tokio::test]
async fn a_job_runs_once_and_is_acknowledged() {
    let (jobs, queue, runs) = harness();
    Succeeds::enqueue(&jobs, Payload { user_id: 7 })
        .await
        .expect("enqueued");

    let ran = worker(&jobs).run_once().await.expect("a batch ran");
    assert_eq!(ran, 1);
    assert_eq!(runs.count(), 1);
    assert_eq!(queue.all()[0].state, moso_jobs::JobState::Done);

    // Nothing is left to run.
    assert_eq!(worker(&jobs).run_once().await.expect("empty"), 0);
    assert_eq!(runs.count(), 1);
}

/// Acceptance criterion 8: `drain()` runs jobs with the same DI graph a real
/// worker uses — so a job that only works because the worker happened to have a
/// provider fails here too.
#[tokio::test]
async fn drain_uses_the_same_dependency_graph_a_worker_does() {
    // Without the `String` provider, the job fails with the message
    // `JobCtx::inject` documents rather than panicking.
    let (jobs, _queue, runs) = harness();
    NeedsInjection::enqueue(&jobs, ()).await.expect("enqueued");
    assert_eq!(jobs.drain().await.expect("drained"), 1);
    assert_eq!(runs.count(), 0, "the body never got past inject");

    // With it, the same job succeeds — same code path, same graph.
    let (wired, _queue, wired_runs) = harness_with(Some("hello".to_owned()));
    NeedsInjection::enqueue(&wired, ()).await.expect("enqueued");
    assert_eq!(wired.drain().await.expect("drained"), 1);
    assert_eq!(wired_runs.count(), 1);
}

/// `drain()` keeps going until the queue is empty, including jobs a job
/// enqueued.
#[tokio::test]
async fn drain_runs_until_the_queue_is_empty() {
    let (jobs, _queue, runs) = harness();
    for id in 0..5 {
        Succeeds::enqueue(&jobs, Payload { user_id: id })
            .await
            .expect("enqueued");
    }
    assert_eq!(jobs.drain().await.expect("drained"), 5);
    assert_eq!(runs.count(), 5);
}

// ---------------------------------------------------------------------------
// Retries and the dead-letter queue
// ---------------------------------------------------------------------------

/// Acceptance criterion 6, first half: a retryable failure is retried up to the
/// budget and then dead-lettered with its payload.
#[tokio::test]
async fn a_retryable_failure_spends_its_budget_and_then_dies() {
    let (jobs, queue, runs) = harness();
    AlwaysRetries::enqueue(&jobs, ()).await.expect("enqueued");

    let worker = worker(&jobs);
    // `RETRIES = 3` with an immediate backoff: three attempts, then the
    // dead-letter queue.
    for expected in 1..=3 {
        assert_eq!(worker.run_once().await.expect("a batch"), 1);
        assert_eq!(runs.count(), expected);
    }
    assert_eq!(worker.run_once().await.expect("nothing left"), 0);
    assert_eq!(runs.count(), 3, "the budget is three attempts");

    let stats = DeadLetterQueue::stats(queue.as_ref())
        .await
        .expect("dead letters");
    assert_eq!(stats.total, 1);
    assert_eq!(stats.by_job, vec![("always_retries".to_owned(), 1)]);

    let (letters, _) = queue
        .list(&DlqFilter::new(), None, 10)
        .await
        .expect("listed");
    assert_eq!(letters.len(), 1);
    assert!(letters[0].last_error.contains("the upstream is down"));
    assert_eq!(letters[0].attempts, 3);
}

/// Acceptance criterion 6, second half: a non-retryable error skips the budget
/// and goes straight to the dead-letter queue.
#[tokio::test]
async fn a_permanent_failure_skips_to_the_dead_letter_queue() {
    let (jobs, queue, runs) = harness();
    FailsPermanently::enqueue(&jobs, ())
        .await
        .expect("enqueued");

    assert_eq!(drain(&worker(&jobs)).await, 1);
    assert_eq!(
        runs.count(),
        1,
        "a permanent failure is attempted once, not twenty-five times"
    );

    let stats = DeadLetterQueue::stats(queue.as_ref()).await.expect("stats");
    assert_eq!(stats.total, 1);
}

/// The poison-payload guard: a row that does not deserialise goes straight to
/// the dead-letter queue rather than turning 40,000 rows into a million failed
/// attempts.
#[tokio::test]
async fn a_poison_payload_never_reaches_the_body() {
    let (jobs, queue, runs) = harness();

    // Push a row by hand with the wrong payload shape — which is exactly what a
    // deploy that renamed a field produces.
    queue
        .push(moso_jobs::QueuedJob::new(
            "needs_payload",
            "default",
            serde_json::json!({ "wrong": "shape" }),
        ))
        .await
        .expect("pushed");

    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);
    assert_eq!(runs.count(), 0, "the body never ran");

    let (letters, _) = queue
        .list(&DlqFilter::new(), None, 10)
        .await
        .expect("listed");
    assert_eq!(letters.len(), 1);
    assert!(letters[0].last_error.contains("did not deserialise"));
    // The payload is kept, so the job can be retried after the deploy that
    // fixes it.
    assert_eq!(letters[0].payload, serde_json::json!({ "wrong": "shape" }));
}

/// A job that exceeds its timeout is cancelled and retried, and the timeout
/// counts against the budget so a permanently slow job does end up dead.
#[tokio::test]
async fn a_job_past_its_timeout_is_cancelled_and_dead_lettered() {
    let (jobs, queue, runs) = harness();
    TooSlow::enqueue(&jobs, ()).await.expect("enqueued");

    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);
    assert_eq!(runs.count(), 1);

    let (letters, _) = queue
        .list(&DlqFilter::new(), None, 10)
        .await
        .expect("listed");
    assert_eq!(letters.len(), 1, "RETRIES = 1 means one attempt");
    assert!(
        letters[0].last_error.contains("exceeded its"),
        "{:?}",
        letters[0].last_error
    );
}

/// Bulk retry after a fix: the payload was kept, so the work still gets done.
#[tokio::test]
async fn the_dead_letter_queue_retries_in_bulk() {
    let (jobs, queue, runs) = harness();
    for _ in 0..3 {
        FailsPermanently::enqueue(&jobs, ())
            .await
            .expect("enqueued");
    }
    // Each enqueue gets its own row: `FailsPermanently` declares no dedup
    // window, so three enqueues are three jobs.
    let worker = worker(&jobs);
    drain(&worker).await;
    assert_eq!(
        DeadLetterQueue::stats(queue.as_ref())
            .await
            .expect("stats")
            .total,
        3
    );

    // A filter that matches nothing retries nothing.
    assert_eq!(
        queue
            .retry(&DlqFilter::new().job("something_else"), 100)
            .await
            .expect("filtered"),
        0
    );

    // The limit is mandatory, and it is honoured.
    assert_eq!(
        queue
            .retry(&DlqFilter::new().job("fails_permanently"), 2)
            .await
            .expect("retried"),
        2
    );
    assert_eq!(
        DeadLetterQueue::stats(queue.as_ref())
            .await
            .expect("stats")
            .total,
        1
    );

    // The retried rows run again — and fail again, which is the honest outcome
    // for a job whose fix was never deployed.
    runs.reset();
    drain(&worker).await;
    assert_eq!(runs.count(), 2);
}

/// Discarding is the other half of the operator's toolkit, and it must also
/// respect the filter and the limit.
#[tokio::test]
async fn the_dead_letter_queue_discards_selectively() {
    let (jobs, queue, _runs) = harness();
    FailsPermanently::enqueue(&jobs, ())
        .await
        .expect("enqueued");
    worker(&jobs).run_once().await.expect("a batch");

    assert_eq!(
        queue
            .discard(&DlqFilter::new().error_contains("nothing like this"), 10)
            .await
            .expect("filtered"),
        0
    );
    assert_eq!(
        queue
            .discard(&DlqFilter::new().error_contains("no longer exists"), 10)
            .await
            .expect("discarded"),
        1
    );
    assert_eq!(
        DeadLetterQueue::stats(queue.as_ref())
            .await
            .expect("stats")
            .total,
        0
    );
}

// ---------------------------------------------------------------------------
// Leases, cancellation and shutdown
// ---------------------------------------------------------------------------

/// Acceptance criterion 4: a worker killed mid-job has the job reclaimed after
/// the lease and run again — exactly once more, not twice.
#[tokio::test]
async fn a_dead_workers_job_is_reclaimed_after_its_lease() {
    let (jobs, queue, runs) = harness();
    Succeeds::enqueue(&jobs, Payload { user_id: 1 })
        .await
        .expect("enqueued");

    // Lease it as a worker would, and then walk away — which is what a `SIGKILL`
    // between the pull and the ack looks like from the queue's side.
    let leased = queue
        .pull(
            &["default".to_owned()],
            10,
            Duration::from_secs(30),
            WorkerId::new("doomed"),
        )
        .await
        .expect("leased");
    assert_eq!(leased.len(), 1);

    // Nothing else can take it while the lease holds.
    assert_eq!(worker(&jobs).run_once().await.expect("nothing"), 0);
    assert_eq!(
        queue.reclaim(&["default".to_owned()]).await.expect("swept"),
        0,
        "a live lease is not reclaimed"
    );

    // Past the lease, it comes back — and runs exactly once.
    queue.advance(Duration::from_secs(31));
    assert_eq!(
        queue.reclaim(&["default".to_owned()]).await.expect("swept"),
        1
    );
    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);
    assert_eq!(runs.count(), 1);

    // And the dead worker's acknowledgement is refused, so it cannot complete a
    // job another worker is now running.
    let (_, stale) = leased.into_iter().next().expect("one job");
    let refused = queue.ack(stale).await;
    assert!(refused.is_err(), "a reclaimed lease must not acknowledge");
}

/// Cancellation is a future the body races, and a cancelled job is retried
/// rather than acknowledged.
#[tokio::test]
async fn a_cancelled_job_stops_and_is_not_acknowledged() {
    let (jobs, queue, runs) = harness();
    let id = RunsForever::enqueue(&jobs, ()).await.expect("enqueued");

    // A one-second lease means the automatic heartbeat runs three times a
    // second, and a heartbeat refused by `cancel` is what stops the body.
    let worker = worker(&jobs).lease(Duration::from_secs(1));
    let running = tokio::spawn(async move { worker.run_once().await });

    // Wait for the body to actually start before cancelling it.
    for _ in 0..200 {
        if runs.count() > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(runs.count(), 1, "the body started");

    assert!(jobs.cancel(id).await.expect("cancellable"), "cancelled");

    let ran = tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the worker stopped")
        .expect("the task finished")
        .expect("a batch");
    assert_eq!(ran, 1);

    let row = queue
        .all()
        .into_iter()
        .find(|job| job.id == id)
        .expect("the row is still there");
    assert_eq!(
        row.state,
        moso_jobs::JobState::Cancelled,
        "a cancelled job is not `Done`"
    );
}

/// Graceful shutdown: stop fetching, finish what is in flight, and leave inside
/// the grace period.
#[tokio::test]
async fn a_worker_stops_fetching_and_drains_within_its_grace() {
    let (jobs, _queue, runs) = harness();
    for id in 0..3 {
        Succeeds::enqueue(&jobs, Payload { user_id: id })
            .await
            .expect("enqueued");
    }

    let shutdown = moso_core::Signal::new();
    let worker = worker(&jobs)
        .grace(Duration::from_secs(5))
        .drain_mode(DrainMode::Requeue);

    let running = tokio::spawn({
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    for _ in 0..400 {
        if runs.count() == 3 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(runs.count(), 3, "everything ran");

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the worker left within the grace")
        .expect("the task finished")
        .expect("a clean stop");
}

/// A worker that cannot reach its queue fails at startup rather than logging a
/// connection error every second for the lifetime of the pod.
#[tokio::test]
async fn a_worker_whose_queue_is_unreachable_fails_at_startup() {
    struct Unreachable;
    impl Queue for Unreachable {
        fn name(&self) -> &'static str {
            "unreachable"
        }
        fn capabilities(&self) -> moso_jobs::QueueCapabilities {
            moso_jobs::QueueCapabilities::minimal()
        }
        fn push<'a>(&'a self, _job: moso_jobs::QueuedJob) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn pull<'a>(
            &'a self,
            _queues: &'a [String],
            _limit: u32,
            _lease: Duration,
            _worker: WorkerId,
        ) -> moso_core::BoxFuture<'a, Result<Vec<(moso_jobs::QueuedJob, moso_jobs::Lease)>>>
        {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn ack<'a>(&'a self, _lease: moso_jobs::Lease) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn nack<'a>(
            &'a self,
            _lease: moso_jobs::Lease,
            _error: &'a str,
            _run_at: Option<chrono::DateTime<chrono::Utc>>,
        ) -> moso_core::BoxFuture<'a, Result> {
            Box::pin(async { Ok(()) })
        }
        fn heartbeat<'a>(
            &'a self,
            _lease: &'a moso_jobs::Lease,
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
        ) -> moso_core::BoxFuture<'a, Result<Vec<moso_jobs::QueueStats>>> {
            Box::pin(async { Ok(Vec::new()) })
        }
        fn probe(&self) -> moso_core::BoxFuture<'_, Result> {
            Box::pin(async { Err(Error::unavailable("unreachable", "connection refused")) })
        }
    }

    // A registry with no serial job: a backend built from
    // `QueueCapabilities::minimal()` cannot serialise, and this test is about
    // the probe rather than about that.
    let jobs = Jobs::new(
        Arc::new(Unreachable),
        Arc::new(JobRegistry::new().register::<Succeeds>()),
    );
    let registry = jobs.shared_registry();
    let error = Worker::new(jobs, registry)
        .run(moso_core::Signal::new())
        .await
        .expect_err("the probe failed");
    assert!(error.retryable());
    assert!(error.to_string().contains("connection refused"));
}

// ---------------------------------------------------------------------------
// Scheduling fairness and backpressure
// ---------------------------------------------------------------------------

/// A saturated queue must not starve another one. With one slot per round and
/// three queues, each of them leads one round in three.
#[tokio::test]
async fn a_saturated_queue_does_not_starve_the_others() {
    let (jobs, _queue, runs) = harness();

    // Fifty bulk jobs and one urgent one.
    for id in 0..50 {
        Bulk::enqueue(&jobs, ())
            .unique_key(format!("bulk:{id}"))
            .spawn()
            .await
            .expect("enqueued");
    }
    Succeeds::enqueue(&jobs, Payload { user_id: 1 })
        .await
        .expect("enqueued");

    // One slot per round, so the only way `default` ever runs is the rotation.
    let worker = Worker::new(jobs.clone(), jobs.shared_registry())
        .concurrency(1)
        .weighted_queues([QueueWeight::new("bulk", 1), QueueWeight::new("default", 1)]);

    for _ in 0..6 {
        worker.run_once().await.expect("a batch");
        if runs.count() > 0 {
            break;
        }
    }
    assert_eq!(
        runs.count(),
        1,
        "the `default` job ran despite fifty bulk jobs ahead of it"
    );
}

/// Backpressure refuses *low-priority* enqueues for a queue that is too deep,
/// and says so as a metric rather than silently.
///
/// The gate is on the enqueue and not on the pull, because a worker that stops
/// pulling a deep queue makes it deeper.
#[tokio::test]
async fn backpressure_refuses_low_priority_work_and_says_so() {
    let (jobs, _queue, _runs) = harness();

    // Below the threshold, everything is accepted.
    assert!(!jobs.backpressure_active("bulk"));
    Bulk::enqueue(&jobs, ()).await.expect("accepted");

    // Run a worker with a threshold of zero until it marks the queue, which is
    // what its maintenance tick does with real depth.
    let shutdown = moso_core::Signal::new();
    let marking = tokio::spawn({
        let marker = worker(&jobs)
            .backpressure(Some(0))
            .reclaim_interval(Duration::from_millis(1))
            .poll(Duration::from_millis(5));
        let shutdown = shutdown.clone();
        async move { marker.run(shutdown).await }
    });
    for _ in 0..400 {
        if jobs.backpressure_active("bulk") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    shutdown.trigger();
    let _ = tokio::time::timeout(Duration::from_secs(5), marking).await;

    assert!(
        jobs.backpressure_active("bulk"),
        "the worker marked the queue"
    );

    let error = Bulk::enqueue(&jobs, ())
        .await
        .expect_err("low priority is refused");
    assert!(error.to_string().contains("backpressure"), "{error}");
    assert!(error.retryable(), "the caller is meant to come back");

    // Normal-priority work on the same queue still goes through: backpressure
    // sheds bulk, not everything.
    Bulk::enqueue(&jobs, ())
        .priority(Priority::Normal)
        .unique_key("not-bulk")
        .spawn()
        .await
        .expect("normal priority is accepted");

    // And the state is a metric an operator can alert on.
    assert!(
        moso_jobs::metrics::snapshot().contains("moso_jobs_backpressure_active"),
        "the metric is emitted"
    );
}

// ---------------------------------------------------------------------------
// Serial jobs
// ---------------------------------------------------------------------------

/// `Job::SERIAL`, which nothing used to read: two instances are enqueued and
/// the second must not start until the first has finished.
///
/// Serialisation is per **job type**, not per payload — the two rows below
/// carry different arguments and still run one after the other.
#[tokio::test]
async fn a_serial_job_runs_its_second_instance_only_after_the_first_finishes() {
    let (jobs, queue, gate) = gated_harness();
    Serialised::enqueue(&jobs, 1).await.expect("enqueued");
    Serialised::enqueue(&jobs, 2).await.expect("enqueued");
    assert_eq!(queue.all().len(), 2, "two rows, two different payloads");

    // Four slots and a worker that keeps pulling, so nothing but the serial
    // guard can be what stops the second instance.
    let shutdown = moso_core::Signal::new();
    let running = tokio::spawn({
        let worker = worker(&jobs).concurrency(4).queues(["serial"]);
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    gate.settle(|gate| gate.started() > 0).await;
    assert_eq!(gate.started(), 1, "the first instance started");

    // Long enough for a 10ms-poll worker to have asked for work many times.
    tokio::time::sleep(Duration::from_millis(120)).await;
    assert_eq!(
        gate.started(),
        1,
        "the second instance must not start while the first holds the chain"
    );
    assert_eq!(
        queue
            .all()
            .iter()
            .filter(|row| row.state == moso_jobs::JobState::Running)
            .count(),
        1,
        "and only one row is leased"
    );

    gate.open();
    gate.settle(|gate| gate.finished() == 2).await;
    assert_eq!(gate.finished(), 2, "both ran, one after the other");

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the worker stopped")
        .expect("the task finished")
        .expect("a clean stop");
}

/// The control, so the test above is not just measuring a concurrency of one:
/// the same body without `SERIAL` runs both instances at once.
#[tokio::test]
async fn a_job_that_is_not_serial_runs_its_instances_side_by_side() {
    let (jobs, _queue, gate) = gated_harness();
    Concurrent::enqueue(&jobs, 1).await.expect("enqueued");
    Concurrent::enqueue(&jobs, 2).await.expect("enqueued");

    let shutdown = moso_core::Signal::new();
    let running = tokio::spawn({
        let worker = worker(&jobs).concurrency(4).queues(["serial"]);
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    gate.settle(|gate| gate.started() == 2).await;
    assert_eq!(gate.started(), 2, "both started before either finished");

    gate.open();
    gate.settle(|gate| gate.finished() == 2).await;
    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("the worker stopped")
        .expect("the task finished")
        .expect("a clean stop");
}

/// A worker that dies mid-serial-job must not block the chain forever: the
/// lease *is* the claim, so it frees itself exactly when the lease expires.
#[tokio::test]
async fn a_dead_workers_serial_lease_frees_the_chain_when_it_expires() {
    let (jobs, queue, _gate) = gated_harness();
    Serialised::enqueue(&jobs, 1).await.expect("enqueued");
    Serialised::enqueue(&jobs, 2).await.expect("enqueued");

    // Lease one and walk away, which is what a `SIGKILL` looks like from here.
    let leased = queue
        .pull(
            &["serial".to_owned()],
            10,
            Duration::from_secs(30),
            WorkerId::new("doomed"),
        )
        .await
        .expect("leased");
    assert_eq!(leased.len(), 1, "the serial guard held the second row back");

    let blocked = queue
        .pull(
            &["serial".to_owned()],
            10,
            Duration::from_secs(30),
            WorkerId::new("other"),
        )
        .await
        .expect("nothing");
    assert!(blocked.is_empty(), "and it holds it back for every worker");

    queue.advance(Duration::from_secs(31));
    let freed = queue
        .pull(
            &["serial".to_owned()],
            10,
            Duration::from_secs(30),
            WorkerId::new("other"),
        )
        .await
        .expect("leased");
    assert_eq!(freed.len(), 1, "an expired lease frees the chain");
}

// ---------------------------------------------------------------------------
// The boot check
// ---------------------------------------------------------------------------

/// Nothing used to call `JobRegistry::validate()`. A worker does now, and it
/// reports **every** problem in one pass rather than one per restart.
#[tokio::test]
async fn a_worker_refuses_to_start_on_a_registry_it_cannot_honour() {
    /// A second job claiming a registered wire name.
    struct Impostor;
    impl Job for Impostor {
        type Args = Payload;
        const NAME: &'static str = "succeeds";
        async fn run(_args: Payload, _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    let queue = Arc::new(MemoryQueue::new());
    let jobs = Jobs::new(
        Arc::clone(&queue) as Arc<dyn Queue>,
        Arc::new(registry().register::<Impostor>()),
    );
    let registry = jobs.shared_registry();
    let worker = Worker::new(jobs, registry).queues(["default", "nobody-writes-here"]);

    let errors = worker.validate();
    assert_eq!(errors.len(), 2, "{}", errors.render(false));

    let error = worker
        .run(moso_core::Signal::new())
        .await
        .expect_err("the registry is broken");
    let rendered = error.to_string();
    assert!(rendered.contains("(2 problems)"), "{rendered}");
    assert!(
        rendered.contains("share the wire name `succeeds`"),
        "{rendered}"
    );
    assert!(rendered.contains("nobody-writes-here"), "{rendered}");
    // Every problem carries the line that fixes it, exactly as `App::build()`
    // reports its own.
    assert!(rendered.contains("fix"), "{rendered}");
    assert!(!error.retryable(), "a boot problem is not worth retrying");
}

/// And a registry with nothing wrong starts, so the check is not a wall.
#[tokio::test]
async fn a_worker_over_a_sound_registry_starts() {
    let (jobs, _queue, _runs) = harness();
    assert!(worker(&jobs).validate().is_empty());
}

// ---------------------------------------------------------------------------
// Trace propagation
// ---------------------------------------------------------------------------

/// Acceptance criterion 7: the trace context travels `request → job → outbound
/// call`, asserted on span parentage rather than on a log line.
#[tokio::test]
async fn a_trace_spans_the_request_the_job_and_the_outbound_call() {
    use moso_jobs::trace::{self, TraceContext};

    let (jobs, _queue, runs) = harness();

    // Hop 1: the request. Enqueue inside its scope, exactly as a handler would.
    let request = TraceContext::root();
    trace::scope(request, async {
        Succeeds::enqueue(&jobs, Payload { user_id: 1 })
            .await
            .expect("enqueued");
    })
    .await;

    // Hop 2: the job. The worker restores the row's context and makes a child.
    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);

    let inside = runs.trace().expect("the job body saw a trace context");
    let job = TraceContext::parse(&inside).expect("a valid header");

    assert_eq!(
        job.trace_id(),
        request.trace_id(),
        "the job runs inside the request's trace"
    );
    assert_ne!(
        job.span_id_hex(),
        request.span_id_hex(),
        "and as its own span"
    );

    // Hop 3: an outbound call from inside the job.
    let outbound = job.child();
    assert_eq!(outbound.trace_id(), request.trace_id());
    assert_eq!(
        outbound.parent_span_id_hex().as_deref(),
        Some(job.span_id_hex().as_str()),
        "the outbound call's parent is the job"
    );
}

/// A job enqueued with no ambient trace still gets one, because a job with no
/// trace at all is harder to debug than one whose trace begins late.
#[tokio::test]
async fn a_job_enqueued_outside_a_trace_still_gets_one() {
    let (jobs, _queue, runs) = harness();
    Succeeds::enqueue(&jobs, Payload { user_id: 1 })
        .await
        .expect("enqueued");
    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);

    assert!(runs.trace().is_some(), "the job still ran inside a trace");
}

// ---------------------------------------------------------------------------
// Actor propagation
// ---------------------------------------------------------------------------

/// A job enqueued under a known actor, when a worker runs it, exposes that
/// actor's identity to the body — the whole point of carrying it on the row. The
/// identity is `moso-authz`'s opaque `ActorIdentity::to_wire` string; a literal
/// stands in for it here so the test needs no dependency on the authz crate.
#[tokio::test]
async fn a_job_runs_with_the_identity_of_whoever_enqueued_it() {
    use moso_jobs::actor;

    let (jobs, _queue, runs) = harness();

    // Enqueue inside the enqueuer's identity scope, exactly as request
    // middleware that resolved an `Actor` would.
    actor::scope("usr_alice".to_owned(), async {
        Succeeds::enqueue(&jobs, Payload { user_id: 1 })
            .await
            .expect("enqueued");
    })
    .await;

    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);

    assert_eq!(
        runs.actor().as_deref(),
        Some("usr_alice"),
        "the job body sees the enqueueing actor, not an empty identity",
    );
}

/// A job enqueued with no ambient actor runs unattributed rather than being
/// pinned on some invented subject — an honest gap, not a wrong answer.
#[tokio::test]
async fn a_job_enqueued_without_an_actor_runs_unattributed() {
    let (jobs, _queue, runs) = harness();
    Succeeds::enqueue(&jobs, Payload { user_id: 1 })
        .await
        .expect("enqueued");
    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);

    assert!(
        runs.actor().is_none(),
        "no enqueueing actor means the job is unattributed",
    );
}

// ---------------------------------------------------------------------------
// Metrics
// ---------------------------------------------------------------------------

/// A worker that counts nothing is a worker nobody can alert on.
#[tokio::test]
async fn a_run_moves_the_documented_counters() {
    let (jobs, _queue, _runs) = harness();
    Succeeds::enqueue(&jobs, Payload { user_id: 1 })
        .await
        .expect("enqueued");
    AlwaysRetries::enqueue(&jobs, ()).await.expect("enqueued");
    worker(&jobs).run_once().await.expect("a batch");

    // The registry is per process and every test in this binary writes to it,
    // so the assertion is on the *series* rather than on a count that another
    // test could have moved.
    let text = moso_jobs::metrics::snapshot();
    assert!(
        text.contains(r#"moso_jobs_enqueued_total{job="succeeds",queue="default"}"#),
        "{text}"
    );
    assert!(
        text.contains(r#"moso_jobs_duration_seconds_count{job="succeeds",status="success"}"#),
        "{text}"
    );
    assert!(
        text.contains(r#"moso_jobs_retries_total{job="always_retries",reason="failed"}"#),
        "{text}"
    );
    assert!(text.contains("moso_jobs_latency_seconds_count"), "{text}");
}
