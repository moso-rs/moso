//! The SQL-table queue, against a real database.
//!
//! Every test here runs twice: once on SQLite, which needs nothing, and once on
//! PostgreSQL when `DATABASE_URL` is set. The second is where the interesting
//! claims live — `SELECT … FOR UPDATE SKIP LOCKED`, `LISTEN`/`NOTIFY`, and the
//! headline feature, transactional enqueue — and it **skips with a message**
//! rather than failing when there is no container, so the suite still passes on
//! a machine without Docker.
//!
//! ```text
//! DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test cargo test -p moso-jobs
//! ```
//!
//! Each test gets its own table prefix, so the two dialects and the tests within
//! a dialect never see each other's rows.

#![allow(missing_docs)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use moso_jobs::backend::PgQueue;
use moso_jobs::{
    DeadLetterQueue, DlqFilter, Enqueue as _, Error, Job, JobCtx, JobRegistry, Jobs, Priority,
    Queue, QueuedJob, Result, Worker, WorkerId,
};
use moso_orm::Db;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// How many times a body ran, **per test**.
///
/// A single global counter would be shared by every test in this binary, and
/// `cargo test` runs them concurrently in one process — so the counter travels
/// through the dependency graph instead, which is also the DI path a real
/// worker uses.
#[derive(Debug, Default)]
struct Runs(AtomicU32);

impl Runs {
    fn hit(ctx: &JobCtx) {
        if let Ok(runs) = ctx.inject::<Runs>() {
            runs.0.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Payload {
    user_id: u64,
}

struct SendWelcome;
impl Job for SendWelcome {
    type Args = Payload;
    const NAME: &'static str = "send_welcome_email";
    const QUEUE: &'static str = "mail";
    const RETRIES: u32 = 3;
    const BACKOFF: moso_jobs::Backoff = moso_jobs::Backoff::Immediate;
    const UNIQUE_FOR: Option<Duration> = Some(Duration::from_secs(600));
    async fn run(_args: Payload, ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Ok(())
    }
}

struct AlwaysFails;
impl Job for AlwaysFails {
    type Args = ();
    const NAME: &'static str = "always_fails";
    const RETRIES: u32 = 2;
    const BACKOFF: moso_jobs::Backoff = moso_jobs::Backoff::Immediate;
    async fn run(_args: (), ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Err(Error::retry("the payment gateway is down"))
    }
}

/// One instance at a time across the fleet, whatever the payload.
struct OneAtATime;
impl Job for OneAtATime {
    type Args = u64;
    const NAME: &'static str = "one_at_a_time";
    const QUEUE: &'static str = "serial";
    const SERIAL: bool = true;
    async fn run(_args: u64, ctx: JobCtx) -> Result {
        Runs::hit(&ctx);
        Ok(())
    }
}

fn registry() -> Arc<JobRegistry> {
    Arc::new(
        JobRegistry::new()
            .register::<SendWelcome>()
            .register::<AlwaysFails>()
            .register::<OneAtATime>(),
    )
}

/// Which databases this run can reach.
///
/// SQLite always; PostgreSQL when `DATABASE_URL` is set. A test that finds no
/// PostgreSQL says so once and passes, because a suite that fails on a laptop
/// with no Docker is a suite people stop running.
async fn databases(tag: &str) -> Vec<(&'static str, Db)> {
    let mut out = Vec::new();

    let path = std::env::temp_dir().join(format!(
        "moso-jobs-{tag}-{}-{}.sqlite",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let sqlite = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
        .await
        .expect("sqlite opens");
    out.push(("sqlite", sqlite));

    match std::env::var("DATABASE_URL") {
        Ok(url) => match Db::connect_url(&url).await {
            Ok(db) => out.push(("postgres", db)),
            Err(error) => panic!(
                "DATABASE_URL is set to `{url}` and the connection failed: {error}\n\
                 unset it to run the SQLite half only"
            ),
        },
        Err(_) => {
            eprintln!(
                "skipping the PostgreSQL half of `{tag}`: DATABASE_URL is not set.\n\
                 run `DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test cargo test \
                 -p moso-jobs` for the full suite"
            );
        }
    }
    out
}

/// A queue with a table prefix nobody else in this suite uses.
fn queue(db: Db, tag: &str, dialect: &str) -> Arc<PgQueue> {
    Arc::new(
        PgQueue::new(db)
            .table_prefix(&format!("moso_t_{tag}_{dialect}"))
            .sweep_interval(Duration::ZERO),
    )
}

/// Drop this test's tables, so a re-run starts clean.
async fn reset(queue: &PgQueue, db: &Db) {
    for table in [queue.table(), queue.dead_table(), queue.schedule_table()] {
        let _ = moso_orm::RawQuery::new(format!("drop table if exists {table}"))
            .execute(db)
            .await;
    }
    queue.migrate().await.expect("the schema is created");
}

fn jobs(queue: Arc<PgQueue>) -> (Jobs, Arc<Runs>) {
    let runs = Arc::new(Runs::default());
    let mut providers = moso_core::di::ProviderMapBuilder::new();
    providers.insert_arc(Arc::clone(&runs));
    let jobs = Jobs::new(Arc::clone(&queue) as Arc<dyn Queue>, registry())
        .with_dead_letters(queue as Arc<dyn DeadLetterQueue>)
        .with_resolver(moso_core::Resolver::new(providers.build()));
    (jobs, runs)
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
// The acceptance criteria
// ---------------------------------------------------------------------------

/// **Acceptance criterion 1**, the headline: a rolled-back transaction leaves
/// no job.
///
/// This is the single most valuable thing in the crate. Without it, a welcome
/// email goes out for a user whose creation rolled back — the most common bug
/// in every Rails, Celery and Sidekiq application.
#[tokio::test]
async fn a_rolled_back_transaction_leaves_no_job() {
    for (dialect, db) in databases("txrollback").await {
        let queue = queue(db.clone(), "txrollback", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        // Committed: the job is there.
        let tx = db.begin().await.expect("a transaction");
        SendWelcome::enqueue(&jobs, Payload { user_id: 1 })
            .spawn_in(&tx)
            .await
            .expect("enqueued in the transaction");
        tx.commit().await.expect("committed");

        let after_commit = Queue::stats(queue.as_ref(), &["mail".to_owned()])
            .await
            .expect("stats")
            .first()
            .map(|one| one.ready)
            .unwrap_or_default();
        assert_eq!(after_commit, 1, "{dialect}: the committed enqueue is there");

        // Rolled back: it is not.
        let tx = db.begin().await.expect("a transaction");
        SendWelcome::enqueue(&jobs, Payload { user_id: 2 })
            .spawn_in(&tx)
            .await
            .expect("enqueued in the transaction");
        tx.rollback().await.expect("rolled back");

        let after_rollback = Queue::stats(queue.as_ref(), &["mail".to_owned()])
            .await
            .expect("stats")
            .first()
            .map(|one| one.ready)
            .unwrap_or_default();
        assert_eq!(
            after_rollback, 1,
            "{dialect}: the rolled-back enqueue left nothing behind"
        );
    }
}

/// A job pulled by one worker is invisible to another: `SKIP LOCKED` on
/// PostgreSQL, and SQLite's own writer serialisation.
#[tokio::test]
async fn two_workers_never_take_the_same_job() {
    for (dialect, db) in databases("skiplocked").await {
        let queue = queue(db.clone(), "skiplocked", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        // Twenty jobs, four workers pulling five at a time, all at once.
        for id in 0..20 {
            SendWelcome::enqueue(&jobs, Payload { user_id: id })
                .unique_key(format!("welcome:{id}"))
                .spawn()
                .await
                .expect("enqueued");
        }

        let mut pulls = Vec::new();
        for index in 0..4 {
            let queue = Arc::clone(&queue);
            pulls.push(tokio::spawn(async move {
                queue
                    .pull(
                        &["mail".to_owned()],
                        5,
                        Duration::from_secs(30),
                        WorkerId::new(format!("worker-{index}")),
                    )
                    .await
                    .expect("pulled")
            }));
        }

        let mut seen = std::collections::BTreeSet::new();
        let mut total = 0;
        for pull in pulls {
            for (job, _lease) in pull.await.expect("the task finished") {
                total += 1;
                assert!(
                    seen.insert(job.id),
                    "{dialect}: two workers took job {}",
                    job.id
                );
            }
        }
        assert_eq!(total, 20, "{dialect}: every job was leased exactly once");
    }
}

/// Deduplication: two enqueues of the same payload inside the window produce
/// one row, and both calls succeed — because that is what deduplication means.
#[tokio::test]
async fn an_identical_payload_is_deduplicated() {
    for (dialect, db) in databases("dedupe").await {
        let queue = queue(db.clone(), "dedupe", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        SendWelcome::enqueue(&jobs, Payload { user_id: 42 })
            .await
            .expect("first");
        SendWelcome::enqueue(&jobs, Payload { user_id: 42 })
            .await
            .expect("the duplicate is a successful no-op");
        SendWelcome::enqueue(&jobs, Payload { user_id: 43 })
            .await
            .expect("a different payload is a different job");

        let ready = Queue::stats(queue.as_ref(), &["mail".to_owned()])
            .await
            .expect("stats")
            .first()
            .map(|one| one.ready)
            .unwrap_or_default();
        assert_eq!(ready, 2, "{dialect}: two distinct jobs, not three");
    }
}

/// The whole life of a failing job: attempts, then the dead-letter table, with
/// the payload intact and the error chain recorded.
#[tokio::test]
async fn a_failing_job_reaches_the_dead_letter_table_with_its_payload() {
    for (dialect, db) in databases("deadletter").await {
        let queue = queue(db.clone(), "deadletter", dialect);
        reset(&queue, &db).await;
        let (jobs, runs) = jobs(Arc::clone(&queue));

        AlwaysFails::enqueue(&jobs, ()).await.expect("enqueued");
        let worker = worker(&jobs);
        drain(&worker).await;
        assert_eq!(
            runs.0.load(Ordering::SeqCst),
            2,
            "{dialect}: `RETRIES = 2` is two attempts"
        );

        let stats = DeadLetterQueue::stats(queue.as_ref())
            .await
            .expect("dead letters");
        assert_eq!(stats.total, 1, "{dialect}");
        assert_eq!(stats.by_job, vec![("always_fails".to_owned(), 1)]);

        let (letters, _) = queue
            .list(&DlqFilter::new(), None, 10)
            .await
            .expect("listed");
        assert_eq!(letters.len(), 1);
        assert!(
            letters[0].last_error.contains("payment gateway"),
            "{dialect}: {}",
            letters[0].last_error
        );
        assert_eq!(letters[0].attempts, 2);

        // The hot table is empty: a dead job weighs on its own table and not on
        // the one the pull path walks.
        let hot = Queue::stats(queue.as_ref(), &["default".to_owned()])
            .await
            .expect("stats")
            .first()
            .map(|one| one.ready + one.running + one.retrying)
            .unwrap_or_default();
        assert_eq!(
            hot, 0,
            "{dialect}: the hot table is bounded by work in flight"
        );

        // Bulk retry puts it back, payload and all.
        assert_eq!(
            queue
                .retry(&DlqFilter::new().error_contains("payment gateway"), 10)
                .await
                .expect("retried"),
            1
        );
        assert_eq!(
            DeadLetterQueue::stats(queue.as_ref())
                .await
                .expect("stats")
                .total,
            0
        );
        runs.0.store(0, Ordering::SeqCst);
        worker.run_once().await.expect("a batch");
        assert_eq!(runs.0.load(Ordering::SeqCst), 1, "{dialect}: it ran again");
    }
}

/// **Acceptance criterion 4**: a worker killed mid-job has the job reclaimed
/// after its lease, and the dead worker's acknowledgement is refused.
#[tokio::test]
async fn a_lost_lease_is_reclaimed_and_the_old_worker_is_locked_out() {
    for (dialect, db) in databases("reclaim").await {
        let queue = queue(db.clone(), "reclaim", dialect);
        reset(&queue, &db).await;
        let (jobs, runs) = jobs(Arc::clone(&queue));

        SendWelcome::enqueue(&jobs, Payload { user_id: 1 })
            .await
            .expect("enqueued");

        // A worker leases it and dies.
        let leased = queue
            .pull(
                &["mail".to_owned()],
                10,
                // A lease already in the past, which is what a dead worker's
                // lease looks like a minute later.
                Duration::from_millis(1),
                WorkerId::new("doomed"),
            )
            .await
            .expect("leased");
        assert_eq!(leased.len(), 1, "{dialect}");

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            queue.reclaim(&["mail".to_owned()]).await.expect("swept"),
            1,
            "{dialect}: the expired lease was reclaimed"
        );

        let (_, stale) = leased.into_iter().next().expect("one job");
        let refused = queue.ack(stale).await;
        assert!(
            refused.is_err(),
            "{dialect}: a reclaimed lease must not acknowledge somebody else's run"
        );

        // And it runs again, exactly once.
        worker(&jobs).run_once().await.expect("a batch");
        assert_eq!(runs.0.load(Ordering::SeqCst), 1, "{dialect}");
    }
}

/// A heartbeat extends the lease, so a long job is not stolen from under
/// itself — and a heartbeat on a lease somebody else took fails.
#[tokio::test]
async fn a_heartbeat_extends_a_lease_and_a_stolen_one_refuses() {
    for (dialect, db) in databases("heartbeat").await {
        let queue = queue(db.clone(), "heartbeat", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        SendWelcome::enqueue(&jobs, Payload { user_id: 1 })
            .await
            .expect("enqueued");
        let mut leased = queue
            .pull(
                &["mail".to_owned()],
                1,
                Duration::from_millis(50),
                WorkerId::new("slow"),
            )
            .await
            .expect("leased");
        let (_, lease) = leased.pop().expect("one job");

        queue
            .heartbeat(&lease, Duration::from_secs(60))
            .await
            .expect("extended");
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert_eq!(
            queue.reclaim(&["mail".to_owned()]).await.expect("swept"),
            0,
            "{dialect}: the heartbeat kept the lease alive past its original expiry"
        );

        // A heartbeat against a token nobody holds is refused.
        let forged = moso_jobs::Lease::new(
            lease.job_id(),
            "not-the-token",
            chrono::Utc::now() + chrono::Duration::seconds(60),
        );
        assert!(
            queue
                .heartbeat(&forged, Duration::from_secs(60))
                .await
                .is_err(),
            "{dialect}"
        );
    }
}

/// Priority is what makes `Priority::High` mean anything, and it has to be the
/// index's order and not the insertion order.
#[tokio::test]
async fn a_high_priority_job_is_pulled_first() {
    for (dialect, db) in databases("priority").await {
        let queue = queue(db.clone(), "priority", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        // Enqueue low first, so insertion order and priority order disagree.
        SendWelcome::enqueue(&jobs, Payload { user_id: 1 })
            .priority(Priority::Low)
            .unique_key("low")
            .spawn()
            .await
            .expect("enqueued");
        SendWelcome::enqueue(&jobs, Payload { user_id: 2 })
            .priority(Priority::Critical)
            .unique_key("critical")
            .spawn()
            .await
            .expect("enqueued");

        let leased = queue
            .pull(
                &["mail".to_owned()],
                1,
                Duration::from_secs(30),
                WorkerId::new("w"),
            )
            .await
            .expect("leased");
        assert_eq!(leased.len(), 1);
        assert_eq!(
            leased[0].0.priority,
            Priority::Critical,
            "{dialect}: priority beats insertion order"
        );
    }
}

/// A delayed job is not ready until its time, and then it is.
#[tokio::test]
async fn a_delayed_job_waits_for_its_time() {
    for (dialect, db) in databases("delay").await {
        let queue = queue(db.clone(), "delay", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        SendWelcome::enqueue(&jobs, Payload { user_id: 1 })
            .delay(Duration::from_secs(3600))
            .spawn()
            .await
            .expect("enqueued");

        assert_eq!(
            worker(&jobs).run_once().await.expect("a batch"),
            0,
            "{dialect}: an hour from now is not now"
        );

        // Move it into the past by hand, which is what a clock does.
        moso_orm::RawQuery::new(format!(
            "update {} set run_at = {}",
            queue.table(),
            if dialect == "postgres" { "$1" } else { "?" }
        ))
        .bind(chrono::Utc::now() - chrono::Duration::seconds(1))
        .execute(&db)
        .await
        .expect("moved");

        assert_eq!(
            worker(&jobs).run_once().await.expect("a batch"),
            1,
            "{dialect}"
        );
    }
}

/// The whole row survives a round trip through the database: every field the
/// queue stores comes back the way it went in.
#[tokio::test]
async fn a_row_round_trips_through_the_table() {
    for (dialect, db) in databases("roundtrip").await {
        let queue = queue(db.clone(), "roundtrip", dialect);
        reset(&queue, &db).await;

        let original = QueuedJob::new(
            "send_welcome_email",
            "mail",
            serde_json::json!({ "user_id": 7, "nested": { "a": [1, 2, 3] } }),
        )
        .with_priority(Priority::High)
        .with_retry(moso_jobs::RetryPolicy::new(
            5,
            moso_jobs::Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600)),
        ))
        .with_run_at(chrono::Utc::now() - chrono::Duration::seconds(1))
        .with_unique_key("welcome:7")
        .with_trace_parent("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01");
        queue.push(original.clone()).await.expect("pushed");

        let leased = queue
            .pull(
                &["mail".to_owned()],
                1,
                Duration::from_secs(30),
                WorkerId::new("w"),
            )
            .await
            .expect("leased");
        let read = &leased.first().expect("one job").0;

        assert_eq!(read.id, original.id, "{dialect}");
        assert_eq!(read.name, original.name, "{dialect}");
        assert_eq!(read.queue, original.queue, "{dialect}");
        assert_eq!(read.payload, original.payload, "{dialect}");
        assert_eq!(read.priority, original.priority, "{dialect}");
        assert_eq!(read.attempt, original.attempt, "{dialect}");
        assert_eq!(read.retry, original.retry, "{dialect}");
        assert_eq!(read.unique_key, original.unique_key, "{dialect}");
        assert_eq!(read.trace_parent, original.trace_parent, "{dialect}");
        // Timestamps survive to the second; SQLite stores them as text.
        assert!(
            (read.enqueued_at - original.enqueued_at)
                .num_seconds()
                .abs()
                <= 1,
            "{dialect}: {:?} vs {:?}",
            read.enqueued_at,
            original.enqueued_at
        );
    }
}

/// The sweeper is what keeps the hot table bounded: a finished row is kept for
/// the dashboard and then deleted.
#[tokio::test]
async fn finished_rows_are_swept_and_the_hot_table_stays_bounded() {
    for (dialect, db) in databases("sweep").await {
        let queue = Arc::new(
            PgQueue::new(db.clone())
                .table_prefix(&format!("moso_t_sweep_{dialect}"))
                // Keep nothing, sweep every time: what a test needs out of a
                // retention policy is determinism.
                .keep_done(Duration::ZERO)
                .sweep_interval(Duration::from_nanos(1)),
        );
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        for id in 0..5 {
            SendWelcome::enqueue(&jobs, Payload { user_id: id })
                .unique_key(format!("welcome:{id}"))
                .spawn()
                .await
                .expect("enqueued");
        }
        assert_eq!(drain(&worker(&jobs)).await, 5, "{dialect}");

        // `reclaim` carries the sweeper, which is the tick a worker already has.
        queue.reclaim(&["mail".to_owned()]).await.expect("swept");

        let remaining =
            moso_orm::RawQuery::new(format!("select count(*) from {}", queue.table())).into_sql();
        let row = moso_orm::Executor::handle(&db)
            .fetch_optional_sql(remaining)
            .await
            .expect("counted")
            .expect("one row");
        assert_eq!(
            row.get_i64(0).expect("a count"),
            0,
            "{dialect}: finished rows were swept"
        );
    }
}

/// A queue nobody has touched still answers `stats`, which is what `/readyz`
/// and the dashboard call on an idle deployment.
#[tokio::test]
async fn an_empty_queue_reports_zero_rather_than_failing() {
    for (dialect, db) in databases("emptystats").await {
        let queue = queue(db.clone(), "emptystats", dialect);
        reset(&queue, &db).await;

        let stats = Queue::stats(queue.as_ref(), &["mail".to_owned(), "default".to_owned()])
            .await
            .expect("stats");
        assert_eq!(stats.len(), 2, "{dialect}");
        for one in &stats {
            assert_eq!(one.ready, 0);
            assert_eq!(one.running, 0);
            assert_eq!(one.dead, 0);
            assert!(one.oldest_ready.is_none());
        }

        queue.probe().await.expect("the queue is reachable");
    }
}

/// An operator cancelling a ready job stops it from ever starting.
#[tokio::test]
async fn cancelling_a_ready_job_stops_it_starting() {
    for (dialect, db) in databases("cancel").await {
        let queue = queue(db.clone(), "cancel", dialect);
        reset(&queue, &db).await;
        let (jobs, runs) = jobs(Arc::clone(&queue));

        let id = SendWelcome::enqueue(&jobs, Payload { user_id: 1 })
            .await
            .expect("enqueued");
        assert!(jobs.cancel(id).await.expect("cancellable"), "{dialect}");
        assert_eq!(
            worker(&jobs).run_once().await.expect("a batch"),
            0,
            "{dialect}: a cancelled job is not pulled"
        );
        assert_eq!(runs.0.load(Ordering::SeqCst), 0);

        // Cancelling it twice is `false`, not an error.
        assert!(!jobs.cancel(id).await.expect("idempotent"), "{dialect}");
    }
}

/// The prefix is the one value that becomes an identifier rather than a bound
/// parameter, so it is the one value that has to be proved safe.
#[tokio::test]
async fn a_hostile_table_prefix_cannot_reach_the_sql() {
    for (_dialect, db) in databases("prefix").await {
        let queue = PgQueue::new(db).table_prefix("moso_t_x\"; drop table users; --");
        assert_eq!(queue.table(), "moso_t_xdroptableusers");
        assert_eq!(queue.dead_table(), "moso_t_xdroptableusers_dead");

        // An empty prefix falls back rather than producing `create table  (…)`.
        let queue = PgQueue::new(
            Db::connect_url("sqlite://:memory:")
                .await
                .expect("in-memory sqlite"),
        )
        .table_prefix("!!!");
        assert_eq!(queue.table(), "moso_jobs");
    }
}

/// `tx.enqueue(Job, args)` — the spelling the design document leads with.
///
/// It reads the handle [`Jobs::install`] put there at boot, which is a
/// process-wide `OnceLock`; only one test in this binary may install one, so
/// this is that test. Everything else uses `spawn_in`, which takes the handle
/// explicitly and exercises the same code path underneath.
#[tokio::test]
async fn the_extension_method_enqueues_in_the_callers_transaction() {
    // Before anything installs a handle, the message says which line to add.
    if moso_jobs::Jobs::installed().is_none() {
        let path =
            std::env::temp_dir().join(format!("moso-jobs-install-{}.sqlite", std::process::id()));
        let db = Db::connect_url(&format!("sqlite://{}?mode=rwc", path.display()))
            .await
            .expect("sqlite opens");
        let queue = queue(db.clone(), "install", "sqlite");
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        assert!(jobs.install(), "the first install wins");
        assert!(!jobs.install(), "a second one is a reported no-op");

        let tx = db.begin().await.expect("a transaction");
        tx.enqueue(SendWelcome, Payload { user_id: 99 })
            .await
            .expect("enqueued through the extension method");
        tx.commit().await.expect("committed");

        let ready = Queue::stats(queue.as_ref(), &["mail".to_owned()])
            .await
            .expect("stats")
            .first()
            .map(|one| one.ready)
            .unwrap_or_default();
        assert_eq!(ready, 1);

        // And on `&Db`, which works and is deliberately not transactional.
        db.enqueue(SendWelcome, Payload { user_id: 100 })
            .await
            .expect("enqueued outside a transaction");
        let ready = Queue::stats(queue.as_ref(), &["mail".to_owned()])
            .await
            .expect("stats")
            .first()
            .map(|one| one.ready)
            .unwrap_or_default();
        assert_eq!(ready, 2);
    }
}

// ---------------------------------------------------------------------------
// Serial jobs, against real SQL
// ---------------------------------------------------------------------------

/// `Job::SERIAL` on the SQL backend: two rows of one serial job, and the second
/// is not leasable until the first is out of the way.
///
/// The claim is the job's own **lease** — there is no second lock to take,
/// renew or leak — so this also pins that finishing the first row frees the
/// chain.
#[tokio::test]
async fn a_serial_job_is_leased_one_instance_at_a_time() {
    for (dialect, db) in databases("serial").await {
        let queue = queue(db.clone(), "serial", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        OneAtATime::enqueue(&jobs, 1).await.expect("enqueued");
        OneAtATime::enqueue(&jobs, 2).await.expect("enqueued");

        let first = Queue::pull(
            queue.as_ref(),
            &["serial".to_owned()],
            10,
            Duration::from_secs(60),
            WorkerId::new("worker-a"),
        )
        .await
        .expect("leased");
        assert_eq!(first.len(), 1, "{dialect}: one instance, not two");

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
            "{dialect}: and another worker cannot start the second one either"
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
        assert_eq!(
            third.len(),
            1,
            "{dialect}: finishing the first releases the chain"
        );
    }
}

/// A job that is not serial keeps running side by side, so the clause above is
/// not quietly serialising the whole queue.
#[tokio::test]
async fn a_job_that_is_not_serial_is_leased_in_a_batch() {
    for (dialect, db) in databases("notserial").await {
        let queue = queue(db.clone(), "notserial", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

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
        assert_eq!(leased.len(), 3, "{dialect}: all three at once");
    }
}

/// `Queue::find` is what scopes the scheduler's overlap check to one schedule's
/// own occurrence, so it has to answer for a row in any state.
#[tokio::test]
async fn a_row_can_be_found_by_identifier_in_every_state() {
    for (dialect, db) in databases("find").await {
        let queue = queue(db.clone(), "find", dialect);
        reset(&queue, &db).await;
        let (jobs, _runs) = jobs(Arc::clone(&queue));

        let id = SendWelcome::enqueue(&jobs, Payload { user_id: 7 })
            .await
            .expect("enqueued");
        let found = Queue::find(queue.as_ref(), id)
            .await
            .expect("looked up")
            .expect("{dialect}: the row is there");
        assert_eq!(found.id, id);
        assert_eq!(found.state, moso_jobs::JobState::Ready);
        assert!(found.state.is_active(), "{dialect}");

        let leased = Queue::pull(
            queue.as_ref(),
            &["mail".to_owned()],
            1,
            Duration::from_secs(60),
            WorkerId::new("worker"),
        )
        .await
        .expect("leased");
        let (_, lease) = leased.into_iter().next().expect("one job");
        Queue::ack(queue.as_ref(), lease).await.expect("finished");

        let done = Queue::find(queue.as_ref(), id)
            .await
            .expect("looked up")
            .expect("the row is still there");
        assert!(
            !done.state.is_active(),
            "{dialect}: a finished occurrence is not still going"
        );

        assert!(
            Queue::find(queue.as_ref(), moso_jobs::JobId::new())
                .await
                .expect("looked up")
                .is_none(),
            "{dialect}: a row that never existed is absent, not an error"
        );
    }
}

/// The schedule record every process reads: `last_run` and who fired it, in the
/// one store the whole fleet shares.
#[tokio::test]
async fn a_schedule_run_is_recorded_durably_and_overwritten_in_place() {
    for (dialect, db) in databases("schedrun").await {
        let queue = queue(db.clone(), "schedrun", dialect);
        reset(&queue, &db).await;

        let id = moso_jobs::ScheduleId::new("nightly_cleanup", "0 3 * * *");
        let first = chrono::Utc::now() - chrono::Duration::hours(1);
        Queue::record_schedule_run(
            queue.as_ref(),
            &moso_jobs::ScheduleRun::new(
                id.clone(),
                "nightly_cleanup",
                WorkerId::new("pod-1"),
                first,
            ),
        )
        .await
        .expect("recorded");

        let runs = Queue::schedule_runs(queue.as_ref()).await.expect("read");
        assert_eq!(runs.len(), 1, "{dialect}");
        assert_eq!(runs[0].leader.as_str(), "pod-1");

        // The next occurrence, from a different leader, replaces the row rather
        // than adding one — a schedule has one last run, not a history.
        let second = chrono::Utc::now();
        Queue::record_schedule_run(
            queue.as_ref(),
            &moso_jobs::ScheduleRun::new(id, "nightly_cleanup", WorkerId::new("pod-2"), second),
        )
        .await
        .expect("recorded");

        let runs = Queue::schedule_runs(queue.as_ref()).await.expect("read");
        assert_eq!(runs.len(), 1, "{dialect}: still one row");
        assert_eq!(runs[0].leader.as_str(), "pod-2");
        assert!(runs[0].ran_at > first, "{dialect}: and the newer time");
    }
}

/// `DlqFilter::error_contains` used to strip `%` and `_` out of the needle, so
/// searching for a literal percent sign searched for something else entirely.
/// Escaped now, with an explicit `escape` clause on every dialect.
#[tokio::test]
async fn a_dead_letter_search_matches_a_wildcard_literally() {
    /// Fails with an error chain full of `like` metacharacters.
    struct FailsLoudly;
    impl Job for FailsLoudly {
        type Args = u64;
        const NAME: &'static str = "fails_loudly";
        const RETRIES: u32 = 1;
        async fn run(args: u64, _ctx: JobCtx) -> Result {
            Err(Error::permanent(match args {
                0 => "the disk is 50% full_up".to_owned(),
                _ => "the disk is 5011 fullXup".to_owned(),
            }))
        }
    }

    for (dialect, db) in databases("likeescape").await {
        let queue = queue(db.clone(), "likeescape", dialect);
        reset(&queue, &db).await;

        let jobs = Jobs::new(
            Arc::clone(&queue) as Arc<dyn Queue>,
            Arc::new(JobRegistry::new().register::<FailsLoudly>()),
        )
        .with_dead_letters(Arc::clone(&queue) as Arc<dyn DeadLetterQueue>);

        FailsLoudly::enqueue(&jobs, 0).await.expect("enqueued");
        FailsLoudly::enqueue(&jobs, 1).await.expect("enqueued");
        drain(&worker(&jobs)).await;
        assert_eq!(
            DeadLetterQueue::stats(queue.as_ref())
                .await
                .expect("stats")
                .total,
            2,
            "{dialect}: both failed permanently"
        );

        // `%` and `_` are `like` wildcards. Unescaped, `50% full_up` would also
        // match `5011 fullXup`; stripped — which is what this used to do — it
        // would match neither of them for the right reason.
        let (found, _) = DeadLetterQueue::list(
            queue.as_ref(),
            &DlqFilter::new().error_contains("50% full_up"),
            None,
            10,
        )
        .await
        .expect("listed");
        assert_eq!(found.len(), 1, "{dialect}: exactly the literal match");
        assert!(found[0].last_error.contains("50% full_up"), "{dialect}");

        // And a needle that is only wildcards matches nothing, rather than
        // everything, which is what stripping them produced.
        let (none, _) = DeadLetterQueue::list(
            queue.as_ref(),
            &DlqFilter::new().error_contains("%_%"),
            None,
            10,
        )
        .await
        .expect("listed");
        assert!(none.is_empty(), "{dialect}: {none:?}");
    }
}
