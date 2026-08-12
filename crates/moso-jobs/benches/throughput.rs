//! Acceptance criterion 9: **1000 jobs/s sustained enqueue *and* execute on
//! PostgreSQL, with a bounded table.**
//!
//! ```text
//! DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test \
//!   cargo bench -p moso-jobs --bench throughput
//! ```
//!
//! Not a `criterion` harness: this measures a *throughput floor* over a fixed
//! amount of work against a real database, which is one number and one
//! assertion, and criterion's statistics would add a dependency to say the same
//! thing less clearly. `harness = false` and a `main` that exits non-zero below
//! the floor is the whole design.
//!
//! # What "bounded table" means, and why it is measured
//!
//! A queue that sustains a thousand jobs a second and grows a row per job is a
//! queue that is fast for a week. The run asserts that the hot table's size at
//! the end is bounded by *work in flight* rather than by work ever done: dead
//! letters live in their own table and finished rows are swept on the tick the
//! worker already has.
//!
//! # Environment
//!
//! | Variable | Default | Meaning |
//! | --- | --- | --- |
//! | `DATABASE_URL` | — | required; the benchmark skips without it |
//! | `MOSO_BENCH_JOBS` | 4000 | how many jobs to move |
//! | `MOSO_BENCH_CONCURRENCY` | 32 | worker slots |
//! | `MOSO_BENCH_FLOOR` | 1000 | jobs per second the run must beat |

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use moso_jobs::backend::PgQueue;
use moso_jobs::{Job, JobCtx, JobRegistry, Jobs, Queue, Result, Worker, WorkerId};
use moso_orm::Db;
use serde::{Deserialize, Serialize};

/// How many job bodies actually ran, so the number reported is execution and
/// not just enqueue.
static EXECUTED: AtomicU64 = AtomicU64::new(0);

/// The smallest payload a real job has: an identifier.
#[derive(Serialize, Deserialize)]
struct Ping {
    /// Which one.
    n: u64,
}

/// A job that does nothing, so the number measured is the queue's and not the
/// application's.
struct Bench;

impl Job for Bench {
    type Args = Ping;
    const NAME: &'static str = "bench_ping";
    const QUEUE: &'static str = "bench";
    const RETRIES: u32 = 1;
    // No deduplication: a fingerprint per row would measure the unique index
    // rather than the queue.
    const UNIQUE_FOR: Option<Duration> = None;

    async fn run(_args: Ping, _ctx: JobCtx) -> Result {
        EXECUTED.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

fn main() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping: DATABASE_URL is not set.\n\
             run `DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test \
             cargo bench -p moso-jobs --bench throughput`"
        );
        return;
    };

    let jobs_to_move: u64 = env_number("MOSO_BENCH_JOBS", 4_000);
    let concurrency = usize::try_from(env_number("MOSO_BENCH_CONCURRENCY", 32)).unwrap_or(32);
    let floor = env_number("MOSO_BENCH_FLOOR", 1_000) as f64;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");

    let outcome = runtime.block_on(run(&url, jobs_to_move, concurrency));

    println!();
    println!("  jobs                {jobs_to_move}");
    println!("  worker concurrency  {concurrency}");
    println!(
        "  enqueue             {:>8.0} jobs/s   ({:.2}s)",
        outcome.enqueue_rate, outcome.enqueue_seconds
    );
    println!(
        "  execute             {:>8.0} jobs/s   ({:.2}s)",
        outcome.execute_rate, outcome.execute_seconds
    );
    println!(
        "  end to end          {:>8.0} jobs/s   ({:.2}s)",
        outcome.combined_rate, outcome.total_seconds
    );
    println!("  hot table at end    {} row(s)", outcome.rows_left);
    println!("  floor               {floor:>8.0} jobs/s");
    println!();

    let mut failed = false;
    if outcome.executed != outcome.enqueued {
        eprintln!(
            "FAIL: {} of {} jobs ran",
            outcome.executed, outcome.enqueued
        );
        failed = true;
    }
    if outcome.combined_rate < floor {
        eprintln!(
            "FAIL: {:.0} jobs/s end to end is below the {floor:.0} jobs/s floor",
            outcome.combined_rate
        );
        failed = true;
    }
    let allowance = u64::try_from(concurrency).unwrap_or(64);
    if outcome.rows_left > allowance {
        eprintln!(
            "FAIL: {} rows left in the hot table; it is bounded by work in flight, not by \
             work ever done",
            outcome.rows_left
        );
        failed = true;
    }

    if failed {
        std::process::exit(1);
    }
    println!("OK");
}

/// What one run measured.
struct Outcome {
    /// How many rows went in.
    enqueued: u64,
    /// How many bodies ran.
    executed: u64,
    /// Seconds spent enqueueing.
    enqueue_seconds: f64,
    /// Seconds spent executing.
    execute_seconds: f64,
    /// Both.
    total_seconds: f64,
    /// Jobs per second, enqueue only.
    enqueue_rate: f64,
    /// Jobs per second, execute only.
    execute_rate: f64,
    /// Jobs per second, end to end.
    combined_rate: f64,
    /// Rows still in the hot table.
    rows_left: u64,
}

/// Enqueue `count` jobs, run them all, and measure both halves.
async fn run(url: &str, count: u64, concurrency: usize) -> Outcome {
    let db = Db::connect_url(url).await.expect("the database connects");

    let queue = Arc::new(
        PgQueue::new(db.clone())
            .table_prefix("moso_bench_jobs")
            // Sweep aggressively: the point of the run is that the hot table
            // stays bounded, and a retention of an hour would hide it.
            .keep_done(Duration::ZERO)
            .sweep_interval(Duration::from_nanos(1)),
    );

    // Start from an empty table so the number is this run's.
    for table in [queue.table(), queue.dead_table()] {
        let _ = moso_orm::RawQuery::new(format!("drop table if exists {table}"))
            .execute(&db)
            .await;
    }
    queue.migrate().await.expect("the schema is created");

    let jobs = Jobs::new(
        Arc::clone(&queue) as Arc<dyn Queue>,
        Arc::new(JobRegistry::new().register::<Bench>()),
    );

    // ── enqueue ────────────────────────────────────────────────────────────
    //
    // In parallel, because an application enqueues from many request handlers
    // at once and a serial loop would measure round-trip latency rather than
    // throughput.
    EXECUTED.store(0, Ordering::Relaxed);
    let started = Instant::now();

    let writers = 16_u64;
    let per_writer = count / writers;
    let mut tasks = Vec::new();
    for writer in 0..writers {
        let jobs = jobs.clone();
        tasks.push(tokio::spawn(async move {
            for n in 0..per_writer {
                Bench::enqueue(
                    &jobs,
                    Ping {
                        n: writer * per_writer + n,
                    },
                )
                .await
                .expect("enqueued");
            }
        }));
    }
    for task in tasks {
        task.await.expect("the writer finished");
    }
    let enqueue_seconds = started.elapsed().as_secs_f64();
    let enqueued = per_writer * writers;

    // ── execute ────────────────────────────────────────────────────────────
    let executing = Instant::now();
    let worker = Worker::new(jobs.clone(), jobs.shared_registry())
        .with_id(WorkerId::new("bench"))
        .concurrency(concurrency)
        .lease(Duration::from_secs(60))
        .poll(Duration::from_millis(5));

    while EXECUTED.load(Ordering::Relaxed) < enqueued {
        if worker.run_once().await.expect("a batch") == 0 {
            break;
        }
    }
    let execute_seconds = executing.elapsed().as_secs_f64();
    let total_seconds = started.elapsed().as_secs_f64();

    // ── bounded table ──────────────────────────────────────────────────────
    queue
        .reclaim(&["bench".to_owned()])
        .await
        .expect("the sweeper ran");
    let rows_left = {
        use moso_orm::Executor as _;
        let sql =
            moso_orm::RawQuery::new(format!("select count(*) from {}", queue.table())).into_sql();
        let row = db
            .handle()
            .fetch_optional_sql(sql)
            .await
            .expect("counted")
            .expect("one row");
        u64::try_from(row.get_i64(0).expect("a count")).unwrap_or(0)
    };

    let executed = EXECUTED.load(Ordering::Relaxed);
    Outcome {
        enqueued,
        executed,
        enqueue_seconds,
        execute_seconds,
        total_seconds,
        enqueue_rate: rate(enqueued, enqueue_seconds),
        execute_rate: rate(executed, execute_seconds),
        combined_rate: rate(executed, total_seconds),
        rows_left,
    }
}

/// Jobs per second, guarding the zero-duration case a very fast machine can
/// produce for a very small run.
fn rate(count: u64, seconds: f64) -> f64 {
    if seconds <= f64::EPSILON {
        return f64::INFINITY;
    }
    count as f64 / seconds
}

/// Read a whole number from the environment, falling back rather than failing:
/// a benchmark that refuses to run because a variable is misspelled is a
/// benchmark nobody runs.
fn env_number(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(fallback)
}
