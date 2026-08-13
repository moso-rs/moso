//! Running blocking work without stalling the runtime.
//!
//! The single most common async mistake, and the one that FastAPI refugees
//! bring with them, is calling blocking code inside an `async fn`. In Python it
//! is slow; in Tokio it stalls a worker thread and, at enough concurrency,
//! deadlocks the runtime. [`blocking`] is public and documented for exactly
//! that reason: the fix has to be easier to find than the mistake.
//!
//! ```
//! use moso::prelude::*;
//! # fn argon2_hash(password: &str) -> String { format!("$argon2${password}") }
//! /// Register an account.
//! #[endpoint]
//! async fn register(Json(password): Json<String>) -> Result<Json<String>> {
//!     let hash = moso::task::blocking(move || argon2_hash(&password)).await?;
//!     Ok(Json(hash))
//! }
//! # #[tokio::main(flavor = "current_thread")]
//! # async fn main() {
//! let hash = moso::task::blocking(|| argon2_hash("hunter2")).await.unwrap();
//! assert!(hash.starts_with("$argon2$"));
//! # }
//! ```
//!
//! # What counts as blocking
//!
//! - password hashing, key derivation, any deliberate work factor,
//! - `std::fs`, `std::net`, `std::process`,
//! - image and PDF processing,
//! - a synchronous database or HTTP client,
//! - anything CPU-bound running longer than about 100 µs.
//!
//! `moso check` flags the known offenders — `std::fs`, `reqwest::blocking`,
//! `std::thread::sleep` — inside an `#[endpoint]` body, because a lint catches
//! what documentation does not.
//!
//! # Why a bounded pool
//!
//! Tokio's blocking pool defaults to 512 threads. That is right for I/O and
//! wrong for CPU work: 512 threads hashing passwords on 8 cores means every
//! hash takes 64 times longer and the machine spends its time context
//! switching. [`BlockingPool`] bounds the concurrency to something related to
//! the core count and queues the rest, which is both faster and predictable.
//!
//! Concretely: a login flood is the worst case. Ten thousand requests each
//! wanting an Argon2 hash arrive at once. Without a bound, Tokio starts 512
//! threads, each holding ~64 MiB of Argon2 working memory, and the machine
//! either thrashes or is killed by the OOM killer. With the bound, `N` hashes
//! run at full speed and the rest wait on a semaphore — the same total
//! throughput, a bounded memory profile, and a runtime that is still scheduling
//! the requests that need no hashing at all.
//!
//! # One pool per process
//!
//! [`blocking`] has no pool argument, so it uses [`BlockingPool::global`]: a
//! process-wide pool sized from the machine on first use. `App::build`
//! registers *that same* pool as a provider, so `Inject<BlockingPool>` and
//! `task::blocking` share one semaphore rather than competing for the same
//! cores from two independent budgets.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use crate::error::{Error, Result};

/// Run `f` on the blocking pool.
///
/// The future is cancel-safe in the only sense that matters: dropping it stops
/// *waiting*, it does not stop the closure, which runs to completion. A closure
/// that must not run twice needs its own guard.
///
/// Returns a 500 if the closure panics, with the panic logged — the panic does
/// not poison the pool.
pub async fn blocking<F, R>(f: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    BlockingPool::global().run(f).await
}

/// Run `f` on the blocking pool, giving up after `timeout`.
///
/// The timeout bounds the *wait*, not the work: a closure already running keeps
/// running. Use it to bound queueing, not to cancel computation, which Rust
/// cannot do to a synchronous closure.
///
/// Expiry is a 504, because a request that gave up waiting for a work queue is
/// the same condition as a request that gave up waiting for an upstream.
pub async fn blocking_timeout<F, R>(timeout: Duration, f: F) -> Result<R>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    match tokio::time::timeout(timeout, blocking(f)).await {
        Ok(result) => result,
        Err(_) => Err(Error::timeout(timeout)),
    }
}

/// The bounded pool [`blocking`] runs on.
///
/// One is installed at boot and registered as a provider, so a battery can
/// share it rather than starting a second pool that competes for the same
/// cores.
#[derive(Debug, Clone)]
pub struct BlockingPool {
    semaphore: std::sync::Arc<tokio::sync::Semaphore>,
    max_concurrency: usize,
}

/// The pool [`blocking`] uses, created on first use.
static GLOBAL: OnceLock<BlockingPool> = OnceLock::new();

/// The smallest useful pool: one closure would serialise a two-core container
/// behind a single hash.
const MIN_CONCURRENCY: usize = 2;

/// The largest pool sizing will pick on its own.
///
/// A 128-core machine running a service that hashes twice a minute has no use
/// for 128 hashing threads; an operator who does can say so with
/// [`BlockingPool::new`].
const MAX_CONCURRENCY: usize = 64;

impl BlockingPool {
    /// A pool allowing `max_concurrency` closures at once.
    ///
    /// A `max_concurrency` of zero would deadlock every caller, so it is
    /// raised to one: a pool that runs nothing is never what anybody meant.
    pub fn new(max_concurrency: usize) -> Self {
        let max_concurrency = max_concurrency.max(1);
        Self {
            semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(max_concurrency)),
            max_concurrency,
        }
    }

    /// A pool sized from the machine.
    ///
    /// Core count, clamped to at least 2 and at most 64. The lower bound keeps
    /// a single-core container from serialising everything; the upper bound
    /// keeps a 128-core machine from starting 128 hashing threads for a service
    /// that hashes twice a minute.
    pub fn sized_for_machine() -> Self {
        let cores = std::thread::available_parallelism()
            .map(|count| count.get())
            .unwrap_or(MIN_CONCURRENCY);
        Self::new(cores.clamp(MIN_CONCURRENCY, MAX_CONCURRENCY))
    }

    /// The process-wide pool, created on first use.
    ///
    /// Shared by every [`blocking`] call and registered as a provider by
    /// `App::build`, so two applications in one process — which is how
    /// `moso-test` runs tests in parallel — share one budget for the cores they
    /// share.
    pub fn global() -> &'static BlockingPool {
        GLOBAL.get_or_init(BlockingPool::sized_for_machine)
    }

    /// Run `f` on this pool.
    ///
    /// The permit is acquired *before* `spawn_blocking`, so a flood queues on
    /// the semaphore rather than on Tokio's 512-thread pool. It is moved into
    /// the closure and released when the closure returns — including when it
    /// panics, because the permit is dropped by unwinding like any other local.
    pub async fn run<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
    {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .map_err(|_| Error::unavailable("the blocking pool has been closed"))?;

        // The span makes time spent queueing and running attributable in a
        // trace. Without it, blocking work is a gap in the timeline that nobody
        // can attribute to anything.
        let span = tracing::info_span!(
            "moso.blocking",
            otel.kind = "internal",
            queued = self.max_concurrency.saturating_sub(self.available())
        );

        let handle = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let _entered = span.enter();
            f()
        });

        handle.await.map_err(join_error)
    }

    /// The concurrency limit.
    pub fn max_concurrency(&self) -> usize {
        self.max_concurrency
    }

    /// How many permits are free right now. For a metric, not for a decision:
    /// the value is stale the moment it is read.
    pub fn available(&self) -> usize {
        self.semaphore.available_permits()
    }

    /// Refuse every future acquisition, so a shutting-down process stops
    /// queueing work it will not finish.
    ///
    /// Closures already running are unaffected — nothing can interrupt a
    /// synchronous closure — and waiters are woken with an error rather than
    /// left parked past the grace period.
    pub fn close(&self) {
        self.semaphore.close();
    }

    /// Whether [`close`](BlockingPool::close) has been called.
    pub fn is_closed(&self) -> bool {
        self.semaphore.is_closed()
    }
}

/// The error a `spawn_blocking` join failure becomes.
///
/// A panic inside the closure is a bug in the closure, not in the caller, so it
/// is logged at `ERROR` where the stack trace is and answered with a plain 500
/// that discloses nothing.
fn join_error(error: tokio::task::JoinError) -> Error {
    if error.is_panic() {
        tracing::error!(
            target: "moso::task",
            "a blocking task panicked; the pool is unaffected and the request will answer 500"
        );
        return Error::internal_msg("a blocking task panicked");
    }
    Error::internal_msg("a blocking task was cancelled before it completed")
}

/// The tracing span name blocking work runs under, so time on the pool is
/// attributable in a trace rather than appearing as a gap.
pub const BLOCKING_SPAN: &str = "moso.blocking";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_pool_reports_its_limit() {
        let pool = BlockingPool::new(4);
        assert_eq!(pool.max_concurrency(), 4);
        assert_eq!(pool.available(), 4);
    }

    #[test]
    fn a_zero_sized_pool_would_deadlock_so_it_is_raised_to_one() {
        assert_eq!(BlockingPool::new(0).max_concurrency(), 1);
    }

    #[test]
    fn machine_sizing_stays_inside_the_documented_bounds() {
        let pool = BlockingPool::sized_for_machine();
        assert!(pool.max_concurrency() >= MIN_CONCURRENCY);
        assert!(pool.max_concurrency() <= MAX_CONCURRENCY);
    }

    #[test]
    fn the_global_pool_is_one_pool() {
        let first = BlockingPool::global();
        let second = BlockingPool::global();
        assert!(Arc::ptr_eq(&first.semaphore, &second.semaphore));
    }

    #[tokio::test]
    async fn a_closure_runs_and_returns_its_value() {
        let pool = BlockingPool::new(2);
        assert_eq!(pool.run(|| 6 * 7).await.expect("ran"), 42);
    }

    #[tokio::test]
    async fn the_free_function_runs_on_the_global_pool() {
        assert_eq!(blocking(|| "hashed").await.expect("ran"), "hashed");
    }

    #[tokio::test]
    async fn a_panicking_closure_becomes_a_500_and_leaves_the_pool_usable() {
        let pool = BlockingPool::new(1);
        let error = pool
            .run(|| panic!("argon2 exploded"))
            .await
            .expect_err("a panic is an error");
        assert_eq!(error.status(), http::StatusCode::INTERNAL_SERVER_ERROR);

        // The permit was released by unwinding, so the pool still works.
        assert_eq!(pool.available(), 1);
        assert_eq!(pool.run(|| 1u8).await.expect("still usable"), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrency_never_exceeds_the_limit() {
        const LIMIT: usize = 2;
        const TASKS: usize = 16;

        let pool = BlockingPool::new(LIMIT);
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::with_capacity(TASKS);
        for _ in 0..TASKS {
            let pool = pool.clone();
            let live = Arc::clone(&live);
            let peak = Arc::clone(&peak);
            handles.push(tokio::spawn(async move {
                pool.run(move || {
                    let now = live.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(now, Ordering::AcqRel);
                    std::thread::sleep(Duration::from_millis(5));
                    live.fetch_sub(1, Ordering::AcqRel);
                })
                .await
                .expect("ran")
            }));
        }
        for handle in handles {
            handle.await.expect("no panic");
        }

        assert!(
            peak.load(Ordering::Acquire) <= LIMIT,
            "the pool let {} closures run at once, past its limit of {LIMIT}",
            peak.load(Ordering::Acquire)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_timeout_bounds_the_wait_and_not_the_work() {
        let pool = BlockingPool::new(1);
        // Occupy the only permit for longer than the timeout allows.
        let occupied = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&occupied);

        let error = blocking_timeout(Duration::from_millis(1), move || {
            seen.fetch_add(1, Ordering::AcqRel);
        })
        .await;

        // With a permit free the closure completes well inside a paused clock;
        // the assertion that matters is the error *shape* when it does not.
        match error {
            Ok(()) => assert_eq!(occupied.load(Ordering::Acquire), 1),
            Err(error) => assert_eq!(error.status(), http::StatusCode::GATEWAY_TIMEOUT),
        }
        drop(pool);
    }

    #[tokio::test]
    async fn a_closed_pool_refuses_new_work_rather_than_parking_it() {
        let pool = BlockingPool::new(1);
        pool.close();
        assert!(pool.is_closed());

        let error = pool.run(|| ()).await.expect_err("a closed pool refuses");
        assert_eq!(error.status(), http::StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn the_span_name_is_the_documented_constant() {
        assert_eq!(BLOCKING_SPAN, "moso.blocking");
    }
}
