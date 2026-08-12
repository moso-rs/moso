---
title: Cache and key value store
description: Declare typed cache namespaces, read through them with single flight de-duplication, and pick between the memory, Redis and PostgreSQL backends without changing a line of handler code.
order: 20
status: shipped
---

`moso-kv` is the key value layer: one `Kv` handle, typed namespaces declared with a macro, and three
interchangeable backends behind a `KvStore` trait. The same code runs against an in-process map in
tests and Redis in production, because the backend is chosen by configuration and never appears in a
handler signature.

What you get for that: a key layout two applications can share on one Redis without colliding, a
cache that turns a store outage into a miss instead of a 500 unless you say otherwise, read-through
caching that collapses a hundred concurrent misses into one computation, and a circuit breaker so a
struggling Redis is not hammered by every request that was going to fail anyway. This page covers the
cache. Rate limits, distributed locks and the pub/sub bus live on the same handle and are covered in
[rate limiting and locks](./rate-limiting.md).

> [!IMPORTANT]
> Three shapes of the KV surface to know up front. The `moso` facade re-exports it as `moso::kv`
> behind the `kv` (and `full`) feature; this guide uses direct `moso_kv::` paths so the examples name
> the crate plainly. `KvConfig` is built in code rather than deserialised from a `[kv]` TOML block, so
> map your own config struct into a `KvConfig` field by field. And observability is `Kv::stats()` plus
> `tracing`, which you export through your own recorder, since the crate leaves the metric sink to
> that seam. Each is expanded where you meet it.

## Adding the crate

```toml title="Cargo.toml"
[dependencies]
moso = { path = "/absolute/path/to/moso/crates/moso" }
moso-kv = { path = "/absolute/path/to/moso/crates/moso-kv" }
```

Three Cargo features, one per backend. Each is the only thing that pulls its driver in.

| Feature | Default | Adds | Pulls in |
| --- | --- | --- | --- |
| `memory` | yes | `backend::MemoryStore`, `Kv::in_memory` | `moka` |
| `redis` | no | `backend::RedisStore`, `backend::RedisConfig` | `fred` |
| `pg-kv` | no | `backend::PostgresStore`, `backend::Sweeper` | `sqlx` |

The memory backend is on by default because a test suite that needs a running Redis is a test suite
people stop running. Configuring a backend whose feature is off is a boot error naming the feature,
not a runtime surprise.

## The smallest thing that works

```rust title="src/cache.rs"
use moso_kv::prelude::*;

moso_kv::namespace! {
    /// A cached greeting.
    pub Greeting: u64 => String, ttl = minutes(1);
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let kv = Kv::in_memory("shop")?;
    kv.set::<Greeting>(&1, &"hello".to_owned()).await?;
    assert_eq!(kv.get::<Greeting>(&1).await?.as_deref(), Some("hello"));
    Ok(())
}
```

`namespace!` declares a zero-sized type. The type is the registration: nothing is added to a
registry, and `kv.get::<Greeting>(&1)` cannot be handed a key of the wrong type, a value of the wrong
type or a TTL that belongs to some other cache.

`Kv::in_memory` also sets `BreakerConfig::never()`, because there is nothing to break.

## Getting a handle

### From configuration

`KvConfig` picks the backend at run time and opens it eagerly, so an unreachable Redis is a boot
failure with a message rather than a 503 on the first cache read.

```rust title="src/main.rs"
use moso_kv::{KvBackend, KvConfig, Result};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let kv = KvConfig::new("shop", KvBackend::Memory).build().await?;
    assert_eq!(kv.store().name(), "memory");
    Ok(())
}
```

The production shape is the same call with more settings:

```rust
let config = KvConfig::new("shop", KvBackend::Redis)
    .url("redis://localhost:6379")
    .pool_size(8)
    .connect_timeout(Duration::from_secs(3));

assert!(config.validate().is_ok());
```

| Setting | Default | Applies to |
| --- | --- | --- |
| `app` | required, `[a-z0-9_-]{1,48}` | every key this handle writes |
| `backend` | `Memory` | which store to open |
| `url` | none | Redis and PostgreSQL, required for both |
| `pool_size` | `8` | Redis and PostgreSQL, clamped to at least 1 |
| `connect_timeout` | 5 s | Redis and PostgreSQL |
| `capacity` | `10_000` | memory only, the entry bound |
| `table` | `moso_kv` | PostgreSQL only, validated because it reaches SQL |
| `breaker` | `BreakerConfig::default()` | every backend |
| `sweep_interval` | 30 s | PostgreSQL only, `Duration::ZERO` turns the sweeper off |

`KvBackend` parses case-insensitively from `memory` / `in-memory` / `inmemory`, `redis` / `valkey`,
and `postgres` / `postgresql` / `pg`. Anything else is an `Error::Config` naming the three that
exist.

`validate()` runs the application-name check, the "this backend needs a URL" check and the table-name
check without opening a connection, so you can assert on a configuration in a unit test.

### From the environment

`KvConfig::from_env("shop")` reads exactly five variables:

| Variable | Type | Default |
| --- | --- | --- |
| `KV_BACKEND` | one of the spellings above | `memory` |
| `KV_URL` | string | none |
| `KV_POOL_SIZE` | `u32` | `8` |
| `KV_CAPACITY` | `u64` | `10000` |
| `KV_TABLE` | string | `moso_kv` |

There is no `KV_CONNECT_TIMEOUT`, no `KV_SWEEP_INTERVAL` and no way to configure the circuit breaker
from the environment. Those three keep their defaults unless you set them in code. `KvConfig` derives
only `Debug` and `Clone`, so it also cannot be deserialised out of your `moso.toml`: declare the
fields you want on your own `#[derive(Config)]` struct and convert. See
[configuration](./configuration.md).

### From a store you built yourself

```rust
use moso_kv::backend::MemoryStore;
use moso_kv::{Kv, KvStore};
use std::sync::Arc;

let store: Arc<dyn KvStore> = Arc::new(MemoryStore::new());
let shop = Kv::builder("shop").shared_store(Arc::clone(&store)).build().expect("built");
let blog = Kv::builder("blog").shared_store(store).build().expect("built");

assert_eq!(shop.app(), "shop");
assert_eq!(blog.app(), "blog");
```

`KvBuilder::store` takes ownership of a `KvStore`; `shared_store` takes an `Arc` so two applications
can share one physical connection pool with separate keyspaces. `breaker(BreakerConfig)` sets the
policy. `build()` fails with `Error::Key` for a bad application name and `Error::Config` when no
store was given.

### Wiring it into the application

```rust title="src/lib.rs"
let kv = KvConfig::from_env("shop")?.build().await?;

let app = App::new(config)
    .provide(kv.clone())
    .health_check("cache", kv.health_check())
    .mount(routes());
```

`Kv` is one `Arc`, so cloning it is free. It implements `moso_core::Dependency`, which means a
handler reads it with `Depends<Kv>` and a missing `.provide(kv)` is a boot error rather than a 500 on
the first request:

```rust title="src/routes/users.rs"
moso_kv::namespace! {
    /// One user, as the API returns them. `UserOut` derives `Clone`, which
    /// `get_or_insert_with` requires.
    pub CachedUser: u64 => UserOut, ttl = moso_kv::minutes(5);
}

/// Fetch a user.
#[endpoint]
async fn show(Path(id): Path<u64>, Depends(kv): Depends<Kv>) -> Result<Json<UserOut>> {
    let user = kv
        .get_or_insert_with::<CachedUser, _, _>(&id, || async { Ok(load_user(id).await?) })
        .await?;
    Ok(Json(user))
}
```

The two `?` in that body cross an error boundary in each direction. The inner one turns a
`moso_core::Error` from your loader into `moso_kv::Error::Http`, and the outer one turns it back, so
a 404 raised inside the loader is still a 404 on the wire.

`kv.health_check()` is non-critical by default: a cache whose namespaces all degrade is a cache whose
absence the service survives, and taking every instance out of rotation over it turns a degraded
service into an outage. The `/readyz` report still says the store is down, which is what an operator
needs. Call `.critical_check()` when sessions live in the same store. See
[health and shutdown](./health-and-shutdown.md).

## Typed namespaces

One `namespace!` invocation declares any number of namespaces, separated by `;`:

```rust title="src/cache.rs"
use moso_kv::{days, minutes, seconds};
use moso_schema::types::{Email, Id};

moso_kv::namespace! {
    /// A user's public profile, absent users cached briefly.
    pub Profile: Id<User> => Option<UserProfile>, ttl = minutes(15), negative_ttl = seconds(30);

    /// One-time login codes. Losing one locks somebody out, so it fails loudly.
    pub LoginCode: Email => String, ttl = minutes(10), on_failure = fail;

    /// Requests seen per IP in the last minute.
    pub IpRate: std::net::IpAddr => u64, ttl = minutes(1), codec = Raw;

    /// The homepage feed, invalidated by bumping the version.
    pub Feed: () => Vec<FeedItem>, ttl = days(1), version = 3, prefix = "home_feed";
}
```

The grammar is `[attributes] [vis] Name : KeyType => ValueType [, option]* ;`. There are exactly six
options and an unknown one is a `compile_error!` that names all six.

| Option | Values | Default | Meaning |
| --- | --- | --- | --- |
| `ttl = <expr>` | any `Duration` expression | none, meaning no expiry | how long a value lives |
| `negative_ttl = <expr>` | any `Duration` expression | falls back to `ttl` | how long a cached `None` lives |
| `codec = ...` | `Json`, `Raw`, or any type path | `Json` | how a value becomes bytes |
| `version = <expr>` | a `u16` expression | `1` | bump to invalidate the namespace |
| `prefix = <literal>` | a string literal | `snake_case` of the type name | the key segment |
| `on_failure = ...` | `degrade` or `fail` | `degrade` | what a store outage does |

Order does not matter. `seconds`, `minutes`, `hours` and `days` are `const fn` helpers in the
prelude, so a TTL is a compile-time constant.

### The key layout

Every key is the same shape. A namespace prefixed `profile` at version 1, in an application called
`shop`, keyed by `7`, lands here:

```text
moso:v1:shop:profile:1:7
 |    |   |     |    |  `- the key parts, one segment each, escaped
 |    |   |     |    `---- Namespace::VERSION
 |    |   |     `--------- Namespace::PREFIX
 |    |   `--------------- the application name
 |    `------------------- the key layout version (KEY_FORMAT)
 `------------------------ the sentinel (KEY_SENTINEL)
```

Six bugs do not happen because of it. Two applications share one Redis safely. A deploy invalidates
one namespace by bumping `version` with no `FLUSHDB` and no effect on anything else. A key cannot
forge a namespace, because every segment before the key parts is at a fixed index and every key part
has its separators escaped. The layout itself is versioned, so changing it later is a migration
rather than a silent mass miss. A namespace prefix ends in `:`, so version 1 never matches version
11. And nothing in a key is a control byte, which matters because PostgreSQL `text` cannot hold a
`NUL`.

The escaping table, which is what makes the forging property true:

| Input | Written as |
| --- | --- |
| `\` | `\\` |
| `:` | `\c` |
| `#` | `\h` |
| a byte below `0x20`, or `0x7F` | `\xHH` |
| anything else, including all non-ASCII UTF-8 | unchanged |

A byte-slice key part is written as a leading `#` followed by lowercase hex, which escaped text can
never start with, so a byte part and a text part live in disjoint alphabets and cannot collide.

`Kv::key::<N>(&k)` gives you the key a namespace would use, which is worth asserting in your own
test, and `Kv::namespace_prefix::<N>()` gives you the prefix.

### What can be a key

`u8` through `u128`, `usize`, the signed equivalents, `bool`, `char`, `str`, `String`, `Cow<str>`,
`[u8]`, `Vec<u8>`, `bytes::Bytes`, `IpAddr`, `Ipv4Addr`, `Ipv6Addr`, `uuid::Uuid`,
`moso_schema::types::Id<E>`, `Email`, `Slug`, `()`, and tuples of arity 2 through 6 of any of those.
Anything else, implement `KeyPart`:

```rust
use moso_kv::{KeyBuf, KeyPart};

/// A tenant, as it appears in a cache key.
pub struct TenantId(pub u64);

impl KeyPart for TenantId {
    fn write_key_part(&self, out: &mut KeyBuf) {
        out.segment_display(self.0);
    }
}
```

Names are validated at compile time. `const _: () = assert_name(PREFIX);` is part of what the macro
emits, so a prefix containing a colon, an upper-case letter or nothing at all is a build failure and
not a 500 on the first cache read. The alphabet is `[a-z0-9_-]{1,48}`.

## Codecs

A codec turns a value into bytes. Two ship.

| Codec | Framed | `Encodable` for | Use it for |
| --- | --- | --- | --- |
| `Json` | yes | anything `Serialize + DeserializeOwned` | almost everything |
| `Raw` | no | `String`, `Vec<u8>`, `Bytes`, `u8`..`u64`, `usize`, `i8`..`i64`, `isize` | counters, and bytes another service reads |

`Raw` means the value's own byte representation. An integer is decimal ASCII, which is exactly what
Redis `INCR` produces and consumes, which is why `Kv::incr` is bounded on `Codec = Raw` at the type
level. There is deliberately no `Raw` impl for `u128` or `i128`.

A framed codec prepends a twelve-byte envelope: a magic byte, a framing version, a flag byte whose
bit 0 means "this is a cached absence", a reserved byte, and the write time in milliseconds since the
Unix epoch, little-endian. That header is what makes three things possible: stale-while-revalidate,
because the age of a value is a property of the value and not of the store's TTL, which is already
spent on eviction; negative caching, because "absent, and we know it" has to be distinguishable from
"absent"; and a version byte, so changing the framing later is a decode error naming the namespace
rather than a wrong value.

A third codec is your own type plus its `Encodable` impls. There is no MsgPack and no bincode in the
crate:

```rust
use bytes::Bytes;
use moso_kv::codec::{Codec, Encodable};
use moso_kv::BoxError;

/// Big-endian `u32`, for a namespace shared with a C service.
#[derive(Debug, Clone, Copy)]
pub struct BigEndian;

impl Codec for BigEndian {
    const NAME: &'static str = "be32";
    const FRAMED: bool = false;
}

impl Encodable<BigEndian> for u32 {
    fn encode_value(&self) -> Result<Bytes, BoxError> {
        Ok(Bytes::copy_from_slice(&self.to_be_bytes()))
    }
    fn decode_value(bytes: &[u8]) -> Result<Self, BoxError> {
        let array: [u8; 4] = bytes.try_into().map_err(|_| "expected 4 bytes")?;
        Ok(u32::from_be_bytes(array))
    }
}
```

Then `codec = BigEndian` in a `namespace!` entry, because the `codec` option accepts any type path.

## Reading and writing

Every method below is generic over the namespace and takes `&N::Key`.

| Call | Returns | Notes |
| --- | --- | --- |
| `get::<N>(&k)` | `Option<N::Value>` | a decode failure is a miss, not an error |
| `entry::<N>(&k)` | `Option<CachedValue<N::Value>>` | the value plus `age` and `negative` |
| `set::<N>(&k, &v)` | `()` | TTL from `N::ttl_for(&v)` |
| `set_ttl::<N>(&k, &v, ttl)` | `()` | overrides the namespace TTL |
| `set_if_absent::<N>(&k, &v)` | `bool` | one-time codes, idempotency keys |
| `delete::<N>(&k)` | `bool` | whether it was there |
| `exists::<N>(&k)` | `bool` | |
| `ttl::<N>(&k)` | `Option<Duration>` | |
| `incr::<N>(&k, by)` | `i64` | only where `N::Codec = Raw` |
| `keys::<N>()` | `Vec<Key>` | needs `Capabilities::scan` |
| `clear_namespace::<N>()` | `u64` | needs `Capabilities::scan` |
| `key::<N>(&k)` | `Key` | the key that would be used |
| `namespace_prefix::<N>()` | `Key` | the prefix all its keys share |
| `store()` | `&Arc<dyn KvStore>` | the raw byte API, when the typed layer is not enough |

`entry` is the one that carries metadata:

```rust
let entry = kv.entry::<Profile>(&7).await?.expect("just written");
assert_eq!(entry.value, "alice");
assert!(entry.age < Duration::from_secs(1));
assert!(!entry.negative);
```

`age` is always `Duration::ZERO` and `negative` is always `false` for an unframed codec, because
there is no envelope to read them from.

Counters go through `incr`, which is atomic in the backend rather than a read-modify-write in your
process:

```rust
moso_kv::namespace! {
    /// Per-IP request counter.
    pub IpRate: std::net::IpAddr => u64, ttl = minutes(1), codec = Raw;
}

let ip = std::net::IpAddr::from([127, 0, 0, 1]);
assert_eq!(kv.incr::<IpRate>(&ip, 1).await?, 1);
assert_eq!(kv.incr::<IpRate>(&ip, 2).await?, 3);
```

## Caching patterns

### Read through, with single flight

`get_or_insert_with` reads, and on a miss runs your closure exactly once no matter how many callers
missed at the same moment. The losers await the winner's result rather than running their own
closure, which is why the value type must be `Clone`.

```rust
use moso_kv::{minutes, seconds, Kv, Result};

moso_kv::namespace! {
    /// Cached user profile, refreshed on write.
    pub Profile: u64 => Option<String>, ttl = minutes(15), negative_ttl = seconds(30);

    /// The session record. Losing one logs somebody out, so it fails loudly.
    pub Session: String => String, ttl = minutes(480), on_failure = fail;
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let kv = Kv::in_memory("shop")?;

    let profile = kv
        .get_or_insert_with::<Profile, _, _>(&7, || async { Ok(Some("alice".to_owned())) })
        .await?;

    assert_eq!(profile.as_deref(), Some("alice"));
    assert_eq!(kv.key::<Profile>(&7)?.as_str(), "moso:v1:shop:profile:1:7");
    Ok(())
}
```

Negative caching is free and you opt into it by writing the return type you were going to write
anyway. Any namespace whose `Value` is an `Option<T>` stores its `None` under `negative_ttl` instead
of `ttl`, so a lookup for a user that does not exist costs one short-lived key rather than a database
round trip per request.

An application error raised inside the closure travels as `Error::Http` and comes back out with its
original status, so a 404 raised inside a cached loader is a 404 to the client and not a 500.

> [!NOTE]
> The single-flight map lives on the `Kv` handle, so it de-duplicates concurrent callers inside one
> process, not across a fleet. Across a fleet, the cache write is what stops the stampede.

### Stale while revalidate

`get_swr` serves a stale value immediately and refreshes behind the request. It requires a framed
codec, enforced by the compiler through `N::Codec: Framed`, because it needs the age the envelope
carries.

```rust
use moso_kv::{minutes, Kv, Result};
use std::time::Duration;

moso_kv::namespace! {
    /// Expensive dashboard numbers.
    pub Dashboard: u64 => Option<u64>, ttl = minutes(10);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let kv = Kv::in_memory("shop")?;

    // Nothing cached: this one computes and waits.
    let first = kv
        .get_swr::<Dashboard, _, _>(&1, Duration::from_millis(20), || async { Ok(Some(1)) })
        .await?;
    assert_eq!(first, Some(1));

    // Fresh: served from the cache.
    let second = kv
        .get_swr::<Dashboard, _, _>(&1, Duration::from_secs(60), || async { Ok(Some(2)) })
        .await?;
    assert_eq!(second, Some(1));
    Ok(())
}
```

The refresh runs in a spawned task under a separate flight key, so a revalidation never joins the
flight of a reader waiting for a key's first value. A failed background refresh logs and leaves the
stale value in place. With nothing cached at all, `get_swr` falls through to `get_or_insert_with` and
the caller waits.

### `cached!` over an existing function

`cached!` wraps a function you already have. It is a `macro_rules!` and not an attribute macro,
because an attribute needs a proc-macro crate and `moso-kv` is a runtime crate. The cost is one line
and one level of indentation; the benefit is no second crate in your dependency graph and no compile
time for anybody who does not use it.

```rust title="src/services/orders.rs"
use moso_kv::{minutes, Kv, Result};

moso_kv::namespace! {
    /// Cached order totals, by customer and currency.
    pub Totals: (u64, String) => Option<u64>, ttl = minutes(2);
}

moso_kv::cached! {
    #[cached(namespace = Totals, key = (customer, currency.clone()), kv = cache)]
    /// Total spend, in minor units.
    pub async fn total(cache: &Kv, db: &Db, customer: u64, currency: String) -> Result<Option<u64>> {
        let _ = (db, &currency);
        Ok(Some(customer * 100))
    }
}
```

Three options and no more. An unknown one is a `compile_error!` naming all three.

| Option | Value | Default |
| --- | --- | --- |
| `namespace = <ty>` | a `Namespace` type | required |
| `key = <expr>` | an expression over the function's parameters | required |
| `kv = <ident>` | which parameter holds the `Kv` | the first parameter |

There is no `ttl` option, and the error message says so: a TTL belongs on the namespace. The
`#[cached(..)]` attribute must come **first**, before any doc comments, because a declarative macro
cannot tell a `#[cached]` that follows a `#[doc]` from any other attribute. The `key` expression is
evaluated before the arguments move into the closure, so it may borrow them.

The macro also generates a module of the same name, and that module is the whole invalidation story:

```rust
// The uncached function, for a caller that must not read a stale value.
total::uncached(&kv, &Db, 3, "eur".to_owned()).await?;

// Invalidate one entry. Call this from the service function that wrote.
total::invalidate(&kv, &(3, "eur".to_owned())).await?;

// The key, for debugging.
assert_eq!(
    total::cache_key(&kv, &(3, "eur".to_owned()))?.as_str(),
    "moso:v1:shop:totals:1:3:eur",
);
```

There is no automatic invalidation and there will not be one. Inferring which cache entries a write
affects is unreliable and silently wrong, and the right place to invalidate is the function that did
the writing. Your three tools, in order of preference: call `invalidate` next to the write, set a TTL
short enough that staleness does not matter, or bump `version` in the namespace when a deploy changes
the shape or the meaning of a whole namespace. That last one is instant, costs one recompile, and
never walks a production keyspace.

### Single flight on its own

The primitive is public, so you can de-duplicate work that is not a cache read at all.

```rust
use moso_kv::flight::SingleFlight;

let flight = SingleFlight::new();
let value = flight.run("rebuild-index", || async { Ok(expensive().await) }).await?;
assert_eq!(flight.in_flight(), 0);
```

Failures are deliberately not cached: the error goes to every waiter and the next caller starts a
fresh flight. An error is a fact about the moment, not about the key, and caching one would turn a
blip into an outage, which is the mistake single flight exists to avoid, made in the other direction.

## When the store is down

Every namespace declares what an outage means for it. `on_failure = degrade` is the default: a cache
is not a database, so a store outage turns a read into a miss, logs at `warn`, ticks a counter and
lets the request reach the source of truth. `on_failure = fail` propagates the error, and the client
gets a 503 with a `Retry-After`.

```rust
// Same store, same outage, two answers.
assert_eq!(kv.get::<Cached>(&1).await.expect("degraded"), None);
assert!(kv.get::<Session>(&1).await.is_err());
```

The fallback under `degrade` is per operation, and reading them literally matters:

| Operation | Degraded result |
| --- | --- |
| `get`, `entry`, `ttl` | `None` |
| `exists`, `delete`, `set_if_absent` | `false` |
| `set`, `set_ttl` | `Ok(())`, nothing written |
| `incr` | `0` |
| `clear_namespace` | `0` |

`incr` returning `0` is the honest answer, and it means "nothing was counted", not "the counter is at
zero". `set_if_absent` returning `false` means "this write did not apply", so a caller that reads it
as "somebody else has this code" fails closed, which is the safe direction.

Three things are never degraded, in any mode. A decode failure on **read** is a miss with a `warn`,
always, even under `fail`, because a rolling deploy that changes a cached type's shape will read
bytes written by the other version and 500-ing every request until the old pods drain is strictly
worse than recomputing. A decode failure on **write** propagates, because serialising a value you are
holding cannot fail for any reason outside your own code. And `Error::Unsupported` propagates,
because the capability was knowable before the call and a wrong answer would be worse than a loud
one.

Errors map onto HTTP like this, and the mapping is what a handler that returns `?` produces:

| Variant | Status | Extras |
| --- | --- | --- |
| `Backend`, `CircuitOpen` | 503 | `Retry-After`, rounded up and floored at 1 second |
| `LockHeld` | 409 | `Retry-After` |
| `LockLost` | 503 | |
| `Unsupported`, `Codec`, `Key`, `Config` | 500 | detail suppressed, so a namespace name never reaches a client |
| `Http(inner)` | whatever the inner error said | the round trip that keeps a 404 a 404 |

`Error::retryable()` is true for `Backend`, `CircuitOpen` and `LockHeld` only.
`Error::is_programmer_error()` is true for `Unsupported`, `Codec`, `Key` and `Config`. See
[the error model](./errors.md).

### The circuit breaker

The breaker sits between `Kv` and the store. After `failure_threshold` consecutive retryable
failures it opens, and every call fails immediately with `Error::CircuitOpen` and no round trip until
the cooldown ends. Under `degrade` that means a struggling Redis costs a miss instead of a connection
timeout on every request.

| Field | Default | Meaning |
| --- | --- | --- |
| `failure_threshold` | `5` | consecutive retryable failures before it opens |
| `cooldown` | 1 s | the first wait before a probe |
| `max_cooldown` | 30 s | the ceiling that repeated failed probes back off to |
| `jitter_percent` | `25` | how much the cooldown is randomised |

Three details are the whole design. Only transient failures count, because a decode failure or an
unsupported operation is a bug and opening a circuit over it would hide it. The cooldown is jittered,
so ten instances that opened at the same moment do not all probe at the same moment and give the
recovering store a thundering herd exactly when it is weakest. And exactly one caller becomes the
probe when the cooldown ends, won by a compare-exchange; a failed probe doubles the cooldown up to
`max_cooldown`.

`BreakerConfig::never()` disables it, which is what `Kv::in_memory` uses. `kv.breaker().state()`
returns `Closed`, `Open` or `HalfOpen`, and `kv.breaker().remaining()` says how long an open circuit
has left.

## Choosing a backend

| | Memory | Redis or Valkey | PostgreSQL |
| --- | --- | --- | --- |
| Feature | `memory` (default) | `redis` | `pg-kv` |
| Driver | `moka` | `fred` | `sqlx` |
| Survives a restart | no | yes | yes |
| Shared between processes | no | yes | yes |
| Pub/sub crosses a process | **no** | yes | yes |
| Server-side scripting | no | yes | **no** |
| `SCAN` | yes | yes, except on a cluster | yes |
| Extra infrastructure | none | a Redis server | a table in a database you already have |

Pick memory for tests and for a single-process development loop. It is not a toy: it implements the
same semantics as Redis including per-key TTL, compare-and-swap, lists, sets, sorted sets and
process-local pub/sub, so a test that passes there passes on Redis. TTL is enforced twice, by
`moka`'s eviction hook and again on every read, which is what makes "a TTL is honoured within 50 ms"
true rather than approximately true.

Pick Redis when you have more than one process and want the fastest option. `fred` gives pooling,
pipelining, cluster, sentinel, TLS and automatic reconnection; the URL schemes are `redis://`,
`rediss://`, `redis-cluster://` and `redis-sentinel://`. Compare-and-swap and `incr` are single Lua
scripts rather than `WATCH`/`MULTI`/`EXEC`, because an optimistic transaction needs a pinned
connection a pool cannot give.

Pick PostgreSQL when the team will not run a second datastore. It is one table with a TTL sweeper and
`LISTEN`/`NOTIFY` pub/sub. The table is created on boot, under a transaction advisory lock because
`CREATE TABLE IF NOT EXISTS` is not race free and every instance runs it:

```sql
CREATE TABLE moso_kv (
  key        text COLLATE "C" PRIMARY KEY,
  value      bytea       NOT NULL,
  kind       smallint    NOT NULL DEFAULT 0,
  expires_at timestamptz,
  updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX moso_kv_expires_at_idx ON moso_kv (expires_at)
  WHERE expires_at IS NOT NULL;
```

`COLLATE "C"` is load bearing: a prefix scan is `key LIKE 'prefix%'` and only a byte-ordered index
answers that without a sequential scan. Expiry is a predicate on every read, not a job, so an expired
row is invisible the microsecond it expires whether or not the sweeper has run. The sweeper only
reclaims space, which is why `sweep_interval = Duration::ZERO` is a supported choice if you would
rather run the reclaim from `cron`.

> [!WARNING]
> The PostgreSQL backend needs `CREATE` on its schema, because the cache creates its own table on
> every boot. There is no migration to run and no migration to review.

### Capabilities, and asking before you call

A backend says what it can do rather than pretending to be Redis. Sixteen of the twenty-nine
`KvStore` methods are optional, and calling an absent one is `Error::Unsupported`, a programmer error
and a 500.

| | `pubsub` | `pubsub_cross_process` | `structures` | `scan` | `atomic_cas` | `scripting` | `persistence` |
| --- | --- | --- | --- | --- | --- | --- | --- |
| memory | yes | **no** | yes | yes | yes | **no** | **no** |
| Redis | yes | yes | yes | yes* | yes | yes | yes |
| PostgreSQL | yes | yes | yes | yes | yes | **no** | yes |

\* a clustered Redis reports `scan: false`, because `SCAN` on a cluster walks one node and a
`delete_prefix` over a cluster would silently delete some of the keys and report a number.

Read them with `kv.capabilities()`. This is the flag a test gates on when it needs two processes to
talk: a test that gates on `pubsub` alone passes against the memory backend and then fails in
production.

### Writing your own

Ten required methods. The rest default to `Error::Unsupported`, and `get_many`, `set_many` and
`delete_prefix` have working defaults built out of the required ones.

```rust
use bytes::Bytes;
use moso_core::{BoxFuture, HealthStatus};
use moso_kv::{Capabilities, Key, KvStore, Result, ScanCursor, SetOpts};
use std::time::Duration;

/// A store that forgets everything.
#[derive(Debug, Default)]
pub struct NullStore;

impl KvStore for NullStore {
    fn name(&self) -> &'static str { "null" }
    fn capabilities(&self) -> Capabilities { Capabilities::none() }

    fn health(&self) -> BoxFuture<'_, HealthStatus> {
        Box::pin(async { HealthStatus::Up })
    }

    fn get<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Bytes>>> {
        Box::pin(async { Ok(None) })
    }
    fn set<'a>(&'a self, _k: &'a Key, _v: Bytes, _o: SetOpts) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(true) })
    }
    fn delete<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn exists<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn expire<'a>(&'a self, _key: &'a Key, _ttl: Duration) -> BoxFuture<'a, Result<bool>> {
        Box::pin(async { Ok(false) })
    }
    fn ttl<'a>(&'a self, _key: &'a Key) -> BoxFuture<'a, Result<Option<Duration>>> {
        Box::pin(async { Ok(None) })
    }
    fn incr<'a>(
        &'a self,
        _key: &'a Key,
        by: i64,
        _ttl: Option<Duration>,
    ) -> BoxFuture<'a, Result<i64>> {
        Box::pin(async move { Ok(by) })
    }
}
```

Every method returns a `BoxFuture` rather than being an `async fn`, because `Kv` holds an
`Arc<dyn KvStore>` and a dyn-compatible trait cannot have `async fn`. That boxing is the price of
choosing the backend by configuration instead of by a type parameter that would leak into every
signature that holds a cache.

The same trait is the seam for a fault-injecting wrapper in your own tests. `tests/degrade.rs` in the
crate is the pattern to copy: a `KvStore` that forwards to a real store until you call `go_down()`.

## What the counters say

```rust
let stats = kv.stats();
println!("{} hits, {} misses, ratio {:.2}", stats.hits, stats.misses, stats.hit_ratio());
```

`KvStats` carries `hits`, `misses`, `writes`, `errors`, `degraded`, `decode_failures`,
`flights_shared` and `revalidations`, plus `hit_ratio()`. It is deliberately constructible, so a test
can write `KvStats { hits: 90, misses: 10, ..Default::default() }`.

`degraded` is the number that matters. A healthy service with a rising `degraded` is a service whose
cache has silently stopped working, and nothing else in the request path will tell you.

> [!NOTE]
> These are in-process counters plus `tracing` events, which is `moso-kv`'s deliberate reporting
> surface: the crate leaves the named metrics (`moso_kv_errors_total` and friends the design
> documents mention) to the recorder seam rather than registering its own. Export `Kv::stats()` from
> your own recorder when you want them on a dashboard. See [observability](./observability.md).

## Failure modes and sharp edges

- **`Kv::keys` and `clear_namespace` need `Capabilities::scan`.** Both fail with `Error::Unsupported`
  on a clustered Redis. Bump `Namespace::VERSION` instead: it is instant and it does not walk a
  production keyspace.
- **A key longer than 1,024 bytes is `KeyError::TooLong`,** which is a programmer error and therefore
  a 500. An unbounded key part (a request body, a full URL) is the usual cause.
- **The derived prefix uses a simplified `snake_case`.** A run of capitals is one word, so
  `HTTPCache` becomes `httpcache` and not `http_cache`. Use `prefix = "..."` when that matters.
- **A namespace keyed by `()` has exactly one key, and it equals the namespace prefix.** That is
  fine: what must never coincide is two different keys.
- **A `Raw` namespace gets no age and no negative caching.** `CachedValue::age` is always
  `Duration::ZERO`, `negative` is always `false`, and `get_swr` will not compile against it.
- **`get_or_insert_with` requires `N::Value: Clone`,** because the winner's value is shared as an
  `Arc` and unwrapped at the boundary.
- **A degraded write is silent by design.** The `warn` log and the `degraded` counter are the only
  signal. Alert on the counter.
- **Changing a cached type's shape without bumping `version` is safe but wasteful.** Old bytes fail
  to decode, count in `decode_failures`, and are treated as misses until they expire.
- **The memory backend's capacity bound is entries, not bytes.** `KV_CAPACITY` defaults to 10,000 and
  eviction is `moka`'s, so a namespace with large values can evict a namespace with small ones.
- **There is no example application using `moso-kv` in the repository yet.** The crate's doctests and
  `crates/moso-kv/tests/cache.rs` are the closest thing to a reference.

## See also

- [Rate limiting and locks](./rate-limiting.md) for the other three things this handle does.
- [Configuration](./configuration.md) for the `#[derive(Config)]` struct you convert into a
  `KvConfig`.
- [Dependency injection](./dependency-injection.md) for what `Depends<Kv>` checks at boot.
- [Health and shutdown](./health-and-shutdown.md) for what a critical readiness check changes.
- [Errors](./errors.md) for how a `moso_kv::Error` becomes a problem document.
- [Testing](./testing.md), because `Kv::in_memory` is what a test uses and it needs nothing running.
