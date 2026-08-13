//! The same suite, against every backend.
//!
//! Acceptance criterion 1 of `docs/02-data/25-kv-cache.md`: *the same test
//! suite passes against `memory`, `redis` and `postgres`, except for tests
//! gated on `capabilities()`*. This file is that suite. It is written once and
//! run three times.
//!
//! # How to run all three
//!
//! ```text
//! docker compose -f compose.test.yaml up -d --wait
//! export DATABASE_URL=postgres://moso:moso@localhost:55433/moso_test
//! export REDIS_URL=redis://localhost:56379
//! cargo test -p moso-kv --features redis,pg-kv
//! ```
//!
//! With neither variable set, the Redis and PostgreSQL legs **skip** with a
//! message rather than failing, so the suite still passes on a machine with no
//! Docker. The memory leg always runs.
//!
//! # Nothing here is mocked
//!
//! Every assertion below runs against a real store. The one place a fake
//! appears in this crate is `tests/degrade.rs`, where the *point* is to make a
//! healthy backend fail on demand, and a mock is the only way to do that
//! without stopping a container mid-test.

#![allow(clippy::print_stdout, reason = "a skipped test has to say why")]

use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use moso_kv::backend::MemoryStore;
use moso_kv::{Key, KvStore, ScanCursor, SetOpts, Side};

/// A keyspace nothing else in this process is using.
///
/// The PostgreSQL and Redis legs share one server between test binaries and
/// between runs, so every run gets its own third segment.
fn unique_app() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.subsec_nanos());
    let seq = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("t{nanos}x{seq}")
}

/// A key in this run's keyspace.
fn key(app: &str, name: &str) -> Key {
    Key::from_raw(format!("moso:v1:{app}:demo:1:{name}")).expect("a valid key")
}

/// The prefix every key from [`key`] starts with.
fn prefix(app: &str) -> Key {
    Key::from_raw(format!("moso:v1:{app}:demo:1:")).expect("a valid key")
}

fn bytes(value: &str) -> Bytes {
    Bytes::copy_from_slice(value.as_bytes())
}

// ---------------------------------------------------------------------------
// The suite
// ---------------------------------------------------------------------------

/// Everything every backend must do.
async fn core_operations(store: &dyn KvStore, app: &str) {
    let key = key(app, "core");

    assert_eq!(store.get(&key).await.expect("get"), None);
    assert!(!store.exists(&key).await.expect("exists"));
    assert_eq!(store.ttl(&key).await.expect("ttl"), None);
    assert!(!store.delete(&key).await.expect("delete"));

    assert!(
        store
            .set(&key, bytes("one"), SetOpts::new())
            .await
            .expect("set")
    );
    assert_eq!(
        store.get(&key).await.expect("get"),
        Some(bytes("one")),
        "{}: a written value reads back",
        store.name()
    );
    assert!(store.exists(&key).await.expect("exists"));
    assert_eq!(
        store.ttl(&key).await.expect("ttl"),
        None,
        "{}: a value written with no ttl has none",
        store.name()
    );

    assert!(
        store
            .set(&key, bytes("two"), SetOpts::new())
            .await
            .expect("set")
    );
    assert_eq!(store.get(&key).await.expect("get"), Some(bytes("two")));

    assert!(store.delete(&key).await.expect("delete"));
    assert!(!store.delete(&key).await.expect("delete"));
    assert_eq!(store.get(&key).await.expect("get"), None);
}

/// Binary values survive, including empty ones and non-UTF-8 ones.
async fn binary_values(store: &dyn KvStore, app: &str) {
    for (name, value) in [
        ("empty", Bytes::new()),
        ("nul", Bytes::from_static(b"\x00\x01\x02")),
        ("high", Bytes::from_static(b"\xff\xfe")),
        ("utf8", Bytes::from_static("héllo ☃".as_bytes())),
    ] {
        let key = key(app, name);
        store
            .set(&key, value.clone(), SetOpts::new())
            .await
            .expect("set");
        assert_eq!(
            store.get(&key).await.expect("get"),
            Some(value),
            "{}: `{name}` did not round trip",
            store.name()
        );
        store.delete(&key).await.expect("delete");
    }
}

/// Acceptance criterion 6: TTLs are honoured within 50 ms on all backends.
async fn ttl_is_honoured_promptly(store: &dyn KvStore, app: &str) {
    let key = key(app, "ttl");
    store
        .set(
            &key,
            bytes("v"),
            SetOpts::new().ttl(Duration::from_millis(120)),
        )
        .await
        .expect("set");

    let remaining = store.ttl(&key).await.expect("ttl").expect("a ttl");
    assert!(
        remaining <= Duration::from_millis(120),
        "{}: reported {remaining:?}",
        store.name()
    );
    assert!(store.exists(&key).await.expect("exists"));

    // Just before the deadline it is still there ...
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(
        store.exists(&key).await.expect("exists"),
        "{}: expired early",
        store.name()
    );

    // ... and within 50 ms of it, gone.
    let deadline = Instant::now() + Duration::from_millis(110);
    loop {
        if !store.exists(&key).await.expect("exists") {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}: still visible more than 50 ms after its ttl",
            store.name()
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    assert_eq!(store.get(&key).await.expect("get"), None);
    assert_eq!(store.ttl(&key).await.expect("ttl"), None);
    store.delete(&key).await.expect("delete");
}

/// `if_absent`, `if_present`, `keep_ttl`, and `expire`.
async fn conditional_writes(store: &dyn KvStore, app: &str) {
    let key = key(app, "conditional");
    store.delete(&key).await.expect("delete");

    assert!(
        !store
            .set(&key, bytes("a"), SetOpts::new().if_present())
            .await
            .expect("set"),
        "{}: `if_present` wrote an absent key",
        store.name()
    );
    assert!(
        store
            .set(&key, bytes("a"), SetOpts::new().if_absent())
            .await
            .expect("set")
    );
    assert!(
        !store
            .set(&key, bytes("b"), SetOpts::new().if_absent())
            .await
            .expect("set"),
        "{}: `if_absent` overwrote a present key",
        store.name()
    );
    assert_eq!(store.get(&key).await.expect("get"), Some(bytes("a")));
    assert!(
        store
            .set(&key, bytes("c"), SetOpts::new().if_present())
            .await
            .expect("set")
    );
    assert_eq!(store.get(&key).await.expect("get"), Some(bytes("c")));

    // `expire` on a live key, and on an absent one.
    assert!(
        store
            .expire(&key, Duration::from_secs(60))
            .await
            .expect("expire")
    );
    assert!(store.ttl(&key).await.expect("ttl").is_some());
    assert!(
        !store
            .expire(
                &key.joined("missing").expect("short"),
                Duration::from_secs(60)
            )
            .await
            .expect("expire")
    );

    // `keep_ttl` keeps it; a plain write drops it.
    store
        .set(&key, bytes("d"), SetOpts::new().keep_ttl())
        .await
        .expect("set");
    assert!(
        store.ttl(&key).await.expect("ttl").is_some(),
        "{}: `keep_ttl` dropped the expiry",
        store.name()
    );
    store
        .set(&key, bytes("e"), SetOpts::new())
        .await
        .expect("set");
    assert_eq!(
        store.ttl(&key).await.expect("ttl"),
        None,
        "{}: a plain write kept the expiry",
        store.name()
    );

    store.delete(&key).await.expect("delete");
}

/// `incr` creates at zero, and only sets a TTL when there is not one.
async fn counters(store: &dyn KvStore, app: &str) {
    let key = key(app, "counter");
    store.delete(&key).await.expect("delete");

    assert_eq!(store.incr(&key, 1, None).await.expect("incr"), 1);
    assert_eq!(store.incr(&key, 4, None).await.expect("incr"), 5);
    assert_eq!(store.incr(&key, -5, None).await.expect("incr"), 0);
    assert_eq!(
        store.get(&key).await.expect("get"),
        Some(bytes("0")),
        "{}: a counter is decimal ASCII, so `INCR` and `GET` agree",
        store.name()
    );

    store.delete(&key).await.expect("delete");
    assert_eq!(
        store
            .incr(&key, 7, Some(Duration::from_secs(60)))
            .await
            .expect("incr"),
        7
    );
    let first = store.ttl(&key).await.expect("ttl").expect("a ttl");

    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        store
            .incr(&key, 1, Some(Duration::from_secs(60)))
            .await
            .expect("incr"),
        8
    );
    let second = store.ttl(&key).await.expect("ttl").expect("still a ttl");
    assert!(
        second <= first,
        "{}: the counter's window slid forward; it must not",
        store.name()
    );

    store.delete(&key).await.expect("delete");
}

/// The three compare-and-swap operations.
async fn compare_and_swap(store: &dyn KvStore, app: &str) {
    if !store.capabilities().atomic_cas {
        return;
    }
    let key = key(app, "cas");
    store.delete(&key).await.expect("delete");

    assert!(
        store
            .compare_and_swap(&key, None, bytes("1"), SetOpts::new())
            .await
            .expect("cas"),
        "{}: absent -> present",
        store.name()
    );
    assert!(
        !store
            .compare_and_swap(&key, None, bytes("2"), SetOpts::new())
            .await
            .expect("cas"),
        "{}: it is no longer absent",
        store.name()
    );
    assert!(
        !store
            .compare_and_swap(&key, Some(b"wrong"), bytes("2"), SetOpts::new())
            .await
            .expect("cas")
    );
    assert!(
        store
            .compare_and_swap(&key, Some(b"1"), bytes("2"), SetOpts::new())
            .await
            .expect("cas")
    );
    assert_eq!(store.get(&key).await.expect("get"), Some(bytes("2")));

    // Compare-and-expire renews only our own value.
    assert!(
        !store
            .compare_and_expire(&key, b"1", Duration::from_secs(60))
            .await
            .expect("cae")
    );
    assert!(
        store
            .compare_and_expire(&key, b"2", Duration::from_secs(60))
            .await
            .expect("cae")
    );
    assert!(store.ttl(&key).await.expect("ttl").is_some());

    // Compare-and-delete cannot remove somebody else's value.
    assert!(!store.compare_and_delete(&key, b"1").await.expect("cad"));
    assert!(store.compare_and_delete(&key, b"2").await.expect("cad"));
    assert!(!store.exists(&key).await.expect("exists"));
}

/// Bulk reads and writes keep their order.
async fn bulk(store: &dyn KvStore, app: &str) {
    let keys: Vec<Key> = (0..5)
        .map(|index| key(app, &format!("bulk{index}")))
        .collect();
    for key in &keys {
        store.delete(key).await.expect("delete");
    }

    let items: Vec<(Key, Bytes)> = keys
        .iter()
        .enumerate()
        .map(|(index, key)| (key.clone(), bytes(&index.to_string())))
        .collect();
    store
        .set_many(&items, SetOpts::new())
        .await
        .expect("set_many");

    let read = store.get_many(&keys).await.expect("get_many");
    assert_eq!(
        read,
        (0..5)
            .map(|index| Some(bytes(&index.to_string())))
            .collect::<Vec<_>>(),
        "{}: get_many did not keep its order",
        store.name()
    );

    store.delete(&keys[2]).await.expect("delete");
    let read = store.get_many(&keys).await.expect("get_many");
    assert_eq!(read[2], None, "{}: a hole stays a hole", store.name());

    for key in &keys {
        store.delete(key).await.expect("delete");
    }
}

/// `scan` and `delete_prefix`, when the backend has them.
async fn scanning(store: &dyn KvStore, app: &str) {
    if !store.capabilities().scan {
        println!(
            "{}: skipping scan — capabilities().scan is false",
            store.name()
        );
        return;
    }

    for index in 0..12_u32 {
        store
            .set(
                &key(app, &format!("scan{index:02}")),
                Bytes::new(),
                SetOpts::new(),
            )
            .await
            .expect("set");
    }
    // A key in another namespace of the same application, to prove the prefix
    // is a boundary.
    let other = Key::from_raw(format!("moso:v1:{app}:other:1:x")).expect("valid");
    store
        .set(&other, Bytes::new(), SetOpts::new())
        .await
        .expect("set");

    let prefix = prefix(app);
    let mut seen = Vec::new();
    let mut cursor = ScanCursor::start();
    let mut rounds = 0;
    loop {
        let (page, next) = store.scan(&prefix, cursor, 5).await.expect("scan");
        for key in page {
            assert!(
                key.as_str().starts_with(prefix.as_str()),
                "{}: scan returned `{key}`, which is outside the prefix",
                store.name()
            );
            seen.push(key);
        }
        rounds += 1;
        assert!(rounds < 50, "{}: the scan did not terminate", store.name());
        if next.is_end() {
            break;
        }
        cursor = next;
    }

    assert_eq!(seen.len(), 12, "{}: saw {seen:?}", store.name());
    assert!(
        !seen.contains(&other),
        "{}: the scan crossed a namespace",
        store.name()
    );

    let removed = store.delete_prefix(&prefix).await.expect("delete_prefix");
    assert_eq!(removed, 12, "{}", store.name());
    assert!(
        store.exists(&other).await.expect("exists"),
        "{}: delete_prefix crossed a namespace",
        store.name()
    );
    store.delete(&other).await.expect("delete");
}

/// Lists, sets and sorted sets, when the backend has them.
async fn structures(store: &dyn KvStore, app: &str) {
    if !store.capabilities().structures {
        println!(
            "{}: skipping structures — capabilities().structures is false",
            store.name()
        );
        return;
    }

    // ── lists ────────────────────────────────────────────────────────────
    let list = key(app, "list");
    store.delete(&list).await.expect("delete");
    assert_eq!(store.list_len(&list).await.expect("len"), 0);
    assert_eq!(
        store.list_pop(&list, Side::Left, None).await.expect("pop"),
        None
    );

    assert_eq!(
        store
            .list_push(&list, &[bytes("b"), bytes("c")], Side::Right)
            .await
            .expect("push"),
        2
    );
    assert_eq!(
        store
            .list_push(&list, &[bytes("a")], Side::Left)
            .await
            .expect("push"),
        3
    );
    assert_eq!(store.list_len(&list).await.expect("len"), 3);
    assert_eq!(
        store.list_pop(&list, Side::Left, None).await.expect("pop"),
        Some(bytes("a")),
        "{}: a queue pops from the left",
        store.name()
    );
    assert_eq!(
        store.list_pop(&list, Side::Right, None).await.expect("pop"),
        Some(bytes("c"))
    );
    assert_eq!(
        store.list_pop(&list, Side::Left, None).await.expect("pop"),
        Some(bytes("b"))
    );
    assert_eq!(store.list_len(&list).await.expect("len"), 0);

    // A blocking pop on an empty list waits and then gives up.
    let started = Instant::now();
    assert_eq!(
        store
            .list_pop(&list, Side::Left, Some(Duration::from_millis(150)))
            .await
            .expect("pop"),
        None
    );
    assert!(
        started.elapsed() >= Duration::from_millis(100),
        "{}: the blocking pop returned immediately",
        store.name()
    );
    store.delete(&list).await.expect("delete");

    // ── sets ─────────────────────────────────────────────────────────────
    let set = key(app, "set");
    store.delete(&set).await.expect("delete");
    assert!(store.set_members(&set).await.expect("members").is_empty());

    assert_eq!(
        store
            .set_add(&set, &[bytes("a"), bytes("b")])
            .await
            .expect("add"),
        2
    );
    assert_eq!(
        store
            .set_add(&set, &[bytes("a"), bytes("b")])
            .await
            .expect("add"),
        0,
        "{}: a set does not add what it already holds",
        store.name()
    );
    let mut members = store.set_members(&set).await.expect("members");
    members.sort();
    assert_eq!(members, vec![bytes("a"), bytes("b")]);
    assert_eq!(
        store.set_remove(&set, &[bytes("a")]).await.expect("remove"),
        1
    );
    assert_eq!(
        store.set_members(&set).await.expect("members"),
        vec![bytes("b")]
    );
    store.delete(&set).await.expect("delete");

    // ── sorted sets ──────────────────────────────────────────────────────
    let zset = key(app, "zset");
    store.delete(&zset).await.expect("delete");
    assert!(
        store
            .zrange_by_score(&zset, f64::MIN, f64::MAX, 10)
            .await
            .expect("zrange")
            .is_empty()
    );

    assert_eq!(
        store
            .zadd(
                &zset,
                &[(2.0, bytes("b")), (1.0, bytes("a")), (3.0, bytes("c")),]
            )
            .await
            .expect("zadd"),
        3
    );
    assert_eq!(
        store
            .zrange_by_score(&zset, 1.0, 2.5, 10)
            .await
            .expect("zrange"),
        vec![bytes("a"), bytes("b")],
        "{}: a range is score-ordered and inclusive",
        store.name()
    );
    assert_eq!(
        store
            .zrange_by_score(&zset, f64::MIN, f64::MAX, 2)
            .await
            .expect("zrange")
            .len(),
        2,
        "{}: the limit is honoured",
        store.name()
    );
    assert_eq!(
        store.zadd(&zset, &[(0.5, bytes("c"))]).await.expect("zadd"),
        0,
        "{}: re-adding a member moves its score and adds nothing",
        store.name()
    );
    assert_eq!(
        store
            .zrange_by_score(&zset, 0.0, 0.9, 10)
            .await
            .expect("zrange"),
        vec![bytes("c")]
    );
    assert_eq!(store.zrem(&zset, &[bytes("c")]).await.expect("zrem"), 1);
    store.delete(&zset).await.expect("delete");
}

/// Publish and subscribe, when the backend has them.
async fn pubsub(store: &dyn KvStore, app: &str) {
    use futures_util::StreamExt as _;

    if !store.capabilities().pubsub {
        println!(
            "{}: skipping pubsub — capabilities().pubsub is false",
            store.name()
        );
        return;
    }

    let channel = format!("moso_test_{app}");
    let mut stream = store.subscribe(&channel).await.expect("subscribe");

    // Redis and PostgreSQL both establish the subscription asynchronously, so
    // publish until one lands rather than assuming the first one does.
    let deadline = Instant::now() + Duration::from_secs(5);
    let received = loop {
        store
            .publish(&channel, Bytes::from_static(b"hello"))
            .await
            .expect("publish");

        match tokio::time::timeout(Duration::from_millis(100), stream.next()).await {
            Ok(Some(payload)) => break payload,
            Ok(None) => panic!("{}: the subscription ended", store.name()),
            Err(_) if Instant::now() < deadline => {}
            Err(_) => panic!("{}: no message within five seconds", store.name()),
        }
    };
    assert_eq!(received, Bytes::from_static(b"hello"), "{}", store.name());
}

/// Every optional method the backend does *not* have says so.
async fn unsupported_is_reported(store: &dyn KvStore, app: &str) {
    let capabilities = store.capabilities();
    let key = key(app, "unsupported");

    if !capabilities.scripting {
        let error = store
            .eval("return 1", &[], &[])
            .await
            .expect_err("scripting is optional");
        assert!(
            matches!(error, moso_kv::Error::Unsupported { .. }),
            "{}: {error}",
            store.name()
        );
    }
    if !capabilities.structures {
        assert!(store.list_len(&key).await.is_err());
    }
    if !capabilities.scan {
        assert!(store.scan(&key, ScanCursor::start(), 1).await.is_err());
        assert!(store.delete_prefix(&key).await.is_err());
    }
}

/// Run everything against one store.
async fn run_suite(store: Arc<dyn KvStore>) {
    let app = unique_app();

    core_operations(store.as_ref(), &app).await;
    binary_values(store.as_ref(), &app).await;
    ttl_is_honoured_promptly(store.as_ref(), &app).await;
    conditional_writes(store.as_ref(), &app).await;
    counters(store.as_ref(), &app).await;
    compare_and_swap(store.as_ref(), &app).await;
    bulk(store.as_ref(), &app).await;
    scanning(store.as_ref(), &app).await;
    structures(store.as_ref(), &app).await;
    pubsub(store.as_ref(), &app).await;
    unsupported_is_reported(store.as_ref(), &app).await;

    // Whatever the suite left behind.
    if store.capabilities().scan {
        store
            .delete_prefix(&prefix(&app))
            .await
            .expect("clean up after the suite");
    }
    assert_eq!(store.health().await, moso_core::HealthStatus::Up);
}

// ---------------------------------------------------------------------------
// The three legs
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn memory_backend() {
    run_suite(Arc::new(MemoryStore::new())).await;
}

#[cfg(feature = "redis")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redis_backend() {
    let Some(url) = std::env::var("REDIS_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        println!(
            "skipping the redis leg: REDIS_URL is not set. \
             Start one with `docker run -d -p 56379:6379 redis:7-alpine` and \
             `export REDIS_URL=redis://localhost:56379`."
        );
        return;
    };

    let store = moso_kv::backend::RedisStore::connect(moso_kv::backend::RedisConfig::new(url))
        .await
        .expect("connected to REDIS_URL");
    run_suite(Arc::new(store)).await;
}

#[cfg(feature = "pg-kv")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn postgres_backend() {
    let Some(url) = std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        println!(
            "skipping the postgres leg: DATABASE_URL is not set. \
             Start one with `./scripts/test-db.sh up` and \
             `export DATABASE_URL=\"$(./scripts/test-db.sh url)\"`."
        );
        return;
    };

    let store =
        moso_kv::backend::PostgresStore::connect(&url, "moso_kv_test", 4, Duration::from_secs(10))
            .await
            .expect("connected to DATABASE_URL");
    run_suite(Arc::new(store)).await;
}

// ---------------------------------------------------------------------------
// The parts that are only true of one backend
// ---------------------------------------------------------------------------

#[cfg(feature = "pg-kv")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_postgres_sweeper_reclaims_expired_rows() {
    let Some(url) = std::env::var("DATABASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        println!("skipping: DATABASE_URL is not set");
        return;
    };

    let store =
        moso_kv::backend::PostgresStore::connect(&url, "moso_kv_test", 2, Duration::from_secs(10))
            .await
            .expect("connected");

    let app = unique_app();
    for index in 0..5_u32 {
        store
            .set(
                &key(&app, &format!("sweep{index}")),
                Bytes::new(),
                SetOpts::new().ttl(Duration::from_millis(50)),
            )
            .await
            .expect("set");
    }

    // Invisible immediately ...
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(!store.exists(&key(&app, "sweep0")).await.expect("exists"));

    // ... and reclaimed by the sweeper.
    let removed = store.sweep().await.expect("sweep");
    assert!(removed >= 5, "swept {removed} rows");

    // A second sweep finds nothing of ours to do.
    let again = store.sweep().await.expect("sweep");
    assert!(again < removed || again == 0);
}

#[cfg(feature = "redis")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn redis_runs_the_rate_limit_script_server_side() {
    let Some(url) = std::env::var("REDIS_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
    else {
        println!("skipping: REDIS_URL is not set");
        return;
    };

    let store = moso_kv::backend::RedisStore::connect(moso_kv::backend::RedisConfig::new(url))
        .await
        .expect("connected");
    assert!(
        store.capabilities().scripting,
        "the redis backend has scripting"
    );

    // The GCRA script, for real, against a real Redis.
    let kv = moso_kv::Kv::builder(unique_app())
        .store(store)
        .build()
        .expect("built");
    let quota = moso_kv::RateQuota::new(10, Duration::from_secs(60));

    let mut admitted = 0;
    for _ in 0..25 {
        if kv
            .rate_limit("script", quota)
            .await
            .expect("decided")
            .allowed
        {
            admitted += 1;
        }
    }
    assert_eq!(admitted, 10, "the script admitted the wrong number");
}
