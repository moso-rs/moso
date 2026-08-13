# 24 - Transactions, Pooling, Replicas & Multi-Tenancy

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## `Db` - the handle

```rust
// spec - moso-orm
#[derive(Clone)]
pub struct Db { /* Arc<Inner> */ }

impl Db {
    pub async fn connect(cfg: &DatabaseConfig) -> Result<Self>;

    /// Run a closure in a transaction, with automatic retry on serialization failures.
    pub async fn transaction<F, T>(&self, f: F) -> Result<T>
    where F: for<'t> AsyncFnOnce(&'t Tx) -> Result<T> + Send;

    pub async fn transaction_with<F, T>(&self, opts: TxOptions, f: F) -> Result<T>;

    /// Explicit handle, for when a closure does not fit.
    pub async fn begin(&self) -> Result<Tx>;

    /// The read-replica handle. Falls back to primary if no replica is configured.
    pub fn read(&self) -> &Db;
    /// Force the primary (after a write, when you need read-your-writes).
    pub fn primary(&self) -> &Db;

    pub fn pool(&self) -> &sqlx::PgPool;      // full escape hatch
    pub fn stats(&self) -> PoolStats;
    pub async fn health(&self) -> HealthStatus;
}
```

`Db` is `Clone` and cheap; it is registered as a provider and reached with `Inject<Db>`.

## Executors: writing code that works in and out of a transaction

Every query method takes `impl Executor`, implemented by `&Db`, `&Tx`, and `&mut Tx`. So a service
function is written once:

```rust
// example
pub async fn credit(ex: impl Executor<'_>, id: Id<Account>, amount: Money) -> Result<Account> {
    Account::find(id).lock(LockMode::ForUpdate).fetch_one(ex).await?;
    /* … */
}

// callable both ways
credit(&db, id, amount).await?;
db.transaction(|tx| async move { credit(tx, id, amount).await }).await?;
```

This is the single most important ergonomic detail in a transaction API and several Rust ORMs get
it wrong by requiring different call shapes.

## `transaction` - semantics

```rust
// example
let order = db.transaction(|tx| async move {
    let order = Order::insert(new).fetch_one(tx).await?;
    Inventory::reserve(tx, &order).await?;
    tx.enqueue(SendReceiptJob { order_id: order.id }).await?;   // commits with the tx
    Ok(order)
}).await?;
```

- Commits on `Ok`, rolls back on `Err` **or panic**.
- **Retries** on serialization failure (`40001`) and deadlock (`40P01`), up to
  `TxOptions::max_retries` (default 3) with jittered exponential backoff. This is why the API is a
  closure: a retry must be able to re-run the body. Non-retryable errors return immediately.
- The closure must be idempotent-safe; the docs are explicit that side effects outside the database
  (HTTP calls, sending mail) must not be inside a retried transaction, and `moso check` warns when
  it sees `reqwest`/`Mailer::send` inside a `transaction` closure.
- Savepoints: `tx.savepoint(|sp| async move { … }).await` nests, mapping to `SAVEPOINT`/`ROLLBACK TO`.

```rust
// spec
pub struct TxOptions {
    pub isolation: Isolation,        // ReadCommitted (default) | RepeatableRead | Serializable
    pub read_only: bool,
    pub deferrable: bool,
    pub max_retries: u32,            // default 3
    pub statement_timeout: Option<Duration>,
    pub lock_timeout: Option<Duration>,
}
```

## Request-scoped transactions

`Depends<RequestTx>` (see `01-http/15`) is the handler-level form: lazily opened, committed after a
2xx, rolled back otherwise. Retry is **disabled** for request transactions, because the request body
may have been consumed - instead a serialization failure becomes a 409 with `retryable: true`, and
the docs point at `db.transaction` for retryable work.

## Connection pooling

```rust
// spec - DatabaseConfig
pub struct DatabaseConfig {
    pub url: SecretString,
    pub max_connections: u32,          // default: max(4, cpus * 2), capped at 20
    pub min_connections: u32,          // default 1 (0 in dev to avoid holding connections)
    pub acquire_timeout: Duration,     // default 10s → 503, not a hang
    pub idle_timeout: Duration,        // default 10m
    pub max_lifetime: Duration,        // default 30m - survives DB failovers cleanly
    pub test_before_acquire: bool,     // default false (costs a round trip)
    pub statement_timeout: Duration,   // default 30s, set per session
    pub lock_timeout: Duration,        // default 5s
    pub application_name: String,      // default: the app name - shows up in pg_stat_activity
    pub slow_query_ms: u64,            // default 200
    pub replicas: Vec<ReplicaConfig>,
    pub tls: TlsConfig,
}
```

Notes that matter operationally:
- **`max_connections` default is small on purpose.** The classic outage is 30 pods × 100 connections
  against a 500-connection Postgres. The boot log prints
  `db: pool max=8 (×N instances must stay under your server's max_connections)`.
- **`application_name` is set** so `pg_stat_activity` tells you which service is holding a lock.
- **Acquire timeout returns 503**, not a hang - with a `Retry-After` and a metric.
- PgBouncer in transaction mode: detected via a `SHOW transaction_read_only`-style probe at boot;
  when detected, Moso disables prepared-statement caching and says so in the log, because that
  mismatch is a top-tier production footgun.
- Pool metrics (`size`, `idle`, `waiting`, `acquire_wait_seconds`) are exported automatically.

## Read replicas

```toml
[[database.replicas]]
url = "postgres://…/shop?target_session_attrs=read-only"
weight = 1
max_lag = "5s"
```

```rust
// example
let posts = Post::query().fetch_all(db.read()).await?;      // replica
let post  = Post::insert(new).fetch_one(&db).await?;        // primary
let fresh = Post::find(post.id).fetch_one(db.primary()).await?;  // read-your-writes
```

- `db.read()` picks a replica by weighted round-robin, skipping any whose measured lag exceeds
  `max_lag` (sampled every 5 s via `pg_last_xact_replay_timestamp`).
- **Read-your-writes:** after any write on a `Db` handle within a request, subsequent `db.read()`
  calls *in that request* are routed to the primary for `database.sticky_window` (default 3 s).
  This is on by default because the alternative is a subtle, intermittent bug class; it is
  disableable per query with `db.read_stale()`.
- Transactions always use the primary unless `TxOptions::read_only` is set.

## Multi-tenancy

Three supported models, documented with the trade-offs, because this is a question every SaaS asks
and no Rust framework answers.

### 1. Discriminator column (default recommendation)
```rust
#[derive(Entity)]
#[entity(tenant = "tenant_id")]
pub struct Invoice { … }
```
Every query for a tenant-scoped entity **requires** a tenant scope; forgetting it is a compile
error:
```
error: `Invoice` is tenant-scoped and this query has no tenant
  = help: use `Invoice::query().scoped(tenant)` or `db.for_tenant(tenant)`
  = help: to query across tenants deliberately: `Invoice::query().across_tenants()`
```
Implemented with a marker type parameter on `Select` that `scoped()` discharges. This is the one
place where compile-time enforcement is worth the type complexity, because the failure mode is a
cross-tenant data leak.

### 2. Schema per tenant
`db.for_tenant(t)` sets `search_path` on a dedicated connection. Migrations run per schema, with
`moso db migrate --all-tenants` and progress reporting. Suits tens-to-hundreds of tenants.

### 3. Database per tenant
A `TenantRouter` provider maps tenant → `Db`. Pools are created lazily with an LRU cap. Suits
few, large tenants with isolation requirements.

Postgres RLS is supported as a *defence in depth* layer on top of model 1 (`#[entity(rls)]` emits
the policy), not as the primary mechanism - RLS with a pooled connection requires careful
`SET LOCAL` discipline that Moso implements and documents.

## Observability of the data layer

Every statement produces a tracing span with: SQL (parameterised, never with values in production),
duration, rows, whether it was in a transaction, and the call site. Metrics:

- `moso_db_query_duration_seconds{operation, entity}` - histogram
- `moso_db_pool_connections{state}` - gauge
- `moso_db_pool_acquire_seconds` - histogram
- `moso_db_transaction_retries_total{reason}` - counter
- `moso_db_statements_per_request` - histogram (the N+1 alarm)

## Acceptance criteria (WP-14a)

1. `impl Executor` works uniformly for `&Db`, `&Tx`, `&mut Tx` across every query method.
2. A serialization failure inside `db.transaction` retries and eventually succeeds; a
   unique-violation does not retry (tested with two concurrent tasks).
3. Panic inside a transaction rolls back and does not poison the pool.
4. Acquire timeout produces 503 with `Retry-After`, not a hang; pool metric reflects waiting.
5. Read-your-writes: a write followed by `db.read()` in the same request hits the primary.
6. Replica lag above `max_lag` removes the replica from rotation within 10 s and logs once.
7. A query on a `#[entity(tenant)]` entity without a scope is a compile error (UI test).
8. PgBouncer transaction mode is detected and prepared-statement caching disabled.
