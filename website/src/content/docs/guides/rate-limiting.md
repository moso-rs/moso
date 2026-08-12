---
title: Rate limiting and locks
description: Apply GCRA rate limits as route guards, take leased distributed locks with fencing tokens, and publish typed messages across instances, all over one Kv handle.
order: 21
status: shipped
---

Three coordination primitives ride on the `Kv` handle from [cache](./cache.md): a rate limiter, a
distributed lock, and a publish/subscribe bus. They share the handle, the key layout and the circuit
breaker, so wiring one gets you the other two for free. None of them is a cache, and none of them
degrades quietly when the store is unreachable: a limiter that stops limiting, a lock that stops
locking and a publish that vanishes are all worse than an error.

What backend each needs is the first question, so here it is before anything else.

| | Works on memory | Works on Redis | Works on PostgreSQL | Coordinates across processes |
| --- | --- | --- | --- | --- |
| Rate limiting | yes | yes | yes | only on Redis or PostgreSQL |
| Distributed locks | yes | yes | yes | only on Redis or PostgreSQL |
| Pub/sub (`KvBus`) | yes | yes | yes | only on Redis or PostgreSQL |
| Pub/sub (`LocalBus`) | needs no backend at all | | | **never**, by design |
| Presence | yes | yes, unless clustered | yes | only on Redis or PostgreSQL |

The memory backend implements all three faithfully, which is what makes them testable with nothing
running. It just cannot see another process, and it says so through
`Capabilities::pubsub_cross_process`, which is `false`. A test that needs two instances to agree
must assert on that flag, because a test that only checks `pubsub` passes locally and fails in
production.

Two rows need a sentence each. `LocalBus` is a `tokio::broadcast` per channel and touches no store,
so it works with no `Kv` at all and never leaves the process. Presence needs `Capabilities::scan`,
which a clustered Redis does not report, so `KvBus::presence()` returns `None` there. If you run
Redis in cluster mode, treat presence as unavailable rather than as a feature you can reach for.

> [!IMPORTANT]
> Three shapes to know up front. `Slot::RateLimit` in the [middleware stack](./middleware.md) is a
> reserved position that stays empty by design: rate limiting ships as a `Guard` you attach to a
> router, which is the seam you fill. Distributed locks are a coordination tool, not a correctness
> guarantee across a Redis failover, and the section below says exactly what they are good for. And
> `KvBus` is a live fan-out without replay or pattern subscriptions, so `Last-Event-ID` resumption
> comes from your own log.

## Rate limiting

### Calling the limiter directly

```rust
use moso_kv::{Kv, RateQuota, Result};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let kv = Kv::in_memory("shop")?;
    let quota = RateQuota::new(2, Duration::from_secs(60));

    assert!(kv.rate_limit("ip:1.2.3.4", quota).await?.allowed);
    assert!(kv.rate_limit("ip:1.2.3.4", quota).await?.allowed);

    let third = kv.rate_limit("ip:1.2.3.4", quota).await?;
    assert!(!third.allowed);
    assert_eq!(third.remaining, 0);
    assert!(third.retry_after > Duration::ZERO);
    Ok(())
}
```

The bucket string is yours to choose here. `Kv::rate_key(bucket)` shows you the key it lands on.

### As a route guard

`RateLimit` implements `moso_core::Guard`, so you attach it with `Router::guard` and it enforces the
limit *and* writes the 429 into the OpenAPI document for every operation it covers, all four headers
included. A bare layer that rejects requests makes the document quietly wrong, which is the gap
guards exist to close.

```rust title="src/routes/auth.rs"
use moso_kv::{RateKey, RateLimit, RateQuota};
use std::time::Duration;

/// The authentication routes. Login is limited; the rest are not.
pub fn router() -> Router {
    moso::routes! {
        POST "/auth/login" => login,
    }
    .guard(
        RateLimit::new(RateQuota::new(10, Duration::from_secs(60)).burst(3))
            .key(RateKey::Ip)
            .scope("login"),
    )
}
```

`.guard(..)` applies to the routes registered **before** the call, which is the same scoping rule as
`Router::layer`. Split the table in two when only some routes should be limited. See
[routing](./routing.md).

The guard resolves its `Kv` from the provider map on every request, so `App::provide(kv)` has to have
happened. That resolution is the one path in the crate that is not checked at boot: a missing
provider is an error at check time, not at build time. `RateLimit::with_kv(kv)` sidesteps it by
holding the handle directly, which is also what a unit test wants.

### Why not the `rate_limit` middleware slot

`Slot::RateLimit` in the [middleware stack](./middleware.md) is a position with no built-in, and it
stays that way on purpose rather than for want of a way to fill it. Filling it is now one call
(`s.replace_custom(Slot::RateLimit, MyLimiterLayer)` takes any `CustomLayer`), but a layer installed
there is invisible to the OpenAPI document, so every operation would claim it cannot return 429 while
the running service returns 429. That is exactly the drift guards exist to delete, which is why
`moso-kv` ships a `Guard` and not a layer.

Reach for the slot when the limit is genuinely global and shape-free (a per-IP flood cut-off in
front of every routed request, 404s included), and accept that it will not appear in the document.
It still will not cover `/healthz`, `/readyz`, `/docs` or `/openapi.json`, which are mounted on the
outer router and never enter the stack. Reach for the guard for anything a client is expected to
handle.

### Quotas

`RateQuota::new(limit, period)` means "at most `limit` requests per `period`", implemented as GCRA:
one theoretical arrival time per bucket, one atomic operation per request, and no window to fall
across. A fixed window would admit twice the quota over a boundary (ten requests at 11:59:59 and ten
more at 12:00:00 is twenty in one second under a limit of ten a minute). A sliding window log fixes
that too but costs one stored entry per request.

| Call | Meaning |
| --- | --- |
| `RateQuota::new(limit, period)` | the quota. `burst` defaults to `limit` |
| `.burst(n)` | how much of the quota may arrive at once, clamped to `1..=limit` |
| `.emission_interval()` | `period / limit`, the steady-state spacing |
| `.tolerance()` | `emission_interval * burst`, how far ahead a client may run |

A `burst` smaller than the limit smooths traffic: `RateQuota::new(60, minutes(1)).burst(5)` allows one
request per second on average and at most five back to back. A `limit` of zero is clamped to one, and
a `burst` over the limit is clamped down.

### Buckets

A bucket name is `<scope>:<kind>:<value>`, so two scopes never share a bucket and two kinds within a
scope never do either. The kind comes from `RateKey`.

| `RateKey` | Value | Notes |
| --- | --- | --- |
| `RateKey::Ip` | the peer address | the default. **Not** `X-Forwarded-For` |
| `RateKey::User` | a `RateSubject` in the request extensions | you insert it, see below |
| `RateKey::header("x-api-key")` | that header's value | missing header falls back to the peer address |
| `RateKey::Global` | the empty string | one bucket for the whole route |
| `RateKey::custom(\|parts\| ..)` | whatever your closure returns | `None` falls back to the peer address |

The fallback rule, stated once and worth repeating: a request whose key produces no value falls back
to its peer address, and one with no peer address falls into a single shared `unknown` bucket.
Sharing a bucket is the safe direction, because it limits harder rather than less.

`RateKey::Ip` reads `ConnectInfo<SocketAddr>` from the request extensions and deliberately ignores
`X-Forwarded-For`. An IP taken from a header the client controls turns a rate limiter into a
rate-limit bypass. Behind a trusted proxy you have two options: terminate at a proxy that limits, or
use `RateKey::custom` from a handler-side limiter where `moso_core`'s `ClientIp` extractor (which
honours the header, but only for a peer named by `http.trusted_proxies`) has already run.
`Guard::check` gets `&Parts` and not `&mut Parts`, which is why the extractor cannot run inside the
guard.

> [!WARNING]
> Nothing in the framework inserts a `RateSubject` today, so `RateKey::User` silently falls back to
> the peer address unless your own middleware puts one in: `parts.extensions.insert(RateSubject::new(format!("user:{id}")))`
> after authentication. `moso-kv` does not know how identity works, on purpose.

### Scopes

The scope is the name of the limit. A login endpoint limited to 10 a minute and a search endpoint
limited to 100 a minute must not consume each other's quota, and the scope is what keeps them apart.
`RateLimit::new(..)` defaults to `"default"`, which is fine until there are two limits.

### The decision and its headers

`RateDecision` is what both paths produce.

| Field | Type | Meaning |
| --- | --- | --- |
| `allowed` | `bool` | whether this request may proceed |
| `limit` | `u32` | the quota, echoed |
| `remaining` | `u32` | how many more are available right now |
| `retry_after` | `Duration` | zero when allowed |
| `reset` | `Duration` | when the bucket returns to empty |

`headers()` returns three entries when allowed (`x-ratelimit-limit`, `x-ratelimit-remaining`,
`x-ratelimit-reset`) and four when not, the fourth being `retry-after` rounded **up** and floored at
one second, so a sub-second wait never invites an immediate retry. `into_error()` turns a denial into
the `moso_core::Error` a handler returns, which is a 429 problem document carrying all four.

A guard can only decorate the rejection. `Guard::check` returns `Result<()>` and never sees a
successful response, so a guard cannot put `x-ratelimit-remaining` on a 200. That limit is documented
rather than worked around: when you want the headers on every response, call `decide` from your own
middleware and copy them across.

```rust title="src/middleware/limits.rs"
use moso::middleware::Next;
use moso::prelude::*;
use moso::{Request, Response};
use moso_kv::{Kv, RateLimit, RateQuota};
use std::time::Duration;

/// Charge the request and attach the headers to whatever comes back.
#[moso::middleware]
pub async fn rate_headers(Inject(kv): Inject<Kv>, req: Request, next: Next) -> Result<Response> {
    let limit = RateLimit::new(RateQuota::new(100, Duration::from_secs(60))).scope("api");
    let (parts, body) = req.into_parts();
    let decision = limit.decide(&kv, &parts).await?;

    if !decision.allowed {
        return Err(decision.into_error());
    }

    let mut response = next.run(Request::from_parts(parts, body)).await;
    for (name, value) in decision.headers() {
        response.headers_mut().insert(name, value);
    }
    Ok(response)
}
```

There is no `on_exceed` hook and no way to change the 429 body from the guard. If you need a
different body, that middleware is where it goes.

### When the store is unreachable

Rate limiting never degrades. A backend failure propagates as `Error::Backend`, which is a 503 with a
`Retry-After`, and you decide what that means for your service. A limiter that quietly stops limiting
when the store blinks is worse than one that says so.

On a backend with scripting (Redis) the whole decision is one `EVAL`. On a backend without it (memory
and PostgreSQL) it is a read and a conditional compare-and-swap in a loop of at most 32 attempts.
Both give the same answer. Thirty-two is deliberately generous, because every retry is a lost race
with another request for the same bucket and losing a race must not turn into a spurious 429; a quota
of ten admits exactly ten, not roughly ten under contention. If all 32 attempts lose, the request is
refused with a `warn`, because a bucket that hot is over its quota anyway.

The clock is the wall clock, not `Instant`, because the state is shared between processes and a
monotonic clock is not comparable across them. A backwards clock jump makes the limiter briefly more
permissive, which is the safe direction.

## Distributed locks

> [!CAUTION]
> This is not safe as a correctness mechanism across a Redis failover. It is Redlock-lite: a
> single-instance lock with a fencing token, an auto-renewed lease and release on drop. If the
> primary fails over to a replica that had not yet received the write, two processes can hold the
> same lock at the same time. No amount of retrying fixes that; it is a property of an
> asynchronously replicated store.

Use it for what it is good at: stopping duplicate work (one importer, one nightly report, one cache
warm) where doing it twice is wasteful rather than wrong, and leader election where the thing being
led tolerates a split brain. When two holders would be a *bug*, use PostgreSQL advisory locks
(`pg_advisory_lock`, which `moso-kv` does not wrap), or make the operation idempotent and skip the
lock entirely.

### Taking one

```rust
use moso_kv::{Kv, Result};
use std::time::Duration;

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let kv = Kv::in_memory("shop")?;

    let guard = kv.lock("import:acme", Duration::from_secs(30)).await?;
    assert!(guard.token() > 0);

    // Somebody else cannot take it while it is held.
    assert!(kv.try_lock("import:acme", Duration::from_secs(30)).await?.is_none());

    drop(guard);
    Ok(())
}
```

Three entry points, all on `Kv`:

| Call | Waits | On contention |
| --- | --- | --- |
| `lock(name, lease)` | up to `lease` | `Error::LockHeld` after the wait, a 409 with `Retry-After` |
| `try_lock(name, lease)` | no | `Ok(None)` |
| `lock_with(name, LockOptions)` | as configured | `Error::LockHeld` |

`LockOptions` is the full set of knobs:

| Field | Default | Meaning |
| --- | --- | --- |
| `lease` | required | how long the lock survives without a renewal |
| `wait` | zero | how long to keep trying before giving up |
| `retry_interval` | 50 ms | how often to retry while waiting |
| `auto_renew` | `true` | spawn a renewer at `lease / 3`, floored at 50 ms |

```rust
use moso_kv::LockOptions;

let guard = kv
    .lock_with(
        "nightly-report",
        LockOptions::new(Duration::from_secs(60))
            .wait(Duration::from_secs(5))
            .retry_interval(Duration::from_millis(200)),
    )
    .await?;
```

The lease is the thing that makes a lock survive a crash. A process that dies holding one releases it
when the lease expires, with nothing to clean up. Pick a lease longer than a normal run of the work
and leave `auto_renew` on: the renewer extends it every `lease / 3` for as long as the guard is
alive, and a renewal that finds the lock is no longer yours returns `Error::LockLost`.

### The guard

| Call | Returns | Notes |
| --- | --- | --- |
| `guard.name()` | `&str` | the name it was taken under |
| `guard.token()` | `i64` | the fencing token, strictly increasing |
| `guard.lease()` | `Duration` | |
| `guard.key()` | `&Key` | `moso:v1:shop:lock:1:import` |
| `guard.is_held().await` | `bool` | whether the stored token is still ours |
| `guard.renew().await` | `()` | `Error::LockLost` when it is not |
| `guard.release().await` | `bool` | consumes the guard, reports whether it was still ours |

### Fencing tokens

Every acquisition gets a strictly increasing `i64` from a counter beside the lock. Pass it to whatever
the lock protects and have that thing reject a token lower than the highest it has seen. That is the
only construction that survives a paused process: a holder that stalls past its lease and wakes up
still believing it holds the lock presents a stale token and is refused.

The counter is incremented **before** the acquisition attempt and unconditionally, so a failed
attempt burns a token. Two contenders can therefore never be handed the same number, which is the
whole point.

### Failure modes

- **`try_lock` can see a just-released lock as still held.** `Drop` is not `async`, so dropping a
  guard spawns the release rather than awaiting it. `lock` waits and still succeeds; `try_lock` does
  not wait and can lose the race. `guard.release().await` is the spelling for "I need it back now",
  and it is also the one that reports a lost lease.
- **Dropping a guard outside a Tokio runtime releases nothing.** It logs at `debug` and the lease
  does the releasing instead.
- **A lock is not a transaction.** Losing the lease mid-operation does not roll anything back. If the
  work must not be applied twice, gate the write on the fencing token or make it idempotent.
- **The fencing counter key has no TTL,** because a token must never repeat. One small key per lock
  name persists for the lifetime of the keyspace.
- **Locks need `Capabilities::atomic_cas`,** which all three shipped backends have. A custom
  `KvStore` without compare-and-delete cannot hold locks and fails with `Error::Unsupported`.
- **On the memory backend a lock is process-local,** which is exactly what a test wants and exactly
  what production does not.

## The pub/sub bus

The bus is at-most-once notification, not a queue. Messages published while nothing is listening are
gone, on every backend. For work that must not be lost, use [background jobs](./jobs.md).

### Raw channels

`Kv` exposes the store's pub/sub as bytes:

```rust
use bytes::Bytes;
use futures_util::StreamExt as _;

let mut stream = kv.subscribe("deploys").await?;
kv.publish("deploys", Bytes::from_static(b"v2 live")).await?;

let payload = stream.next().await.expect("a message");
```

These channels are **not** namespaced. Two applications sharing one Redis will hear each other unless
they prefix their own channel names. Publishes never degrade, for the same reason rate limits do not:
a dropped notification is not a cache miss.

### Typed topics

A `Topic` binds a message type to a channel name plus an instance, so one topic type serves every
user without a subscriber ever hearing somebody else's messages.

```rust title="src/realtime/topics.rs"
use moso_kv::Topic;
use serde::{Deserialize, Serialize};

/// What arrives in a user's notification feed.
#[derive(Serialize, Deserialize)]
pub struct Notification {
    /// What to show.
    pub text: String,
}

/// One user's notification feed.
pub struct UserNotifications(pub u64);

impl Topic for UserNotifications {
    type Message = Notification;
    const NAME: &'static str = "notifications";

    fn instance(&self) -> std::borrow::Cow<'_, str> {
        self.0.to_string().into()
    }
}
```

The channel is `{NAME}:{instance}`, so `UserNotifications(7).channel()` is `notifications:7`. A topic
whose `instance` is empty has one channel, named for the topic.

`TypedBus` is implemented for every `Bus`, including `dyn Bus`, so the typed methods are available
wherever you inject one:

```rust
use futures_util::StreamExt as _;
use moso_kv::{LocalBus, TypedBus as _};

let bus = LocalBus::new();
let topic = UserNotifications(7);
let mut stream = bus.subscribe(&topic).await.expect("subscribes");

bus.publish(&topic, &Notification { text: "your order shipped".to_owned() })
    .await
    .expect("publishes");

assert_eq!(stream.next().await.expect("a message").text, "your order shipped");
```

Swap `LocalBus::new()` for `KvBus::new(kv)?` and the same code crosses a process boundary, provided
the `Kv` is over Redis or PostgreSQL. That is the whole point of the split: handler code does not
change.

### Choosing a bus

| | `LocalBus` | `KvBus` |
| --- | --- | --- |
| Backing | one `tokio::broadcast` per channel | the store's pub/sub |
| `cross_process` | **false** | the store's `pubsub_cross_process` |
| `replay` | false | false |
| `patterns` | **true** | **false** |
| `presence` | true | only when the store can `scan` |
| `max_message_bytes` | 1 MiB | 1 MiB |
| Channel names | as written | prefixed `moso:v1:<app>:<channel>` |

`KvBus::new` checks the `pubsub` capability at construction, so a store that cannot publish is a boot
error and not a surprise at the first message. `KvBus::name()` returns the underlying store's name,
so a `KvBus` over the memory store reports `"memory"`.

Pattern subscriptions are `LocalBus` only, because nothing in `KvStore` exposes `PSUBSCRIBE`.
`KvBus::subscribe_pattern` always fails with `Error::Unsupported`, and `LocalBus::subscribe_pattern`
only merges channels that already exist at subscribe time, matching what Redis does within one
connection's lifetime.

### Presence

Presence is a set of keys with TTL heartbeats, so a crashed process expires rather than leaving a
ghost in the member list. Reach it through `Bus::presence()`, which returns `None` when the backend
cannot track it.

```rust
let presence = bus.presence().expect("this bus tracks presence");

presence.join("room:1", "ada", Duration::from_secs(30)).await?;
assert_eq!(presence.members("room:1").await?, vec!["ada"]);
assert_eq!(presence.count("room:1").await?, 1);

// A heartbeat within the lease keeps it alive; without one the member disappears.
presence.heartbeat("room:1", "ada", Duration::from_secs(30)).await?;

// Leaving is immediate rather than waiting out the lease.
presence.leave("room:1", "ada").await?;
```

`KvBus` defaults to a 30 second presence TTL with clients heartbeating every ten, so a member can
miss two heartbeats before disappearing. That survives a garbage collection pause without leaving a
ghost for a minute. `KvBus::presence_ttl(..)` changes it.

### Failure modes

- **At most once, everywhere.** A message published while nothing is listening is gone. There is no
  acknowledgement and no redelivery.
- **No replay.** `BusCapabilities::replay` is false on both shipped buses, so a subscriber that
  reconnects sees only what arrives after it reconnects. SSE `Last-Event-ID` resumption has to come
  from your own event log. See [server sent events and realtime](./realtime.md).
- **A slow subscriber misses messages rather than being disconnected.** `LocalBus` buffers 256 per
  subscriber (`LocalBus::buffer(n)` changes it) and the memory backend's channels buffer 256. Falling
  behind logs a `warn` with a `missed` count and the stream continues.
- **A message that fails to decode is skipped, not surfaced.** One bad publish from an older deploy
  must not end a subscriber's stream and disconnect every socket behind it. `TopicStream::skipped()`
  counts them, and a non-zero value means a publisher and a subscriber disagree about a message
  shape, which is a deploy-ordering problem worth seeing.
- **`KvBus` refuses a message over 1 MiB** with an error naming the channel, rather than letting the
  driver reject it later.
- **PostgreSQL caps channel names at 63 bytes and payloads at 3,999 bytes.** Longer is
  `Error::Channel`, never a silent truncation. The payload figure is because `moso-kv` hex-encodes so
  arbitrary bytes survive a `text` round trip, against PostgreSQL's 8,000 byte wire limit.
- **`Bus::probe()` is the liveness check.** Use it in a readiness probe when realtime is critical to
  the service, rather than assuming a subscribe that succeeded once still works.

## See also

- [Cache and key value store](./cache.md) for the `Kv` handle, backends and the circuit breaker these
  all share.
- [Middleware](./middleware.md) for why `Slot::RateLimit` is empty, how `replace_custom` fills it,
  and how a guard differs from a layer.
- [Routing](./routing.md) for the `.guard(..)` scoping rule.
- [OpenAPI](./openapi.md) for what `Guard::describe` writes into an operation.
- [Server sent events and realtime](./realtime.md) for the transport on top of the bus.
- [Background jobs](./jobs.md) when a message must not be lost.
- [Errors](./errors.md) for the 429, 409 and 503 these produce.
