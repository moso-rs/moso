//! The frozen surface, exercised from outside the crate.
//!
//! An integration test rather than a unit one because the two claims worth
//! checking are both about how the crate looks from an application: that a
//! `Job` can be written by hand exactly as `#[job]` will generate it, and that
//! `tx.enqueue(..)` resolves on all three executors through the blanket
//! [`Enqueue`] impl.
//!
//! Nothing here drives a queue: the behaviour of the backends, the worker and
//! the scheduler is proved by `tests/sql_backend.rs`, `tests/worker.rs` and the
//! crate's own unit tests. What this file proves is the part those cannot —
//! that the *shape* an application writes against still compiles from outside,
//! and that the values on that surface (a job's constants, a retry policy, a
//! state predicate, an identifier) mean what the documentation says.

#![allow(dead_code, missing_docs)]

use std::time::Duration;

use moso_jobs::{
    Backoff, DrainMode, Enqueue, Job, JobCtx, JobId, JobState, Jobs, Priority, QueueWeight, Result,
    RetryPolicy, Schedule, WorkerId,
};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// What `#[job]` generates
// ---------------------------------------------------------------------------

/// The payload. `Serialize + DeserializeOwned` because it crosses a process
/// boundary — which is why adding a field with a serde default is a safe deploy
/// and renaming one is not.
#[derive(Serialize, Deserialize)]
pub struct SendWelcome {
    pub user_id: u64,
    /// Added in a later deploy. The default is what makes the old rows still
    /// deserialise, which is the whole payload-versioning rule in one field.
    #[serde(default)]
    pub locale: Option<String>,
}

/// The unit struct `#[job]` emits next to the function.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendWelcomeEmail;

impl Job for SendWelcomeEmail {
    type Args = SendWelcome;

    const NAME: &'static str = "send_welcome_email";
    const QUEUE: &'static str = "mail";
    const RETRIES: u32 = 5;
    const TIMEOUT: Duration = Duration::from_secs(120);
    const UNIQUE_FOR: Option<Duration> = Some(Duration::from_secs(600));
    const PRIORITY: Priority = Priority::High;

    fn backoff(attempt: u32) -> Duration {
        Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600)).delay(attempt)
    }

    async fn run(args: SendWelcome, ctx: JobCtx) -> Result {
        // The shape `#[job]` generates for an `Inject(..)` parameter.
        let _greeting = ctx.inject::<String>()?;
        let _ = args.user_id;
        Ok(())
    }

    async fn on_failure(args: &SendWelcome, error: &moso_jobs::Error, _ctx: &JobCtx) {
        let _ = (args.user_id, error.retryable());
    }
}

/// A second job, so the registry has something to hold more than one of.
#[derive(Clone, Copy, Debug, Default)]
pub struct NightlyCleanup;

impl Job for NightlyCleanup {
    type Args = ();

    const NAME: &'static str = "nightly_cleanup";

    async fn run(_args: (), _ctx: JobCtx) -> Result {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compile-only exercises of the frozen signatures
// ---------------------------------------------------------------------------

/// The transactional enqueue, on the executor a `db.transaction` closure hands
/// out. This is the headline feature, and it is the one signature where a
/// mistake would be invisible until an application tried to write it.
async fn transactional_enqueue(tx: &moso_orm::Tx, user_id: u64) -> Result<JobId> {
    tx.enqueue(
        SendWelcomeEmail,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .await
}

/// The same call on `&mut Tx`, which is what a caller holding
/// `let mut tx = db.begin().await?` has.
async fn transactional_enqueue_mut(tx: &mut moso_orm::Tx, user_id: u64) -> Result<JobId> {
    tx.enqueue(
        SendWelcomeEmail,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .await
}

/// And on `&Db`, which works and is *not* transactional. The method lives on
/// `Executor` so a service function generic over its executor can enqueue; the
/// guarantee comes from the transaction, not from the method.
async fn non_transactional_enqueue(db: &moso_orm::Db, user_id: u64) -> Result<JobId> {
    db.enqueue(
        SendWelcomeEmail,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .await
}

/// `J::enqueue(&jobs, args).await` with no `.spawn()` — the `IntoFuture` impl
/// is what makes the common case one line and the configured case a chain.
async fn enqueue_is_a_future(jobs: &Jobs, user_id: u64) -> Result<JobId> {
    SendWelcomeEmail::enqueue(
        jobs,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .await
}

/// …and the configured case, which ends in `.spawn()`.
async fn enqueue_with_options(jobs: &Jobs, user_id: u64) -> Result<JobId> {
    SendWelcomeEmail::enqueue(
        jobs,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .delay(Duration::from_secs(60))
    .priority(Priority::High)
    .queue("mail-urgent")
    .unique_key(format!("welcome:{user_id}"))
    .spawn()
    .await
}

/// The registry, exactly as `src/jobs/mod.rs` will read.
fn registry() -> moso_jobs::JobRegistry {
    moso_jobs::JobRegistry::new()
        .register::<SendWelcomeEmail>()
        .register::<NightlyCleanup>()
        .schedule(
            moso_jobs::Cron::new::<NightlyCleanup>("0 3 * * *", ())
                .timezone("Europe/Rome")
                .catch_up(false)
                .overlap(moso_jobs::Overlap::Skip),
        )
        .schedule(moso_jobs::Every::new::<NightlyCleanup>(
            Duration::from_secs(300),
            (),
        ))
}

/// A worker, built the way `moso worker` will build one.
fn worker(jobs: Jobs, registry: std::sync::Arc<moso_jobs::JobRegistry>) -> moso_jobs::Worker {
    moso_jobs::Worker::new(jobs, registry)
        .concurrency(8)
        .weighted_queues([QueueWeight::new("default", 3), QueueWeight::new("mail", 1)])
        .lease(Duration::from_secs(120))
        .grace(Duration::from_secs(30))
        .drain_mode(DrainMode::Requeue)
        .backpressure(Some(50_000))
}

/// `Cron` and `Every` both erase into one `Schedule`, which is what lets the
/// registry hold them in one list.
fn schedules_erase_uniformly() {
    fn takes(_: Schedule) {}
    takes(moso_jobs::Cron::new::<NightlyCleanup>("0 3 * * *", ()).into());
    takes(moso_jobs::Every::new::<NightlyCleanup>(Duration::from_secs(60), ()).into());
}

/// A job body is held across `.await` inside a worker task, so the future has
/// to be `Send`.
fn job_futures_are_send() {
    fn assert_send<F: Future + Send>(_: F) {}
    assert_send(SendWelcomeEmail::run(
        SendWelcome {
            user_id: 1,
            locale: None,
        },
        unreachable_ctx(),
    ));
}

/// Only ever used inside a `Send` bound check, never evaluated.
fn unreachable_ctx() -> JobCtx {
    unreachable!("the bound check never runs the future")
}

// ---------------------------------------------------------------------------
// Assertions that do run
// ---------------------------------------------------------------------------

/// The constants a job declares are the constants the registry will read. A
/// default that drifted from the documentation would change every job that did
/// not override it.
#[test]
fn a_jobs_constants_are_what_it_declared() {
    assert_eq!(SendWelcomeEmail::NAME, "send_welcome_email");
    assert_eq!(SendWelcomeEmail::QUEUE, "mail");
    assert_eq!(SendWelcomeEmail::RETRIES, 5);
    assert_eq!(SendWelcomeEmail::TIMEOUT, Duration::from_secs(120));
    assert_eq!(SendWelcomeEmail::PRIORITY, Priority::High);
    const { assert!(!SendWelcomeEmail::SERIAL) };

    // The defaults, on a job that overrode nothing.
    assert_eq!(NightlyCleanup::QUEUE, moso_jobs::DEFAULT_QUEUE);
    assert_eq!(NightlyCleanup::RETRIES, moso_jobs::DEFAULT_RETRIES);
    assert_eq!(NightlyCleanup::TIMEOUT, moso_jobs::DEFAULT_TIMEOUT);
    assert_eq!(NightlyCleanup::PRIORITY, Priority::Normal);
    assert_eq!(NightlyCleanup::UNIQUE_FOR, None);
}

/// The wire name is deliberately not the Rust path: renaming a module must not
/// orphan queued rows.
#[test]
fn the_wire_name_is_not_the_rust_path() {
    assert!(!SendWelcomeEmail::NAME.contains("::"));
    assert_eq!(
        SendWelcomeEmail::NAME,
        SendWelcomeEmail::NAME.to_lowercase()
    );
}

/// The payload must round-trip through JSON, and a field added with a serde
/// default must not break a row written before it existed.
#[test]
fn a_payload_survives_a_field_being_added() {
    let old_row = serde_json::json!({ "user_id": 7 });
    let decoded: SendWelcome = serde_json::from_value(old_row).expect("old rows still decode");
    assert_eq!(decoded.user_id, 7);
    assert_eq!(decoded.locale, None);
}

/// A retry policy is carried on the row rather than read from the type, so its
/// shape has to be inspectable and `Copy`.
#[test]
fn a_retry_policy_is_a_plain_value() {
    let policy = RetryPolicy::new(5, Backoff::default_exponential());
    assert_eq!(policy.max_attempts(), 5);
    assert_eq!(policy.backoff(), Backoff::default_exponential());

    let copied = policy;
    assert_eq!(copied.max_attempts(), policy.max_attempts());
}

/// `JobState::is_active` decides what a sweeper may delete, so the boundary is
/// worth pinning here as well as inside the crate.
#[test]
fn only_unfinished_states_are_active() {
    assert!(JobState::Ready.is_active());
    assert!(JobState::Running.is_active());
    assert!(JobState::Retrying.is_active());
    assert!(!JobState::Done.is_active());
    assert!(!JobState::Dead.is_active());
    assert!(!JobState::Cancelled.is_active());
}

/// A worker identifier has to survive a round trip through the queue row.
#[test]
fn worker_identifiers_round_trip() {
    let id = WorkerId::new("pod-7-a1b2");
    assert_eq!(id.as_str(), "pod-7-a1b2");
    assert_eq!(id.to_string(), "pod-7-a1b2");
    assert_eq!(WorkerId::new("pod-7-a1b2"), id);
}

/// Job identifiers are time-ordered, which is what keeps the queue table's
/// primary-key index from fragmenting.
#[test]
fn job_identifiers_are_time_ordered() {
    let first = JobId::new();
    let second = JobId::new();
    assert!(
        first <= second,
        "a v7 identifier generated later must not sort earlier"
    );
    assert_eq!(JobId::from_uuid(first.as_uuid()), first);
}
