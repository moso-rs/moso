//! The [`Kv`] layer, against every backend.
//!
//! Where `tests/conformance.rs` exercises [`KvStore`](moso_kv::KvStore), this
//! file exercises what is built on top of it: single-flight caching, negative
//! caching, stale-while-revalidate, distributed locks and rate limiting. Those
//! are the four things that are easy to get subtly wrong and impossible to
//! notice, so each one is asserted against a **counter or a clock**, not
//! against a shape.
//!
//! Acceptance criteria 3, 4 and 7 of `docs/02-data/25-kv-cache.md` live here.
//!
//! Run all three backends with:
//!
//! ```text
//! export DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test
//! export REDIS_URL=redis://localhost:56379
//! cargo test -p moso-kv --features redis,pg-kv --test cache
//! ```

#![allow(clippy::print_stdout, reason = "a skipped test has to say why")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use moso_kv::backend::MemoryStore;
use moso_kv::breaker::BreakerConfig;
use moso_kv::{Kv, KvStore, LockOptions, RateQuota, minutes, seconds};

moso_kv::namespace! {
    /// A profile, cached, with negative caching.
    pub Profile: u64 => Option<String>, ttl = minutes(5), negative_ttl = seconds(2);

    /// A dashboard number, for the stale-while-revalidate test.
    pub Dashboard: u64 => Option<u64>, ttl = minutes(5);
}

/// A keyspace nothing else is using, so the shared servers stay usable.
fn unique_app() -> String {
    use std::sync::atomic::AtomicU64;
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("c{nanos}x{seq}")
}

/// Build a handle over `store` in its own keyspace.
fn handle(store: Arc<dyn KvStore>) -> Kv {
    Kv::builder(unique_app())
        .shared_store(store)
        // The suite is about the cache layer, not about the breaker; the
        // breaker has its own tests in `tests/degrade.rs`.
        .breaker(BreakerConfig::never())
        .build()
        .expect("built")
}

/// Every backend that is available in this run.
async fn backends() -> Vec<Arc<dyn KvStore>> {
    // `mut` is used only when a backend feature is on, and the whole point of
    // this function is that the set is decided by cargo features.
    #[allow(unused_mut, reason = "the redis and postgres legs are cargo features")]
    let mut out: Vec<Arc<dyn KvStore>> = vec![Arc::new(MemoryStore::new())];

    #[cfg(feature = "redis")]
    match std::env::var("REDIS_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        Some(url) => out.push(Arc::new(
            moso_kv::backend::RedisStore::connect(moso_kv::backend::RedisConfig::new(url))
                .await
                .expect("connected to REDIS_URL"),
        )),
        None => println!("skipping redis: REDIS_URL is not set"),
    }

    #[cfg(feature = "pg-kv")]
    match std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    {
        Some(url) => out.push(Arc::new(
            moso_kv::backend::PostgresStore::connect(
                &url,
                "moso_kv_test",
                8,
                Duration::from_secs(10),
            )
            .await
            .expect("connected to DATABASE_URL"),
        )),
        None => println!("skipping postgres: DATABASE_URL is not set"),
    }

    out
}

// ---------------------------------------------------------------------------
// Acceptance criterion 3: single-flight
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hundred_concurrent_callers_are_one_computation() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..100 {
            let kv = kv.clone();
            let calls = Arc::clone(&calls);
            handles.push(tokio::spawn(async move {
                kv.get_or_insert_with::<Profile, _, _>(&1, || async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    // Long enough that every one of the hundred is inside the
                    // flight before the first finishes.
                    tokio::time::sleep(Duration::from_millis(80)).await;
                    Ok(Some(String::from("built")))
                })
                .await
            }));
        }

        for handle in handles {
            assert_eq!(
                handle.await.expect("joined").expect("value"),
                Some(String::from("built")),
                "{name}"
            );
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the computation ran more than once"
        );
        assert!(
            kv.stats().flights_shared > 0,
            "{name}: nobody waited for the leader, so the counter is lying"
        );

        kv.clear_namespace::<Profile>().await.expect("cleaned up");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cached_none_stops_the_stampede_it_would_otherwise_cause() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);
        let calls = AtomicUsize::new(0);

        for _ in 0..5 {
            let value = kv
                .get_or_insert_with::<Profile, _, _>(&2, || async {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(None)
                })
                .await
                .expect("value");
            assert_eq!(value, None, "{name}");
        }
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the `None` was not cached"
        );

        // ... under the *negative* ttl, which is shorter.
        let ttl = kv.ttl::<Profile>(&2).await.expect("ttl").expect("a ttl");
        assert!(ttl <= seconds(2), "{name}: {ttl:?}");

        kv.clear_namespace::<Profile>().await.expect("cleaned up");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_while_revalidate_serves_now_and_refreshes_behind() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        kv.set::<Dashboard>(&1, &Some(1)).await.expect("set");
        tokio::time::sleep(Duration::from_millis(60)).await;

        let started = Instant::now();
        let served = kv
            .get_swr::<Dashboard, _, _>(&1, Duration::from_millis(20), || async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(Some(2))
            })
            .await
            .expect("swr");

        assert_eq!(served, Some(1), "{name}: the stale value is served");
        assert!(
            started.elapsed() < Duration::from_millis(150),
            "{name}: the caller waited for the refresh"
        );

        // The refresh lands afterwards.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if kv.get::<Dashboard>(&1).await.expect("get") == Some(Some(2)) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: the background revalidation never landed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        kv.clear_namespace::<Dashboard>().await.expect("cleaned up");
    }
}

// ---------------------------------------------------------------------------
// Acceptance criterion 4: the rate limiter
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ten_a_minute_admits_exactly_ten_across_four_workers() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);
        let quota = RateQuota::new(10, Duration::from_secs(60));
        let admitted = Arc::new(AtomicUsize::new(0));

        let mut workers = Vec::new();
        for _ in 0..4 {
            let kv = kv.clone();
            let admitted = Arc::clone(&admitted);
            workers.push(tokio::spawn(async move {
                for _ in 0..25 {
                    if kv
                        .rate_limit("login", quota)
                        .await
                        .expect("decided")
                        .allowed
                    {
                        admitted.fetch_add(1, Ordering::SeqCst);
                    }
                }
            }));
        }
        for worker in workers {
            worker.await.expect("joined");
        }

        assert_eq!(
            admitted.load(Ordering::SeqCst),
            10,
            "{name}: a quota of ten admitted the wrong number"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rate_limit_headers_are_correct() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);
        let quota = RateQuota::new(3, Duration::from_secs(60));

        let mut decisions = Vec::new();
        for _ in 0..4 {
            decisions.push(kv.rate_limit("headers", quota).await.expect("decided"));
        }

        assert_eq!(
            decisions.iter().map(|d| d.remaining).collect::<Vec<_>>(),
            vec![2, 1, 0, 0],
            "{name}: remaining did not count down"
        );

        let allowed = decisions[0].headers();
        assert_eq!(allowed.len(), 3, "{name}: no Retry-After when allowed");
        assert_eq!(allowed[0].1, "3", "{name}: X-RateLimit-Limit");
        assert_eq!(allowed[1].1, "2", "{name}: X-RateLimit-Remaining");

        let denied = &decisions[3];
        assert!(!denied.allowed, "{name}");
        let headers = denied.headers();
        assert_eq!(headers.len(), 4, "{name}: Retry-After on a 429");
        assert_eq!(headers[1].1, "0", "{name}");
        assert!(denied.retry_after > Duration::ZERO, "{name}");
        assert!(denied.reset > Duration::ZERO, "{name}");

        let error = denied.into_error();
        assert_eq!(
            error.status(),
            http::StatusCode::TOO_MANY_REQUESTS,
            "{name}"
        );
    }
}

// ---------------------------------------------------------------------------
// Acceptance criterion 7: locks
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lock_excludes_and_releases_on_drop() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        {
            let guard = kv
                .lock_with("import", LockOptions::new(Duration::from_secs(30)))
                .await
                .expect("acquired");
            assert!(guard.is_held().await.expect("held"), "{name}");
            assert!(
                kv.try_lock("import", Duration::from_secs(30))
                    .await
                    .expect("try")
                    .is_none(),
                "{name}: two holders at once"
            );
        }

        // The drop spawns the release, so wait for it.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(guard) = kv
                .try_lock("import", Duration::from_secs(30))
                .await
                .expect("try")
            {
                guard.release().await.expect("released");
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: the lock never released on drop"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lock_releases_on_panic() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        let task = {
            let kv = kv.clone();
            tokio::spawn(async move {
                let _guard = kv
                    .lock_with("panicky", LockOptions::new(Duration::from_secs(30)))
                    .await
                    .expect("acquired");
                panic!("the work failed");
            })
        };
        assert!(task.await.is_err(), "{name}: the task should have panicked");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if kv
                .try_lock("panicky", Duration::from_secs(30))
                .await
                .expect("try")
                .is_some()
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "{name}: a panicking holder did not release its lock"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lease_frees_a_lock_whose_holder_vanished() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        let guard = kv
            .lock_with(
                "abandoned",
                LockOptions::new(Duration::from_millis(300)).no_renew(),
            )
            .await
            .expect("acquired");

        // Forgetting the guard is what a process exit amounts to: no `Drop`,
        // no release, only the lease.
        std::mem::forget(guard);
        assert!(
            kv.try_lock("abandoned", Duration::from_secs(1))
                .await
                .expect("try")
                .is_none(),
            "{name}: the lock was free before its lease ran out"
        );

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(
            kv.try_lock("abandoned", Duration::from_secs(1))
                .await
                .expect("try")
                .is_some(),
            "{name}: the lease did not free the lock"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_lease_outlives_its_nominal_length() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        let guard = kv
            .lock_with("renewed", LockOptions::new(Duration::from_millis(300)))
            .await
            .expect("acquired");

        // Three leases' worth. Without renewal this would have lapsed twice.
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(
            guard.is_held().await.expect("held"),
            "{name}: auto-renewal did not keep the lease alive"
        );
        guard.release().await.expect("released");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn only_one_of_many_contenders_holds_a_lock() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);
        let held = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));

        let mut workers = Vec::new();
        for _ in 0..8 {
            let kv = kv.clone();
            let held = Arc::clone(&held);
            let peak = Arc::clone(&peak);
            workers.push(tokio::spawn(async move {
                let mut acquired = 0_usize;
                for _ in 0..5 {
                    let Some(guard) = kv
                        .try_lock("contended", Duration::from_secs(5))
                        .await
                        .expect("try")
                    else {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        continue;
                    };

                    let now = held.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    held.fetch_sub(1, Ordering::SeqCst);

                    guard.release().await.expect("released");
                    acquired += 1;
                }
                acquired
            }));
        }

        let mut total = 0;
        for worker in workers {
            total += worker.await.expect("joined");
        }

        assert!(total > 0, "{name}: nobody ever got the lock");
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "{name}: two holders were inside the lock at once"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_fencing_token_strictly_increases_across_holders() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        let mut previous = 0_i64;
        for _ in 0..5 {
            let guard = kv
                .lock_with("fenced", LockOptions::new(Duration::from_secs(30)))
                .await
                .expect("acquired");
            assert!(
                guard.token() > previous,
                "{name}: token {} did not exceed {previous}",
                guard.token()
            );
            previous = guard.token();
            guard.release().await.expect("released");
        }
    }
}

// ---------------------------------------------------------------------------
// The typed layer
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_namespace_round_trips_and_stays_inside_itself() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        assert_eq!(kv.get::<Profile>(&7).await.expect("get"), None, "{name}");
        kv.set::<Profile>(&7, &Some(String::from("alice")))
            .await
            .expect("set");
        assert_eq!(
            kv.get::<Profile>(&7).await.expect("get"),
            Some(Some(String::from("alice"))),
            "{name}"
        );

        // Another namespace with the same key is a different value.
        kv.set::<Dashboard>(&7, &Some(1)).await.expect("set");
        assert_eq!(
            kv.get::<Profile>(&7).await.expect("get"),
            Some(Some(String::from("alice"))),
            "{name}: the namespaces collided"
        );

        assert_eq!(
            kv.key::<Profile>(&7).expect("short").as_str(),
            format!("moso:v1:{}:profile:1:7", kv.app()),
            "{name}"
        );

        assert_eq!(kv.clear_namespace::<Profile>().await.expect("clear"), 1);
        assert!(
            kv.exists::<Dashboard>(&7).await.expect("exists"),
            "{name}: clearing one namespace emptied another"
        );
        kv.clear_namespace::<Dashboard>().await.expect("clear");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_counters_say_what_happened() {
    for store in backends().await {
        let name = store.name();
        let kv = handle(store);

        let _ = kv.get::<Profile>(&99).await.expect("get");
        kv.set::<Profile>(&99, &Some(String::from("x")))
            .await
            .expect("set");
        let _ = kv.get::<Profile>(&99).await.expect("get");

        let stats = kv.stats();
        assert_eq!(stats.flights_shared, 0, "{name}: nothing was contended");
        assert_eq!(stats.misses, 1, "{name}");
        assert_eq!(stats.hits, 1, "{name}");
        assert_eq!(stats.writes, 1, "{name}");
        assert_eq!(stats.errors, 0, "{name}");
        assert_eq!(stats.degraded, 0, "{name}");
        assert!((stats.hit_ratio() - 0.5).abs() < f64::EPSILON, "{name}");

        kv.clear_namespace::<Profile>().await.expect("clear");
    }
}
