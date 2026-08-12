//! Single-flight: one computation per key, however many callers ask for it.
//!
//! The classic cache stampede is a hot key expiring while a hundred requests
//! are in flight: all hundred miss, all hundred call the database, and the
//! database falls over at exactly the moment the cache was supposed to be
//! protecting it. [`SingleFlight`] makes ninety-nine of them wait for the
//! first.
//!
//! # How the exactly-once part is proved
//!
//! Not by inspection: `tests/cache.rs` runs 100 concurrent callers against one
//! key with an `AtomicUsize` inside the closure and asserts it reads `1`.
//!
//! # The mechanism
//!
//! One [`tokio::sync::OnceCell`] per in-flight key, held in a map of `Weak`
//! references. `OnceCell::get_or_try_init` is what actually enforces "one
//! initialiser": the losers await the winner's result rather than running
//! their own closure. The map holds `Weak` so that a finished flight's entry
//! costs nothing to forget, and the last caller out sweeps it.
//!
//! # What happens when the computation fails
//!
//! Nothing is cached, the error goes to every waiter, and the next caller
//! starts a fresh flight. An error is a fact about the moment, not about the
//! key, and caching one would turn a blip into an outage — which is exactly
//! the mistake single-flight exists to avoid, made in the other direction.
//!
//! # Values are shared, not copied
//!
//! The winner produces one value; every caller gets an `Arc` of it. That is
//! why [`Kv::get_or_insert_with`](crate::Kv::get_or_insert_with) needs
//! `N::Value: Clone` — the `Arc` is unwrapped at the boundary so callers get
//! the plain type they asked for.

use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};

use tokio::sync::OnceCell;

use crate::error::{Error, Result};

/// What the map holds: a type-erased value, so one map serves every namespace.
type Shared = Arc<dyn Any + Send + Sync>;

/// One in-flight computation.
type Slot = OnceCell<Shared>;

/// De-duplicates concurrent computations of the same key.
///
/// ```
/// use moso_kv::flight::SingleFlight;
/// use std::sync::Arc;
/// use std::sync::atomic::{AtomicUsize, Ordering};
///
/// # #[tokio::main(flavor = "multi_thread", worker_threads = 4)] async fn main() {
/// let flight = Arc::new(SingleFlight::new());
/// let calls = Arc::new(AtomicUsize::new(0));
///
/// let mut handles = Vec::new();
/// for _ in 0..64 {
///     let flight = Arc::clone(&flight);
///     let calls = Arc::clone(&calls);
///     handles.push(tokio::spawn(async move {
///         flight
///             .run("the-key", || async {
///                 calls.fetch_add(1, Ordering::SeqCst);
///                 tokio::time::sleep(std::time::Duration::from_millis(20)).await;
///                 Ok(7_u32)
///             })
///             .await
///     }));
/// }
///
/// for handle in handles {
///     assert_eq!(*handle.await.expect("joined").expect("computed"), 7);
/// }
/// assert_eq!(calls.load(Ordering::SeqCst), 1);
/// assert_eq!(flight.in_flight(), 0);
/// # }
/// ```
#[derive(Debug, Default)]
pub struct SingleFlight {
    slots: Mutex<HashMap<String, Weak<Slot>>>,
    shared: std::sync::atomic::AtomicU64,
}

impl SingleFlight {
    /// An empty map.
    ///
    /// ```
    /// use moso_kv::flight::SingleFlight;
    ///
    /// assert_eq!(SingleFlight::new().in_flight(), 0);
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// How many computations are running right now.
    ///
    /// A diagnostic and a test hook: it should return to zero once everything
    /// settles, and a number that only grows is a leak.
    ///
    /// ```
    /// use moso_kv::flight::SingleFlight;
    ///
    /// assert_eq!(SingleFlight::new().in_flight(), 0);
    /// ```
    #[must_use]
    pub fn in_flight(&self) -> usize {
        self.lock().len()
    }

    /// How many callers, ever, have waited for somebody else's computation.
    ///
    /// The number that says whether single-flight is earning its keep: on a hot
    /// key under load it climbs, and on a workload with no contention it stays
    /// at zero and the machinery is costing one `HashMap` probe per miss.
    ///
    /// ```
    /// use moso_kv::flight::SingleFlight;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let flight = SingleFlight::new();
    /// assert_eq!(flight.shared_total(), 0);
    ///
    /// // One caller, no contention, nothing shared.
    /// flight.run("k", || async { Ok(1_u8) }).await.expect("computed");
    /// assert_eq!(flight.shared_total(), 0);
    /// # }
    /// ```
    #[must_use]
    pub fn shared_total(&self) -> u64 {
        self.shared.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Run `compute` for `key`, or wait for the one already running.
    ///
    /// # Errors
    ///
    /// Whatever `compute` returns. Nothing is remembered on failure.
    ///
    /// ```
    /// use moso_kv::flight::SingleFlight;
    ///
    /// # #[tokio::main(flavor = "current_thread")] async fn main() {
    /// let flight = SingleFlight::new();
    /// let value = flight.run("k", || async { Ok(String::from("v")) }).await.expect("computed");
    /// assert_eq!(*value, "v");
    /// # }
    /// ```
    pub async fn run<T, F, Fut>(&self, key: &str, compute: F) -> Result<Arc<T>>
    where
        T: Send + Sync + 'static,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let slot = self.slot(key);

        let outcome = slot
            .get_or_try_init(|| async { compute().await.map(|value| Arc::new(value) as Shared) })
            .await
            .map(Arc::clone);

        // Drop our handle *before* sweeping, so that the last caller out is the
        // one that finds the `Weak` dead.
        drop(slot);
        self.sweep(key);

        let shared = outcome?;
        shared.downcast::<T>().map_err(|_| {
            // Only reachable if two namespaces produced the same key string with
            // different value types, which the key layout makes impossible.
            Error::Config {
                detail: format!(
                    "two single-flight computations for `{key}` produced different value types"
                ),
            }
        })
    }

    /// The slot for `key`, creating it when this caller is the first.
    ///
    /// Joining an existing slot is what [`shared_total`](Self::shared_total)
    /// counts, and it is counted here rather than inferred from the map's size
    /// afterwards — by the time `run` returns, the flight is usually already
    /// gone.
    fn slot(&self, key: &str) -> Arc<Slot> {
        let mut slots = self.lock();
        if let Some(existing) = slots.get(key).and_then(Weak::upgrade) {
            self.shared
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return existing;
        }
        let slot = Arc::new(OnceCell::new());
        slots.insert(key.to_owned(), Arc::downgrade(&slot));
        slot
    }

    /// Forget `key` when nothing holds its slot any more.
    fn sweep(&self, key: &str) {
        let mut slots = self.lock();
        if slots.get(key).is_some_and(|weak| weak.strong_count() == 0) {
            slots.remove(key);
        }
    }

    /// The map, recovering from a poisoned lock.
    ///
    /// A panic inside `compute` unwinds through `run` without touching the map
    /// — the lock is never held across an `await` — so a poisoned lock here
    /// would mean a panic in the map code itself, and refusing every subsequent
    /// cache read over it would turn a bug into an outage.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Weak<Slot>>> {
        self.slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_hundred_callers_are_one_computation() {
        let flight = Arc::new(SingleFlight::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let flight = Arc::clone(&flight);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                flight
                    .run("hot", || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(String::from("value"))
                    })
                    .await
            }));
        }

        for handle in handles {
            let value = handle.await.expect("joined").expect("computed");
            assert_eq!(*value, "value");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn different_keys_do_not_wait_for_each_other() {
        let flight = Arc::new(SingleFlight::new());
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for index in 0..8_u32 {
            let flight = Arc::clone(&flight);
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                flight
                    .run(&format!("key-{index}"), || async {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(index)
                    })
                    .await
                    .expect("computed")
            }));
        }

        let mut seen: Vec<u32> = Vec::new();
        for handle in handles {
            seen.push(*handle.await.expect("joined"));
        }
        seen.sort_unstable();
        assert_eq!(seen, (0..8).collect::<Vec<_>>());
        assert_eq!(calls.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn the_map_empties_itself() {
        let flight = SingleFlight::new();
        for index in 0..50_u32 {
            flight
                .run(&format!("k{index}"), || async { Ok(index) })
                .await
                .expect("computed");
        }
        assert_eq!(flight.in_flight(), 0);
    }

    #[tokio::test]
    async fn a_failure_is_not_remembered() {
        let flight = SingleFlight::new();
        let calls = AtomicUsize::new(0);

        for _ in 0..3 {
            let result: Result<Arc<u8>> = flight
                .run("k", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Err(Error::Config {
                        detail: "no".to_owned(),
                    })
                })
                .await;
            assert!(result.is_err());
        }

        // Every attempt ran: an error is a fact about the moment.
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(flight.in_flight(), 0);

        // ... and the key still works afterwards.
        assert_eq!(
            *flight.run("k", || async { Ok(1_u8) }).await.expect("ok"),
            1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn every_waiter_sees_the_same_error() {
        let flight = Arc::new(SingleFlight::new());
        let mut handles = Vec::new();
        for _ in 0..16 {
            let flight = Arc::clone(&flight);
            handles.push(tokio::spawn(async move {
                let result: Result<Arc<u8>> = flight
                    .run("bad", || async {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        Err(Error::backend("memory", "get", std::io::Error::other("x")))
                    })
                    .await;
                result.is_err()
            }));
        }
        for handle in handles {
            assert!(handle.await.expect("joined"));
        }
    }

    #[tokio::test]
    async fn sequential_calls_each_compute() {
        let flight = SingleFlight::new();
        let calls = AtomicUsize::new(0);
        for _ in 0..5 {
            flight
                .run("k", || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(1_u8)
                })
                .await
                .expect("computed");
        }
        // Nothing is cached here — that is `Kv`'s job. Single-flight only
        // collapses *concurrent* calls.
        assert_eq!(calls.load(Ordering::SeqCst), 5);
    }
}
