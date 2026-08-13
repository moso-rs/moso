# 25 - KV Storage & Caching

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## Position

Every non-trivial web app needs a key-value store for sessions, caches, rate limits, locks, and
ephemeral state. Today a Rust developer picks between `redis-rs` and `fred`, wires up a pool,
invents a key scheme, and hand-rolls serialisation. Moso ships the layer with typed namespaces and
a backend trait, so the *same code* runs against an in-process map in tests and Redis in production.

## The trait

```rust
// spec - moso-kv/src/store.rs
#[async_trait]
pub trait KvStore: Send + Sync + 'static {
    async fn get(&self, key: &Key) -> Result<Option<Bytes>>;
    async fn set(&self, key: &Key, val: Bytes, opts: SetOpts) -> Result<bool>;
    async fn delete(&self, key: &Key) -> Result<bool>;
    async fn exists(&self, key: &Key) -> Result<bool>;
    async fn expire(&self, key: &Key, ttl: Duration) -> Result<bool>;
    async fn ttl(&self, key: &Key) -> Result<Option<Duration>>;

    // atomics
    async fn incr(&self, key: &Key, by: i64, ttl: Option<Duration>) -> Result<i64>;
    async fn compare_and_swap(&self, key: &Key, old: Option<&[u8]>, new: Bytes) -> Result<bool>;

    // bulk
    async fn get_many(&self, keys: &[Key]) -> Result<Vec<Option<Bytes>>>;
    async fn set_many(&self, items: &[(Key, Bytes)], opts: SetOpts) -> Result<()>;
    async fn delete_prefix(&self, prefix: &Key) -> Result<u64>;
    async fn scan(&self, prefix: &Key, cursor: Cursor, limit: u32) -> Result<(Vec<Key>, Cursor)>;

    // structures (optional - check `capabilities()`)
    async fn list_push(&self, key: &Key, vals: &[Bytes], side: Side) -> Result<u64>;
    async fn list_pop(&self, key: &Key, side: Side, timeout: Option<Duration>) -> Result<Option<Bytes>>;
    async fn set_add(&self, key: &Key, members: &[Bytes]) -> Result<u64>;
    async fn set_members(&self, key: &Key) -> Result<Vec<Bytes>>;
    async fn zadd(&self, key: &Key, scored: &[(f64, Bytes)]) -> Result<u64>;
    async fn zrange_by_score(&self, key: &Key, lo: f64, hi: f64, limit: u32) -> Result<Vec<Bytes>>;

    // pubsub (optional)
    async fn publish(&self, channel: &str, payload: Bytes) -> Result<u64>;
    async fn subscribe(&self, channel: &str) -> Result<BoxStream<'static, Bytes>>;

    fn capabilities(&self) -> Capabilities;
    async fn health(&self) -> HealthStatus;
}

pub struct SetOpts { pub ttl: Option<Duration>, pub if_absent: bool, pub if_present: bool,
                     pub keep_ttl: bool }

pub struct Capabilities { pub pubsub: bool, pub structures: bool, pub scan: bool,
                          pub atomic_cas: bool, pub scripting: bool, pub persistence: bool }
```

`capabilities()` is how a battery degrades gracefully: `moso-jobs` uses list operations if present,
otherwise the Postgres backend; the rate limiter uses a script if present, otherwise CAS.

## Typed namespaces (the ergonomic layer)

Raw byte APIs are not what handler code should touch.

```rust
// example
moso::kv::namespace! {
    /// Cached user profile, refreshed on write.
    pub Profile: Id<User> => UserProfile, ttl = 15.min(), codec = Json;

    /// One-time login codes.
    pub LoginCode: Email => Code, ttl = 10.min(), codec = Json;

    /// Per-IP request counter for the rate limiter.
    pub IpRate: IpAddr => u64, ttl = 1.min(), codec = Raw;
}
```

Generates, for each entry, a zero-sized type implementing `Namespace` with:

```rust
// spec
pub trait Namespace: 'static {
    type Key: KeyPart;
    type Value: Serialize + DeserializeOwned;
    const PREFIX: &'static str;          // "profile" - derived from the name, overridable
    const TTL: Option<Duration>;
    type Codec: Codec;                   // Json | MsgPack | Bincode | Raw
}

// usage, via the Kv handle
impl Kv {
    pub async fn get<N: Namespace>(&self, k: &N::Key) -> Result<Option<N::Value>>;
    pub async fn set<N: Namespace>(&self, k: &N::Key, v: &N::Value) -> Result<()>;
    pub async fn set_ttl<N: Namespace>(&self, k: &N::Key, v: &N::Value, ttl: Duration) -> Result<()>;
    pub async fn delete<N: Namespace>(&self, k: &N::Key) -> Result<bool>;
    pub async fn get_or_insert_with<N, F, Fut>(&self, k: &N::Key, f: F) -> Result<N::Value>;
    pub async fn incr<N: Namespace<Value = u64>>(&self, k: &N::Key, by: i64) -> Result<i64>;
}
```

```rust
// example
let profile = kv.get_or_insert_with::<Profile, _, _>(&user.id, || async {
    UserProfile::build(&db, user.id).await
}).await?;
```

Properties:
- Keys are namespaced and versioned: `moso:v1:shop:profile:0192f…`. The app name and a schema
  version are included so two apps can share a Redis and a deploy can invalidate a namespace by
  bumping its version (`#[namespace(version = 2)]`).
- Values are typed. A namespace's `Value` cannot be confused with another's.
- Key parts are escaped so a key containing `:` cannot forge another namespace.

## Cache patterns

### `#[cached]`
```rust
// example
#[moso::cached(ttl = "5m", key = "user:{id}", namespace = Profile)]
pub async fn load_profile(db: &Db, id: Id<User>) -> Result<UserProfile> { … }
```

Expands to a `get_or_insert_with` with **single-flight** de-duplication (concurrent callers with the
same key wait for one computation, not N) and negative caching for `Ok(None)` results, which is the
usual stampede source.

Invalidation is explicit - `#[cached]` generates `load_profile::invalidate(&kv, id)`. There is no
automatic entity-change invalidation, because inferring it is unreliable and silently wrong; the
docs recommend invalidating in the service function that writes.

### Stale-while-revalidate
```rust
kv.get_swr::<Profile, _, _>(&id, Duration::from_secs(60), || async { … }).await?
```
Serves a stale value immediately and refreshes in the background - the right default for expensive
reads on hot paths.

### Distributed locks
```rust
// example
let _guard = kv.lock("import:acme", Duration::from_secs(30)).await?;
```
Redlock-lite: single-instance lock with a fencing token, auto-renewal while the guard is held, and
release on drop. The docs are explicit that this is **not** safe as a correctness mechanism across
a Redis failover, and point at Postgres advisory locks (`db.advisory_lock`) when correctness
matters. Being honest here matters more than the feature.

### Rate limiting
```rust
// example
Router::new()
    .post("/auth/login", login)
    .guard(RateLimit::new()
        .key(RateKey::Ip)               // Ip | User | Header("x-api-key") | Custom
        .quota(10, Duration::from_secs(60))
        .burst(3)
        .on_exceed(Response::problem(429)))
```
GCRA (leaky bucket) implemented with one atomic op per request. Emits `X-RateLimit-Limit`,
`-Remaining`, `-Reset` and `Retry-After`. Contributes a documented 429 to the OpenAPI, because it
is a `Guard`.

## Backends

| Backend | Feature | Use | Notes |
| --- | --- | --- | --- |
| `memory` | default | dev, tests, single-instance | `moka`-backed, TTL, capacity bound, no pubsub across processes |
| `redis` | `redis` | production standard | via `fred`: pooling, pipelining, cluster, sentinel, TLS, auto-reconnect |
| `postgres` | `pg-kv` | teams refusing a second datastore | table-backed with a TTL sweeper; `LISTEN/NOTIFY` provides pubsub |
| custom | - | anything else | implement `KvStore`; DynamoDB/Cloudflare KV are natural fits |

Backend choice is config, not code:
```toml
[kv]
backend = "redis"
url = "redis://localhost:6379"
pool_size = 8
```

`memory` in tests means the test suite has no external dependency, which is a Loop-1 requirement.
The `memory` backend deliberately implements the *same semantics* including TTL granularity, so a
test that passes there passes on Redis; where semantics genuinely differ (cross-process pubsub) the
capability flag is false and the test harness says so.

## Failure policy

A cache is not a database. Moso's default: **KV failures degrade, they do not fail the request.**

```rust
// spec
pub enum KvFailureMode { Degrade, Fail }     // per-namespace, default Degrade
```

With `Degrade`, a Redis outage turns `get` into `Ok(None)` and `set` into a no-op, logs at `warn`
with a rate limit, increments `moso_kv_errors_total`, and lets the request proceed to the source of
truth. Sessions and locks default to `Fail`, because silently losing them is worse. Each namespace
declares its mode, and the default in the macro is chosen by the namespace's role.

A circuit breaker opens after `N` consecutive failures to avoid piling connection attempts onto a
struggling Redis, and probes for recovery with jitter.

## Acceptance criteria (WP-15a)

1. The same test suite passes against `memory`, `redis`, and `postgres` backends, except for tests
   gated on `capabilities()`.
2. `namespace!` produces distinct, collision-free prefixes; a key containing `:` cannot forge
   another namespace (fuzz test).
3. `#[cached]` de-duplicates concurrent identical calls to one computation (counter assertion with
   100 concurrent callers).
4. Rate limiter: 10/min quota admits exactly 10 in a minute across 4 concurrent workers, and the
   headers are correct.
5. Redis outage with `Degrade` namespaces keeps the app serving; with `Fail` namespaces produces
   503; both are covered by a test that kills the backend mid-run.
6. TTLs are honoured within 50 ms on all backends.
7. `kv.lock` releases on drop, on panic, and on process exit within the lease.
