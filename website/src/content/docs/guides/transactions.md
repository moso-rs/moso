---
title: Transactions and pooling
description: Run work atomically with retrying closures, savepoints and request-scoped transactions, and size the connection pool so an exhausted one answers 503 instead of hanging.
order: 16
status: shipped
---

Moso gives you three ways to be atomic, in increasing order of how much you have to think about.
`Db::transaction` takes a closure and retries it when the server says the transaction lost a race.
`Db::begin` hands you a handle when a closure does not fit. `Depends<RequestTx>` in a handler
signature makes the whole request one transaction, committed after a 2xx and rolled back otherwise.

Underneath all three is one pool with deliberately small defaults, a bounded acquire timeout so an
exhausted pool answers `503` rather than never answering at all, and a 30-minute connection lifetime
so a failover drains without a restart. This page covers all of it, including the parts that fail.

> [!IMPORTANT]
> The request-scoped transaction is implemented and tested, and you reach for it with `Db::transaction`
> inside the handler, which is the supported entry point. `RequestTxLayer` implements
> `moso_core::middleware::CustomLayer` and is provided for forward compatibility with a middleware
> installation point; the public installation points (`Router::layer`, `MiddlewareStack::append`,
> `insert_before`, `insert_after`, `replace`) take a `tower::Layer<Route>` today, so keep the
> transaction in the handler. The rest of this page is shipped.

## The retrying closure

This is the form to reach for. It commits on `Ok`, rolls back on `Err` or panic, and retries a
serialisation failure or a deadlock with jittered exponential backoff.

```rust title="src/services/orders.rs"
use moso::db::prelude::*;

/// Move an item from one order to another, atomically.
pub async fn transfer(db: &Db, item: i64, to: i64) -> Result<()> {
    db.transaction(async |tx| {
        Item::update_all()
            .filter(Item::ID.eq(item))
            .set(Item::ORDER_ID, to)
            .execute(tx)
            .await?;
        Order::update_all()
            .filter(Order::ID.eq(to))
            .set(Order::UPDATED_BY, "transfer")
            .execute(tx)
            .await?;
        Ok(())
    })
    .await
}
```

The argument is a closure and not a handle for one reason: a retry has to be able to re-run the
body. A handle-based API cannot retry, because by the time the failure is observed the caller's
statements have already been issued and their results consumed.

> [!CAUTION]
> The closure can run more than once. Nothing outside the database belongs inside it: no HTTP call,
> no mail, no writes to a file. Collect what you need to do, do it after the transaction commits.

## Writing a function once for both

`Executor<'e>` is the trait every query method takes. It is sealed and has exactly four
implementors: `&Db`, `&Tx`, `&mut Tx` and `&RequestTx`. So a service function written against
`impl Executor<'_>` runs unchanged inside a transaction and outside one:

```rust title="src/services/users.rs"
/// Find a user by email, on a pool or inside a transaction.
pub async fn by_email(email: &str, ex: impl Executor<'_>) -> Result<Option<User>> {
    User::query().filter(User::EMAIL.eq(email)).fetch_optional(ex).await
}

// Both of these compile, and there is only one function.
let outside = by_email("ada@example.com", &db).await?;
let inside = db.transaction(async |tx| by_email("ada@example.com", tx).await).await?;
```

The trait has one required method, `handle()`, and everything else lives on the concrete `Handle`.
That is why `Select<User>::fetch_all` monomorphises once per entity rather than once per pairing of
entity and executor.

## Opening a transaction by hand

When the work does not fit in a closure (a long function with early returns, a handle you pass
around), `Db::begin` gives you a `Tx`:

```rust
let tx = db.begin().await.expect("begin");
run(&tx, "insert into t values (1, 10)").await.expect("insert");
tx.commit().await.expect("commit");

let tx = db.begin().await.expect("begin");
run(&tx, "insert into t values (2, 20)").await.expect("insert");
tx.rollback().await.expect("rollback");

{
    let tx = db.begin().await.expect("begin");
    run(&tx, "insert into t values (1)").await.expect("insert");
    // No commit, no rollback: the handle simply goes out of scope.
}
// The row is gone, and `db.ping()` proves the connection came back usable.
```

`Db::begin` does **not** retry, and `Db::begin_with(options)` ignores `max_retries` for the same
reason: there is no body to re-run.

### Four things roll a transaction back

1. The closure returns `Err`.
2. You call `Tx::rollback`.
3. The task **panics**.
4. The `Tx` is dropped without either.

The last two are the same mechanism. The driver transaction is owned by the `Tx`, so unwinding past
it issues the rollback and returns the connection to the pool. The pool is not poisoned in any of
the four cases, which is asserted by a test that panics inside a transaction and then keeps using
the pool.

## Savepoints

`Tx::savepoint` runs a closure inside a named savepoint: released when the closure returns `Ok`,
rolled back to when it returns `Err`. The outer transaction survives either way.

```rust
db.transaction(async |tx| {
    run(tx, "insert into t values (1)").await?;

    tx.savepoint(async |sp| {
        run(sp, "insert into t values (2)").await?;
        Ok(())
    })
    .await?;

    // A savepoint whose body fails takes its own writes with it and
    // leaves the outer transaction alive.
    let failed: Result<()> = tx
        .savepoint(async |sp| {
            run(sp, "insert into t values (3)").await?;
            Err(Error::not_found("deliberate"))
        })
        .await;
    assert!(failed.is_err());

    // Nesting works, and the outer transaction is still writable.
    tx.savepoint(async |sp| {
        sp.savepoint(async |inner| {
            run(inner, "insert into t values (4)").await?;
            Ok(())
        })
        .await
    })
    .await?;

    run(tx, "insert into t values (5)").await?;
    Ok(())
})
.await
.expect("the transaction as a whole succeeded");
```

Rows 1, 2, 4 and 5 survive. Row 3 does not.

Savepoint names come from a counter on the shared transaction state (`moso_sp_0`, `moso_sp_1`), so
two sibling savepoints at the same depth never collide. `Tx::depth()` tells you where you are, with
`0` for the outermost transaction.

A savepoint handle refuses to commit or roll back the *whole* transaction, with an error naming the
fix: return `Ok` from the closure to release it, `Err` to roll back to it.

## Isolation, access mode and timeouts

`TxOptions` is what `transaction_with` and `begin_with` take.

| Field | Type | Default | What it does |
| --- | --- | --- | --- |
| `isolation` | `Isolation` | `ReadCommitted` | `ReadCommitted`, `RepeatableRead` or `Serializable` |
| `read_only` | `bool` | `false` | refuses writes, see below |
| `deferrable` | `bool` | `false` | `DEFERRABLE`, meaningful only with serialisable plus read-only |
| `max_retries` | `u32` | `3` | attempts after the first, on a transient conflict |
| `statement_timeout` | `Option<Duration>` | `None` | `SET LOCAL statement_timeout` for this transaction |
| `lock_timeout` | `Option<Duration>` | `None` | `SET LOCAL lock_timeout` for this transaction |

Each has a `const` builder: `TxOptions::new().isolation(Isolation::Serializable).read_only()`.

The level and the access mode ride on the `BEGIN` itself rather than a `SET TRANSACTION` afterwards.
That is one round trip instead of two, and there is no window in which the transaction is open at
the wrong level:

| Options | PostgreSQL | SQLite |
| --- | --- | --- |
| defaults | `begin isolation level read committed read write` | `begin immediate` |
| `.isolation(Serializable).read_only().deferrable()` | `begin isolation level serializable read only deferrable` | not applicable |
| `.deferrable()` alone | `begin isolation level read committed read write` | not applicable |
| `.read_only()` | `... read only` | `begin` |

`.deferrable()` on its own is dropped, because it means nothing outside a read-only serialisable
transaction.

### Read-only transactions

A read-only transaction refuses a write **before** it is sent, on both backends: Moso classifies the
statement it built and returns an error rather than a round trip.

Raw SQL is not parsed, so there is one gap. On PostgreSQL the server catches a raw write in a
read-only transaction as SQLSTATE `25006`. On SQLite it does not, so a raw write inside a read-only
transaction is the one case that gets through. Build the statement rather than writing it out when
you can.

## Retries on serialisation failure

At `Serializable`, PostgreSQL can abort a transaction that would have produced a non-serialisable
outcome. That is not a bug in your code, it is the level working, and the correct response is to run
it again. `Db::transaction` does that for you:

```rust
let gate = Arc::new(tokio::sync::Barrier::new(2));
let options = TxOptions::new()
    .isolation(Isolation::Serializable)
    .max_retries(5);

/// One half of the write skew. Only the *first* attempt waits at the
/// gate; a retry must not, because its partner is not coming back.
async fn skew(
    db: &Db,
    options: TxOptions,
    table: &str,
    read: i32,
    write: i32,
    gate: Arc<tokio::sync::Barrier>,
) -> Result<()> {
    let attempts = AtomicU32::new(0);
    db.transaction_with(options, async |tx| {
        let first = attempts.fetch_add(1, Ordering::Relaxed) == 0;
        run(tx, &format!("select v from {table} where id = {read}")).await?;
        if first {
            gate.wait().await;
        }
        run(tx, &format!("update {table} set v = 1 where id = {write}")).await?;
        Ok(())
    })
    .await
}

let (a, b) = tokio::join!(
    skew(&db, options.clone(), &table, 1, 2, Arc::clone(&gate)),
    skew(&db, options, &table, 2, 1, Arc::clone(&gate)),
);
a.expect("the first half retried its way through");
b.expect("and so did the second");
```

With `max_retries(0)` the same pair produces exactly one loser, and the loss is
`Error::Serialization` whose `is_retryable()` is `true` and whose `sqlstate()` is `Some("40001")`.

### What is retried, and what is not

| Error | SQLSTATE | Retried by the loop |
| --- | --- | --- |
| `Error::Serialization` | `40001` | yes |
| `Error::Deadlock` | `40P01` | yes |
| `Error::PoolTimeout` | none | **no** |
| `Error::UniqueViolation` and every other constraint error | `23xxx` | no, the body runs once |

`Error::is_retryable()` includes `PoolTimeout`, but the transaction retry loop deliberately does
not. A pool timeout means the process is out of connections, and retrying immediately makes that
worse. It is reported so the caller, usually the HTTP layer as a `503` with a `Retry-After`, can
shed load instead.

Backoff is exponential from 20ms, capped at one second, with jitter drawn from real operating-system
entropy rather than a hash of the clock. A clock hash would correlate across exactly the tasks the
jitter exists to separate.

## Row locks

A transaction is where `SELECT ... FOR UPDATE` belongs. `Select::lock` and `lock_with` take a mode
and a behaviour:

```rust
// Claim the next task nobody else has claimed.
let claimed = Task::query()
    .filter(Task::STATE.eq("queued"))
    .order_by(Task::ID.asc())
    .limit(1)
    .lock_with(LockMode::ForUpdate, LockBehavior::SkipLocked)
    .fetch_optional(tx)
    .await?;
```

`LockMode` is `ForUpdate`, `ForNoKeyUpdate`, `ForShare` or `ForKeyShare`. `LockBehavior` is `Wait`
(the default), `SkipLocked` or `NoWait`. `SkipLocked` is the queue-worker idiom, and it is what
[background jobs](./jobs.md) use.

## Advisory locks

For mutual exclusion that no row represents (one nightly rebuild across a fleet, one migration
runner), PostgreSQL advisory locks are keyed by a 64-bit integer and nothing else.

Prefer the transaction-scoped form. The commit releases it and there is no guard to leak:

```rust
use moso_orm::db::AdvisoryKey;

db.transaction(async |tx| {
    tx.advisory_lock(AdvisoryKey::hashed("rebuild-search-index")).await?;
    rebuild(tx).await
})
.await?;
```

`AdvisoryKey::of(i64)` takes a raw number, `AdvisoryKey::pair(namespace, key)` splits it into two
32-bit halves, and `AdvisoryKey::hashed(name)` derives it from a name with a stable FNV-1a hash that
gives the same number in every process and every release. Prefer `hashed`: `AdvisoryKey::of(1)`
collides with every other subsystem that also picked `1`.

`Tx::try_advisory_lock` returns `false` instead of waiting.

The session-level form, `Db::advisory_lock`, returns an `AdvisoryLock` guard and holds a connection
**out of the pool** for as long as the guard lives, because a connection that went back to the pool
would carry the lock to whoever got it next. Dropping the guard without calling `unlock()` closes
the connection rather than returning it, which releases the lock at the cost of a reconnect.

Both forms are `Error::Unsupported` on SQLite, which has no advisory locks.

## Enqueuing a job on the transaction

`moso_jobs::Enqueue` is a blanket implementation over `Executor`, so a job enqueued on a transaction
commits and rolls back with the work that caused it:

```rust
use moso::jobs::Enqueue;

db.transaction(async |tx| {
    let user = User::insert(new_user.clone()).returning_entity().fetch_one(tx).await?;
    tx.enqueue(SendWelcomeEmail, user.id).await?;
    Ok(user)
})
.await?;
```

Enqueuing on `&db` works too, but there is no transaction to join, so the row is committed on its
own. See [background jobs](./jobs.md).

## Request-scoped transactions

Adding `Depends<RequestTx>` to a handler signature makes that request one atomic unit:

```rust title="src/routes/orders.rs"
/// Place an order.
#[endpoint]
async fn place(
    Json(body): Json<PlaceOrder>,
    Depends(tx): Depends<RequestTx>,
) -> Result<Created<OrderOut>> {
    let order = Order::insert(body.order_row())
        .returning_entity()
        .fetch_one(&tx)
        .await?;
    Item::insert_many(body.item_rows(order.id)).execute(&tx).await?;
    Ok(Created::at(format!("/orders/{}", order.id), OrderOut::from(order)))
}
```

`PlaceOrder` and `OrderOut` are `#[derive(Schema)]` types; `Order` and `Item` are entities. Keeping
them apart is deliberate, and [entities are not schemas](./schemas.md) is the
argument.

How it works: the layer inserts an empty slot into the request extensions and opens nothing. The
handler's `Depends<RequestTx>` fills the slot lazily on first ask. A second `Depends<RequestTx>` in
the same signature, or in a nested extractor, joins the existing transaction rather than opening a
second one. After the handler returns, the layer looks at the response status and commits or rolls
back.

The split into a layer plus a dependency is forced. "Did this succeed" is only knowable after the
handler has returned, which a dependency cannot observe. "Is a transaction wanted at all" is only
knowable from the handler's signature, which a layer cannot see. A handler that never asks never
opens one and never holds a connection.

| Situation | Outcome |
| --- | --- |
| Response status `< 300` | commit |
| Response status `300..500` | rollback, unless `RequestTxLayer::commit_on_client_error()` |
| Response status `>= 500`, or a panic | rollback |
| The commit itself fails on a would-be 2xx | the response is replaced by the error it really was |
| The handler never asked for `RequestTx` | nothing was opened, nothing to do |

`commit_on_client_error` exists for applications that record a rejected attempt (a failed login, a
rate-limit hit) in the same transaction that rejected it.

`RequestTx::options()` is your application's `TxOptions` with `max_retries(0)`. A request
transaction never retries: the request body has already been consumed, and re-running a handler is
not something a middleware can do. A serialisation failure becomes a `409` with `retryable: true`,
and the fix is to move that work into `Db::transaction`, whose closure can be re-run.

A `RequestTx` clone stashed in a spawned task cannot write after the response is sent. The
transaction ends when the response is written, and a later statement on the stale clone gets
"this transaction has already been committed". That is what request-scoped means.

### What happens when a handler returns an error

Nothing special, which is the point. The handler returns `Err`, the error becomes a response, the
layer sees a non-2xx status and rolls back. Your handler does not call `rollback` and does not need
a `?` guard around every statement.

The mapping from a data-layer error to that status is automatic:

| ORM error | HTTP | Extra |
| --- | --- | --- |
| `NotFound` | 404 | |
| `UniqueViolation` | 409 | a JSON Pointer at the offending column, code `unique` |
| `ForeignKeyViolation` | 422 | a validation problem, code `foreign_key` |
| `NotNullViolation` | 422 | code `required` |
| `CheckViolation` | 422 | code `invalid` |
| `StaleWrite` | 409 | `retryable: true` |
| `Serialization`, `Deadlock` | 409 | `retryable: true` |
| `TenantMissing` | 500 | our bug, never the client's |
| `PoolTimeout` | 503 | `Retry-After: 1`, `retryable: true` |
| `StatementTimeout` | 504 | |
| `Connection` | 503 | |
| `Cursor` | 400 | |
| `Decode` and everything else | 500 | detail suppressed in production |

`Depends<RequestTx>` also documents the `409` and the `503` into that operation's OpenAPI entry
automatically. See [errors](./errors.md) and [OpenAPI](./openapi.md).

## The connection pool

`Db::connect(&config)` validates the configuration before opening anything, resolves the backend
from the URL scheme, probes for a transaction-mode pooler on PostgreSQL, builds the pool, opens
replica pools and starts a lag sampler.

```rust title="src/main.rs"
use moso::db::{DatabaseConfig, Db};
use std::time::Duration;

let config = DatabaseConfig::from_url(std::env::var("DATABASE_URL")?)
    .with_max_connections(8)
    .with_acquire_timeout(Duration::from_secs(10))
    .with_application_name("shop-api");

let db = Db::connect(&config).await?;
let app = App::new(cfg).provide(db.clone()).health_check("database", db.health_check());
```

`Db` is `Clone` and cheap: one `Arc`. Register it with `App::provide` and reach it from a handler
with `Inject<Db>`. Nothing wires it up for you.

A note on paths: `Db`, `DatabaseConfig`, `ReplicaConfig`, `TlsMode`, `PoolStats`, `Tx`, `TxOptions`
and `Isolation` are all reachable as `moso::db::*`. The operational types that are not re-exported at
that level (`AdvisoryKey`, `PgBouncerMode`, `DbMetrics`, `QuerySample`) live one module down in
`moso_orm::db`, which is where the snippets on this page import them from.

`DatabaseConfig` has no `Deserialize` implementation today, so reading a `[database]` section out of
your own `#[derive(Config)]` struct and mapping it onto these builders is application code. See
[configuration](./configuration.md).

### Configuration and defaults

| Field | Builder | Default | Why |
| --- | --- | --- | --- |
| `url` | `from_url` | required | scheme picks the backend: `postgres`, `postgresql` or `sqlite` |
| `max_connections` | `with_max_connections` | `cpus * 2`, clamped to 4..=20 | small on purpose, see sizing below |
| `min_connections` | `with_min_connections` | 1 (0 under `for_dev`) | |
| `acquire_timeout` | `with_acquire_timeout` | 10s | an exhausted pool must answer, not hang |
| `idle_timeout` | `with_idle_timeout` | 600s | |
| `max_lifetime` | `with_max_lifetime` | 1800s | every connection retires within it, so a pool pinned to a demoted primary drains without a restart |
| `test_before_acquire` | `with_test_before_acquire` | `false` | a round trip per acquire, for a flaky network |
| `statement_timeout` | `with_statement_timeout` | 30s | session level, sent in the startup packet |
| `lock_timeout` | `with_lock_timeout` | 5s | session level; also SQLite's `busy_timeout` |
| `application_name` | `with_application_name` | `"moso"` | `pg_stat_activity` can only name the process holding a lock if somebody filled the column in |
| `slow_query_ms` | `with_slow_query_ms` | 200 (50 under `for_dev`) | |
| `explain_slow` | `with_explain_slow` | `false` | `EXPLAIN` is a second round trip on a server that is already struggling |
| `n_plus_one_threshold` | `with_n_plus_one_threshold` | 20 (10 under `for_dev`) | see [relations](./relations.md) |
| `sticky_window` | `with_sticky_window` | 3s | read-your-writes, see below |
| `replicas` | `with_replica` | empty | |
| `tls` | `with_tls` | `Prefer` | |
| `tenancy` | `with_tenancy` | `Discriminator` | see [multi-tenancy](./multi-tenancy.md) |
| `pgbouncer` | `with_pgbouncer` | `Detect` | |
| `max_tenant_pools` | `with_max_tenant_pools` | 32 | |

`DatabaseConfig::for_dev()` flips the three development-friendly settings in one call.

### Sizing the pool

The default is small deliberately. The classic outage is thirty pods times a hundred connections
against a five-hundred-connection server. `DatabaseConfig::boot_summary()` prints the multiplication
you have to do:

```text
db: pool max=8 min=1 acquire_timeout=10s (×N instances must stay under your server's max_connections)
```

Multiply `max_connections` by every process that will run, add your migration runner and any
sidecar, and keep the total under the server's own limit with room to spare for a superuser
connection during an incident.

`DatabaseConfig::validate()` runs before anything opens, and every message names the offending field
with a `help:` line. It rejects an empty URL, an unsupported scheme, `max_connections == 0`,
`min_connections > max_connections`, a zero `acquire_timeout`, replicas on SQLite, and a replica with
weight 0. A `mysql://` URL is a configuration error naming the two supported schemes.

### When the pool runs out

```rust
let config = DatabaseConfig::from_url("sqlite://:memory:")
    .with_acquire_timeout(Duration::from_millis(150));
let db = Db::connect(&config).await.expect("open");

// The only connection is inside this transaction.
let held = db.begin().await.expect("begin");

let started = Instant::now();
let error = db.ping().await.expect_err("there is no second connection");
let waited = started.elapsed();

assert!(matches!(error, Error::PoolTimeout { .. }), "expected a pool timeout, got {error:?}");
assert!(waited < Duration::from_secs(2), "the acquire waited {waited:?}, which is a hang rather than a timeout");
let text = error.to_string();
assert!(text.contains("help:"), "{text}");
assert!(text.contains("max_connections"), "{text}");
```

`acquire_timeout` exists so that this is a `503` with a `Retry-After` and not a request that never
answers.

`Db::stats()` returns a `PoolStats` with `size`, `idle`, `in_flight`, `waiting` and `max`.
`is_saturated()` is `idle == 0 && waiting > 0`, because full is not saturated until somebody waits,
and `utilisation()` is `in_flight / max`.

### Connection poolers and TLS

On PostgreSQL, `Db::connect` probes for a transaction-mode pooler by asking `pg_backend_pid()` inside
four separate transactions. A direct connection cannot change its backend process id, so a change
**proves** something is reassigning the server connection. When it does, the prepared-statement
cache is turned off and a `warn` says why.

The probe has zero false positives and some false negatives: an idle pooler often reuses the same
server connection, so a quiet deployment may not be detected. `PgBouncerMode` covers that:

| Mode | Behaviour |
| --- | --- |
| `Detect` | the default; probe and act on the result |
| `Assume` | skip the probe, disable the statement cache |
| `Never` | skip the probe, keep the statement cache |

A `?pgbouncer=true` marker in the URL is honoured unconditionally.

`TlsMode` is `Disable`, `Prefer` (the default), `Require` or `VerifyFull`. A URL that already names
`sslmode` always wins over `database.tls`. With the `tls` cargo feature off, asking for `require` or
`verify-full` is a configuration error rather than a connection that quietly goes plaintext.

### Health and metrics

`Db::health_check()` returns a `DatabaseCheck`, critical by default, `non_critical()` to degrade
instead of failing readiness. Register it with `App::health_check("database", db.health_check())`.
See [health and shutdown](./health-and-shutdown.md).

Metrics go to your own recorder, so Moso depends on no metrics facade:

```rust
use moso_orm::db::{DbMetrics, QuerySample};

/// Prints every statement that took longer than a tenth of a second.
pub struct Slow;

impl DbMetrics for Slow {
    fn query(&self, sample: &QuerySample<'_>) {
        if sample.elapsed.as_millis() > 100 {
            eprintln!("{} took {:?}", sample.operation, sample.elapsed);
        }
    }
}
```

Attach it with `let db = db.with_metrics(Arc::new(Slow));`. Every method on `DbMetrics` has a
do-nothing default, so a recorder that only wants query durations implements one. The four metric
names are constants: `moso_db_query_duration_seconds`, `moso_db_pool_connections`,
`moso_db_pool_acquire_seconds` and `moso_db_transaction_retries_total`. The recorder runs on the
caller's task, so it must not block. See [observability](./observability.md).

For anything Moso does not wrap, `Db::postgres_pool()` and `Db::sqlite_pool()` return sqlx's own
pool. If you write through one, call `Db::mark_write()` so read-your-writes still holds.

## Read replicas

Replicas are configured per URL, with a weight and a lag tolerance:

```rust
let config = DatabaseConfig::from_url(primary)
    .with_replica(ReplicaConfig::from_url(replica_a).with_weight(2))
    .with_replica(ReplicaConfig::from_url(replica_b).with_max_lag(Duration::from_secs(2)));
```

`Db::read()` picks a replica by weighted round-robin, skipping any whose measured lag exceeds its
`max_lag`, and falling back to the primary when none is healthy. Lag is sampled every five seconds
with `pg_last_xact_replay_timestamp()`. A `NULL` reading means "no measurable lag", not "infinitely
behind": treating an idle replica as unhealthy would take a perfectly good server out of rotation
for being quiet. A lagging replica makes the health report `Degraded` rather than `Down`, because
taking the instance out of rotation for a replication hiccup turns a slow replica into an outage.

### Read-your-writes is on by default

```rust
assert!(
    !core::ptr::eq(db.read(), &db),
    "with a healthy replica and no recent write, a read goes to the replica"
);

// Run a read here. It does not move the window.
assert!(!core::ptr::eq(db.read(), &db), "a `select` is not a write");

db.mark_write();
assert!(
    core::ptr::eq(db.read(), &db),
    "acceptance 5: after a write, a read in the same window hits the primary"
);
assert!(
    !core::ptr::eq(db.read_stale(), &db),
    "`read_stale` is the opt-out for a read that tolerates staleness"
);
```

Any successful statement whose first word is not `select`, `with` or a control keyword marks the
write clock. For `database.sticky_window` afterwards, `db.read()` returns the primary. A replica two
hundred milliseconds behind produces a bug class that only ever reproduces in production, so this is
on unless you opt out.

| Method | Where the read goes |
| --- | --- |
| `db.read()` | a replica, unless the sticky window says primary |
| `db.read_stale()` | a replica, ignoring the sticky window |
| `db.primary()` | the primary, always |
| `db.request_scoped()` | a clone with its **own** sticky window and statement counter |

`request_scoped()` matters on a write-heavy service. The window is shared between clones by default,
which is always correct (it can only send a read somewhere fresher) but costs the replicas most of
their traffic. Narrowing it to one request gives them back.

## Streaming a large result set

`Select::fetch_stream` and `Handle::fetch_stream` return a stream instead of a `Vec`.

There are two limits worth knowing before you rely on it. Preloads are **not** applied to a stream:
batching needs the whole parent set, and pretending otherwise would reintroduce the N+1. And inside
a transaction the stream buffers everything into memory, because a stream borrowing both the
transaction's lock and its connection would be a self-referential type and this crate forbids
`unsafe`. For a large result set inside a transaction, paginate.

## SQLite notes

SQLite is fully supported and needs no running service, which is what keeps the test suite green on
a machine with no Docker. Two behaviours to know:

- Connect options set `create_if_missing(true)` and, importantly, `foreign_keys(true)`, which SQLite
  does not do by default and every application assumes.
- An in-memory pool is pinned to **one** connection, because every connection to `:memory:` is a
  different database. `PoolStats::max` reports the forced number, not the configured one. A test
  whose fixture must survive across statements should use an on-disk file.

Advisory locks, read replicas, schema-per-tenant, isolation above SQLite's own, per-transaction
`SET LOCAL` timeouts and the one-statement `sync` all need PostgreSQL. The test PostgreSQL server
starts with `docker compose -f compose.test.yaml up -d` and is reached through `DATABASE_URL`.

## Failure modes

| What happened | What you get |
| --- | --- |
| The pool is empty for longer than `acquire_timeout` | `Error::PoolTimeout` with `help:` naming `max_connections`, mapped to `503` with `Retry-After` |
| A statement runs past `statement_timeout` | `Error::StatementTimeout`, mapped to `504` |
| A serialisable transaction lost a race | retried up to `max_retries`, then `Error::Serialization`, mapped to `409` with `retryable: true` |
| A deadlock | the same, `40P01` |
| A write inside a read-only transaction | refused before it is sent, except raw SQL on SQLite |
| A panic inside a transaction | rollback, connection returned unpoisoned |
| A `Tx` dropped without commit or rollback | rollback |
| A savepoint handle asked to commit the transaction | an error naming the fix |
| `Depends<RequestTx>` on a route without the layer | `500` whose message names both installation spellings |
| A commit fails after a 2xx handler | the 2xx is replaced by the real error, never answered as success |
| A configuration Moso cannot honour | a boot error naming the field, before anything opens |

## See also

- [Relations](./relations.md) for preloading and the statement counter these examples use.
- [Migrations](./migrations.md) for schema changes.
- [Multi-tenancy](./multi-tenancy.md) for per-tenant pools and the tenant obligation.
- [Errors](./errors.md) for the problem documents the mapping table produces.
- [Background jobs](./jobs.md) for `tx.enqueue` and `SKIP LOCKED` workers.
- [Testing](./testing.md) for `TestDb` and `assert_queries!`.
- [Observability](./observability.md) for wiring `DbMetrics` into an exporter.
