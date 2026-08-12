//! The Redis queue, against a real Redis.
//!
//! `RedisQueue` is written against [`KvStore`](moso_kv::KvStore) and its
//! in-memory sibling exercises most of the code, but three things only a real
//! Redis proves: that the sorted-set claim (`ZREM` returning 1 to exactly one
//! caller) actually serialises a race across connections, that the serial slot
//! (`SET … NX` with the lease as its TTL) holds a chain to one running instance
//! fleet-wide, and that a job survives the round trip through `fred` and back.
//! Until this file existed the `serial_chains: true` capability that
//! [`Worker::validate`](moso_jobs::Worker) trusts was entirely unexecuted.
//!
//! Every test here **skips with a message** rather than failing when `REDIS_URL`
//! is unset, mirroring the `databases()` helper in `sql_backend.rs`: the macOS
//! CI leg runs the whole suite with it deliberately unset, so a test that failed
//! without Redis would be a broken test rather than a caught regression.
//!
//! ```text
//! eval "$(./scripts/test-db.sh env)"   # exports REDIS_URL (and DATABASE_URL)
//! cargo nextest run -p moso-jobs --features jobs-redis
//! ```
//!
//! The whole file is behind `#[cfg(feature = "jobs-redis")]`, because
//! `backend::RedisQueue` does not exist without it. Each test uses a key prefix
//! nobody else uses, so concurrent tests and re-runs never see each other's
//! keys.

#![cfg(feature = "jobs-redis")]
#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use moso_jobs::backend::RedisQueue;
use moso_jobs::{DeadLetterQueue, Job, JobCtx, JobRegistry, Jobs, Queue, Result, Worker, WorkerId};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// How many bodies ran, **per test**.
///
/// A single global counter would be shared by every test in this binary, and
/// `cargo test` runs them concurrently in one process — so the counter travels
/// through the dependency graph instead, which is also the DI path a real
/// worker resolves on.
#[derive(Debug, Default)]
struct Runs(AtomicU32);

impl Runs {
    fn hit(ctx: &JobCtx) {
        if let Ok(runs) = ctx.inject::<Runs>() {
            runs.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn count(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A gate a serial body waits on, so a test decides when the first instance of
/// a chain finishes and can observe whether the second started early.
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
    open: AtomicBool,
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

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Payload {
    user_id: u64,
}

/// A plain, non-serial job with a deduplication window.
struct SendWelcome;
impl Job for SendWelcome {
    type Args = Payload;
    const NAME: &'static str = "send_welcome_email";
    const QUEUE: &'static str = "mail";
    const UNIQUE_FOR: Option<Duration> = Some(Duration::from_secs(600));
    async fn run(_args: Payload, ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Ok(())
    }
}

/// A serial job whose body waits on the [`Gate`], so a test can hold the chain's
/// first instance open and watch the second stay blocked.
struct SerialChain;
impl Job for SerialChain {
    type Args = u64;
    const NAME: &'static str = "serial_chain";
    const QUEUE: &'static str = "serial";
    const SERIAL: bool = true;
    async fn run(_args: u64, ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Gate::wait(&ctx).await
    }
}

fn registry() -> Arc<JobRegistry> {
    Arc::new(
        JobRegistry::new()
            .register::<SendWelcome>()
            .register::<SerialChain>(),
    )
}

/// The Redis this run can reach, or `None` with a skip message.
///
/// `REDIS_URL` set: probe it once and **panic loudly** if it is unreachable, so
/// a misconfigured URL is a clear failure rather than a silent skip — exactly as
/// `databases()` panics on an unusable `DATABASE_URL`. `REDIS_URL` unset: say so
/// once and let the caller return, because a suite that fails on a laptop with
/// no Redis is a suite people stop running.
async fn redis(tag: &str) -> Option<String> {
    match std::env::var("REDIS_URL") {
        Ok(url) if !url.trim().is_empty() => {
            // A throwaway queue on its own prefix, purely to answer "is anyone
            // home" before a test writes anything.
            let probe =
                RedisQueue::new(url.clone()).prefix(format!("moso-probe-{}", std::process::id()));
            match Queue::probe(&probe).await {
                Ok(()) => Some(url),
                Err(error) => panic!(
                    "REDIS_URL is set to `{url}` and the connection failed: {error}\n\
                     unset it to skip the Redis suite"
                ),
            }
        }
        _ => {
            eprintln!(
                "skipping `{tag}`: REDIS_URL is not set.\n\
                 run `eval \"$(./scripts/test-db.sh env)\"` and \
                 `cargo nextest run -p moso-jobs --features jobs-redis` for the Redis suite"
            );
            None
        }
    }
}

/// A queue on a key prefix nobody else in this suite uses.
///
/// The prefix is `[a-z0-9_-]+` — the only alphabet `moso-kv` accepts for a
/// namespace — so the tag, the process id and a nanosecond clock keep two
/// dialects, two runs and two tests apart.
fn queue(url: &str, tag: &str) -> Arc<RedisQueue> {
    let prefix = format!(
        "t-{tag}-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    Arc::new(RedisQueue::new(url.to_owned()).prefix(prefix))
}

/// A handle over `queue`, wired with the same dead-letter view, resolver and
/// `Runs`/`Gate` providers a real worker resolves against.
///
/// Building the handle is also what teaches the backend which job names are
/// [`SERIAL`](moso_jobs::Job::SERIAL): `Jobs::new` reads the registry and calls
/// `Queue::serial_jobs`, which is the one seam that turns the `serial_chain`
/// name into a serial slot.
fn harness(queue: Arc<RedisQueue>) -> (Jobs, Arc<Runs>, Arc<Gate>) {
    let runs = Arc::new(Runs::default());
    let gate = Arc::new(Gate::default());

    let mut providers = moso_core::di::ProviderMapBuilder::new();
    providers.insert_arc(Arc::clone(&runs));
    providers.insert_arc(Arc::clone(&gate));

    let jobs = Jobs::new(Arc::clone(&queue) as Arc<dyn Queue>, registry())
        .with_dead_letters(queue as Arc<dyn DeadLetterQueue>)
        .with_resolver(moso_core::Resolver::new(providers.build()));
    (jobs, runs, gate)
}

fn worker(jobs: &Jobs) -> Worker {
    Worker::new(jobs.clone(), jobs.shared_registry())
        .with_id(WorkerId::new("test-worker"))
        .concurrency(4)
        .lease(Duration::from_secs(30))
        .poll(Duration::from_millis(10))
}

// ---------------------------------------------------------------------------
// Enqueue and process
// ---------------------------------------------------------------------------

/// The baseline against a real broker: a job enqueued is a job run, once, and
/// acknowledged — which on Redis means its blob is deleted and the ready set is
/// empty again.
#[tokio::test]
async fn an_enqueued_job_is_processed_and_acknowledged() {
    let Some(url) = redis("process").await else {
        return;
    };
    let queue = queue(&url, "process");
    let (jobs, runs, _gate) = harness(Arc::clone(&queue));

    SendWelcome::enqueue(&jobs, Payload { user_id: 7 })
        .await
        .expect("enqueued");

    assert_eq!(worker(&jobs).run_once().await.expect("a batch"), 1);
    assert_eq!(runs.count(), 1, "the body ran exactly once");

    let stats = Queue::stats(queue.as_ref(), &["mail".to_owned()])
        .await
        .expect("stats");
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].ready, 0, "the acknowledged job left the ready set");
    assert_eq!(stats[0].running, 0, "and is not still leased");
}

/// Deduplication on the unique-key marker: two enqueues of the same payload
/// inside the window collapse to one ready row, and both calls succeed — because
/// that is what deduplication means.
#[tokio::test]
async fn an_identical_payload_is_deduplicated() {
    let Some(url) = redis("dedupe").await else {
        return;
    };
    let queue = queue(&url, "dedupe");
    let (jobs, _runs, _gate) = harness(Arc::clone(&queue));

    SendWelcome::enqueue(&jobs, Payload { user_id: 42 })
        .await
        .expect("first");
    SendWelcome::enqueue(&jobs, Payload { user_id: 42 })
        .await
        .expect("the duplicate is a successful no-op");
    SendWelcome::enqueue(&jobs, Payload { user_id: 43 })
        .await
        .expect("a different payload is a different job");

    let stats = Queue::stats(queue.as_ref(), &["mail".to_owned()])
        .await
        .expect("stats");
    assert_eq!(stats[0].ready, 2, "two distinct jobs, not three");
}

// ---------------------------------------------------------------------------
// The serial slot, against real Redis
// ---------------------------------------------------------------------------

/// `Job::SERIAL` on the Redis backend, at the pull level: two rows of one serial
/// job, and the second is not leasable until the first is acknowledged.
///
/// This is the deterministic core of what `serial_chains: true` promises. The
/// first pull takes the serial slot (`SET … NX`); the second pull finds the slot
/// held, puts the member back and leases nothing; the `ack` releases the slot
/// (a compare-and-delete, so a reclaimed worker cannot free somebody else's);
/// and the third pull can proceed.
#[tokio::test]
async fn a_serial_job_is_leased_one_instance_at_a_time() {
    let Some(url) = redis("serial").await else {
        return;
    };
    let queue = queue(&url, "serial");
    let (jobs, _runs, _gate) = harness(Arc::clone(&queue));

    SerialChain::enqueue(&jobs, 1).await.expect("enqueued");
    SerialChain::enqueue(&jobs, 2).await.expect("enqueued");

    let first = Queue::pull(
        queue.as_ref(),
        &["serial".to_owned()],
        10,
        Duration::from_secs(60),
        WorkerId::new("worker-a"),
    )
    .await
    .expect("leased");
    assert_eq!(first.len(), 1, "one instance, not two");

    let second = Queue::pull(
        queue.as_ref(),
        &["serial".to_owned()],
        10,
        Duration::from_secs(60),
        WorkerId::new("worker-b"),
    )
    .await
    .expect("nothing");
    assert!(
        second.is_empty(),
        "another worker cannot start the second one while the slot is held"
    );

    let (_, lease) = first.into_iter().next().expect("one job");
    Queue::ack(queue.as_ref(), lease)
        .await
        .expect("acknowledged");

    let third = Queue::pull(
        queue.as_ref(),
        &["serial".to_owned()],
        10,
        Duration::from_secs(60),
        WorkerId::new("worker-b"),
    )
    .await
    .expect("leased");
    assert_eq!(third.len(), 1, "acknowledging the first releases the chain");
}

/// The behavioural claim, end to end through a running worker: two instances of
/// a serial chain run **in order, not concurrently**.
///
/// Four concurrent slots and a fast poll, so nothing but the serial slot can be
/// what holds the second instance back while the first sits open on the gate.
#[tokio::test]
async fn a_serial_chain_runs_in_order_not_concurrently() {
    let Some(url) = redis("order").await else {
        return;
    };
    let queue = queue(&url, "order");
    let (jobs, _runs, gate) = harness(Arc::clone(&queue));

    SerialChain::enqueue(&jobs, 1).await.expect("enqueued");
    SerialChain::enqueue(&jobs, 2).await.expect("enqueued");

    let shutdown = moso_core::Signal::new();
    let running = tokio::spawn({
        let worker = worker(&jobs).concurrency(4).queues(["serial"]);
        let shutdown = shutdown.clone();
        async move { worker.run(shutdown).await }
    });

    gate.settle(|gate| gate.started() > 0).await;
    assert_eq!(gate.started(), 1, "the first instance started");

    // Long enough for a 10ms-poll worker to have asked for work many times.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        gate.started(),
        1,
        "the second instance must not start while the first holds the chain"
    );

    gate.open();
    gate.settle(|gate| gate.finished() == 2).await;
    assert_eq!(gate.finished(), 2, "both ran, one after the other");

    shutdown.trigger();
    tokio::time::timeout(Duration::from_secs(10), running)
        .await
        .expect("the worker stopped")
        .expect("the task finished")
        .expect("a clean stop");
}

/// The control, so the serial test above is not just measuring a concurrency of
/// one: a job that is **not** serial leases every ready row in one batch, which
/// proves the serial slot is not quietly serialising the whole queue.
#[tokio::test]
async fn a_non_serial_job_is_leased_in_a_batch() {
    let Some(url) = redis("batch").await else {
        return;
    };
    let queue = queue(&url, "batch");
    let (jobs, _runs, _gate) = harness(Arc::clone(&queue));

    for user_id in 0..3 {
        SendWelcome::enqueue(&jobs, Payload { user_id })
            .unique_key(format!("welcome:{user_id}"))
            .spawn()
            .await
            .expect("enqueued");
    }

    let leased = Queue::pull(
        queue.as_ref(),
        &["mail".to_owned()],
        10,
        Duration::from_secs(60),
        WorkerId::new("worker"),
    )
    .await
    .expect("leased");
    assert_eq!(leased.len(), 3, "all three at once");
}
