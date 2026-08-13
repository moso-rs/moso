# moso-kv

Moso's key-value and cache layer.

One trait - `KvStore` - with three implementations (an in-process `moka` cache, Redis via
`fred`, and a PostgreSQL table), a typed namespace layer on top of it, and the four patterns
every application ends up hand-rolling: single-flight caching, stale-while-revalidate,
distributed locks, and rate limiting.

```rust
use moso_kv::{minutes, seconds, Kv, Result};

moso_kv::namespace! {
    /// Cached user profile, refreshed on write.
    pub Profile: u64 => Option<String>, ttl = minutes(15), negative_ttl = seconds(30);

    /// The session record. Losing one logs somebody out, so it fails loudly.
    pub Session: String => String, ttl = minutes(480), on_failure = fail;
}

async fn profile(kv: &Kv, id: u64) -> Result<Option<String>> {
    kv.get_or_insert_with::<Profile, _, _>(&id, || async {
        Ok(Some(String::from("…from the database…")))
    })
    .await
}
```

## What is in the box

| | |
| --- | --- |
| `KvStore` | 26 operations, `capabilities()`, dyn-compatible |
| `namespace!` | typed, versioned, collision-free key prefixes |
| `cached!` | single-flight de-duplication and negative caching |
| `Kv::get_swr` | stale-while-revalidate |
| `Kv::lock` | fencing token, auto-renewal, release on drop |
| `RateLimit` | GCRA, as a `Guard`, with `X-RateLimit-*` headers |
| `FailureMode` | degrade-or-fail per namespace, with a circuit breaker |

## Backends

| Backend | Feature | Notes |
| --- | --- | --- |
| memory | `memory` (default) | `moka`; full semantics, no cross-process pubsub |
| Redis | `redis` | `fred`: pooling, pipelining, cluster, sentinel, TLS, auto-reconnect |
| PostgreSQL | `pg-kv` | one table, a TTL sweeper, and `LISTEN`/`NOTIFY` for pubsub |

Backend choice is configuration, not code:

```toml
[kv]
backend = "redis"
url = "redis://localhost:6379"
pool_size = 8
```

## Running the tests against all three

The memory leg always runs. The other two skip with a message unless their URL is set, so the
suite passes on a machine with no Docker.

```sh
docker compose -f compose.test.yaml up -d --wait
export DATABASE_URL="postgres://moso:moso@localhost:55433/moso_test"

docker run -d --name moso-kv-test-redis -p 56379:6379 redis:7-alpine
export REDIS_URL="redis://localhost:56379"

cargo test -p moso-kv --features redis,pg-kv
```

## Licence

MIT - see the root [`LICENSE`](../../LICENSE).
