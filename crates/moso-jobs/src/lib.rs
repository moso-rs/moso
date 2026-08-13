#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc = "Moso's background-job battery: transactional enqueue, retries, scheduling and workers."]
//!
//! The ecosystem has good pieces and they are un-wired: you assemble the
//! backend, the worker, the retry policy, the serialisation and the
//! observability yourself. This crate is the opinionated layer, with a
//! **Moso-owned [`Job`] trait** so the substrate can change without breaking
//! application code, and with the two features that matter most in practice —
//! **transactional enqueue** and **a real dashboard**.
//!
//! ```no_run
//! use moso_jobs::{Enqueue, Job, JobCtx, Result};
//! use serde::{Deserialize, Serialize};
//!
//! /// Which account to greet.
//! #[derive(Serialize, Deserialize)]
//! pub struct SendWelcome {
//!     /// The new account.
//!     pub user_id: u64,
//! }
//!
//! /// Greets a new account.
//! #[derive(Clone, Copy, Debug, Default)]
//! pub struct SendWelcomeEmail;
//!
//! impl Job for SendWelcomeEmail {
//!     type Args = SendWelcome;
//!     const NAME: &'static str = "send_welcome_email";
//!     const QUEUE: &'static str = "mail";
//!
//!     async fn run(args: SendWelcome, _ctx: JobCtx) -> Result {
//!         let _ = args.user_id;
//!         Ok(())
//!     }
//! }
//!
//! // The enqueue rolls back with the transaction that wrote the user.
//! async fn signup(tx: &moso_orm::Tx, user_id: u64) -> Result {
//!     tx.enqueue(SendWelcomeEmail, SendWelcome { user_id }).await?;
//!     Ok(())
//! }
//! ```
//!
//! # The map
//!
//! | Module | Contents |
//! | --- | --- |
//! | [`mod@job`] | [`Job`], [`JobCtx`], [`JobId`] |
//! | [`mod@registry`] | [`JobRegistry`], [`RegisteredJob`] |
//! | [`mod@queue`] | [`Queue`], [`QueuedJob`], [`JobState`], [`Lease`], [`QueueStats`] |
//! | [`mod@enqueue`] | [`Jobs`], [`EnqueueBuilder`], [`Enqueue`] — the `tx.enqueue` extension |
//! | [`mod@retry`] | [`Priority`], [`Backoff`], [`Overlap`], [`RetryPolicy`] |
//! | [`mod@schedule`] | [`Cron`], [`Every`], [`Schedule`], [`Scheduler`] |
//! | [`mod@worker`] | [`Worker`], [`WorkerId`], [`QueueWeight`], [`DrainMode`] |
//! | [`mod@dlq`] | [`DeadLetter`], [`DeadLetterQueue`], [`DlqFilter`], [`DlqStats`] |
//! | [`mod@backend`] | the Postgres, Redis and memory queues, plus the outbox |
//! | [`mod@dashboard`] | the standalone `/_jobs` view |
//! | [`mod@config`] | [`JobsConfig`], [`JobsHealthCheck`] |
//! | [`mod@health`] | [`health::WorkerHealth`] — `/healthz`, `/readyz`, `/metrics` on a worker pod |
//! | [`mod@metrics`] | the six documented counters, as Prometheus text |
//! | [`mod@trace`] | W3C trace context: request → job → outbound call |
//! | [`mod@actor`] | the enqueueing actor's identity: request → job, for audit |
//! | [`mod@cron`] | the five-field expression and its timezone |
//! | [`mod@error`] | [`Error`], and the retry-or-not decision |
//!
//! # Reliability, stated plainly
//!
//! | Property | Guarantee |
//! | --- | --- |
//! | Delivery | **At-least-once.** Jobs must be idempotent. [`JobCtx::once`] is the helper. |
//! | Ordering | Not guaranteed across jobs. [`Job::SERIAL`] gives one instance of a job *type* at a time, fleet-wide; a `unique_key` chain gives it per payload. |
//! | Exactly-once effects | The application's, with an idempotency key and a unique constraint. |
//! | Durability | Postgres: full. Redis: whatever its persistence configuration gives, and the boot log says so when append-only writing is off. |
//! | Retry | Exponential with full jitter; [`Error::retryable`] decides. Non-retryable errors skip straight to the dead-letter queue. |
//!
//! # Four decisions worth knowing before reading the code
//!
//! **Transactional enqueue is the headline.** [`Enqueue`] extends
//! `moso_orm::Executor`, so `tx.enqueue(..)` writes the job row inside the
//! caller's transaction. A welcome email for a user whose creation rolled back
//! — the single most common bug in Rails, Celery and Sidekiq applications — is
//! not expressible. The Redis backend gets the same guarantee through
//! `backend::Outbox`, and the documentation says so rather than
//! implying the two are identical.
//!
//! **Registration is explicit.** [`JobRegistry::register`] is a statement you
//! can read (ADR-0004: no `inventory`, no `ctor`), which is what lets
//! `App::build()` prove that every enqueued job type is registered and fail at
//! boot naming the enqueue site.
//!
//! **The scheduler elects a leader by default.** Twenty web pods running the
//! nightly job twenty times is the second most common jobs bug, and it is solved
//! by [`Scheduler`] rather than by a configuration option somebody has to
//! discover.
//!
//! **The wire name is not the Rust path.** [`Job::NAME`] is stable across
//! refactors, because renaming a module must not orphan 40,000 queued rows.
//!
//! # Cargo features
//!
//! | Feature | Default | What it adds |
//! | --- | --- | --- |
//! | `jobs-pg` | yes | `backend::PgQueue` — transactional enqueue |
//! | `jobs-memory` | yes | `backend::MemoryQueue` — tests and `moso dev` |
//! | `jobs-redis` | no | `backend::RedisQueue`, and `backend::Outbox` with `jobs-pg` |
//!
//! Code spans rather than links for the feature-gated names: a link to a type
//! that only exists under a cargo feature is a broken link in every build that
//! does not turn it on, and `rustdoc::broken_intra_doc_links` is `deny` across
//! this workspace.

pub mod actor;
pub mod backend;
pub mod config;
pub mod cron;
pub mod dashboard;
pub mod dlq;
pub mod enqueue;
pub mod error;
pub mod health;
pub mod job;
pub mod metrics;
pub mod queue;
pub mod registry;
pub mod retry;
pub mod schedule;
pub mod trace;
pub mod worker;

mod once;
mod rng;

pub use crate::config::{JobsBackendKind, JobsConfig, JobsHealthCheck};
pub use crate::dlq::{DeadLetter, DeadLetterQueue, DlqFilter, DlqStats};
pub use crate::enqueue::{Enqueue, EnqueueBuilder, Jobs};
pub use crate::error::{BoxError, Error, Result};
pub use crate::job::{DEFAULT_QUEUE, DEFAULT_RETRIES, DEFAULT_TIMEOUT, Job, JobCtx, JobId};
pub use crate::queue::{JobState, Lease, Queue, QueueCapabilities, QueueStats, QueuedJob};
pub use crate::registry::{JobRegistry, RegisteredJob, RunFn};
pub use crate::retry::{Backoff, Overlap, Priority, RetryPolicy};
pub use crate::schedule::{
    Cron, Every, Schedule, ScheduleId, ScheduleKind, ScheduleRun, Scheduler, SchedulerLeadership,
    SchedulerReadiness,
};
pub use crate::worker::{DrainMode, QueueWeight, Worker, WorkerId};

/// The version of this crate, for `moso doctor` and the boot log.
///
/// ```
/// assert!(!moso_jobs::VERSION.is_empty());
/// ```
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Everything a `#[job]` body imports.
///
/// Named `prelude` and re-exported by the facade as `moso::jobs::prelude`,
/// which is what `docs/03-batteries/32-jobs.md` writes at the top of every job
/// module.
///
/// ```no_run
/// use moso_jobs::prelude::*;
///
/// async fn body(ctx: JobCtx) -> Result {
///     let _ = ctx.attempt();
///     Ok(())
/// }
/// ```
pub mod prelude {
    pub use crate::{
        Backoff, Enqueue, EnqueueBuilder, Error, Job, JobCtx, JobId, JobRegistry, Jobs, Priority,
        Result, RetryPolicy,
    };
}

/// A [`JobCtx`] for a test that needs one and does not have a worker.
///
/// `JobCtx` is deliberately unconstructable from outside this crate — every
/// field on it is a promise the worker keeps — so the crate's own tests build
/// one here rather than each inventing a different half-real context.
#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Arc;

    /// A context over an empty provider map and an in-memory queue.
    pub(crate) fn ctx() -> crate::JobCtx {
        let jobs = crate::Jobs::new(
            Arc::new(crate::backend::MemoryQueue::new()),
            Arc::new(crate::JobRegistry::new()),
        );
        crate::JobCtx::new(
            crate::JobId::new(),
            "test",
            crate::DEFAULT_QUEUE.to_owned(),
            1,
            1,
            chrono::Utc::now(),
            crate::WorkerId::new("test"),
            moso_core::Resolver::new(Arc::new(moso_core::ProviderMap::default())),
            Arc::new(jobs),
            None,
            None,
            Arc::default(),
            None,
            std::time::Duration::from_secs(60),
        )
    }
}

#[cfg(test)]
mod tests {
    /// The public surface resolves from the crate root, so an application
    /// writes `moso_jobs::Job` and not `moso_jobs::job::Job`.
    #[test]
    fn the_frozen_surface_resolves_from_the_root() {
        fn exists<T>() {}

        exists::<crate::Backoff>();
        exists::<crate::DeadLetter>();
        exists::<crate::DlqFilter>();
        exists::<crate::DlqStats>();
        exists::<crate::DrainMode>();
        exists::<crate::Error>();
        exists::<crate::JobId>();
        exists::<crate::JobRegistry>();
        exists::<crate::JobState>();
        exists::<crate::JobsConfig>();
        exists::<crate::Lease>();
        exists::<crate::Overlap>();
        exists::<crate::Priority>();
        exists::<crate::QueueCapabilities>();
        exists::<crate::QueueStats>();
        exists::<crate::QueueWeight>();
        exists::<crate::QueuedJob>();
        exists::<crate::RegisteredJob>();
        exists::<crate::RetryPolicy>();
        exists::<crate::Schedule>();
        exists::<crate::ScheduleId>();
        exists::<crate::WorkerId>();

        fn dyn_compatible(_: &dyn crate::Queue, _: &dyn crate::DeadLetterQueue) {}
        let _ = dyn_compatible;
    }

    /// Priority has to order the way a queue's `ORDER BY priority DESC` does,
    /// or "high priority" silently means "last".
    #[test]
    fn priority_orders_the_way_the_queue_sorts() {
        use crate::Priority;

        assert!(Priority::Critical > Priority::High);
        assert!(Priority::High > Priority::Normal);
        assert!(Priority::Normal > Priority::Low);
        assert_eq!(Priority::Normal.as_i16(), 0);
        assert!(Priority::Low.as_i16() < 0);
    }

    /// The defaults are the ones the documentation quotes.
    #[test]
    fn the_defaults_are_what_the_docs_say() {
        use std::time::Duration;

        assert_eq!(crate::DEFAULT_QUEUE, "default");
        assert_eq!(crate::DEFAULT_RETRIES, 25);
        assert_eq!(crate::DEFAULT_TIMEOUT, Duration::from_secs(300));
    }

    /// `JobState::is_active` decides whether a row is swept, so the set of
    /// active states is worth pinning rather than reading off a `match`.
    #[test]
    fn only_unfinished_states_are_active() {
        use crate::JobState;

        assert!(JobState::Ready.is_active());
        assert!(JobState::Running.is_active());
        assert!(JobState::Retrying.is_active());
        assert!(!JobState::Done.is_active());
        assert!(!JobState::Dead.is_active());
        assert!(!JobState::Cancelled.is_active());
    }
}
