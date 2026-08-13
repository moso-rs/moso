# 32 - Background Jobs & Scheduling

> ⛔ **NOT IMPLEMENTED.** This document is design intent only. No crate in the workspace provides
> any of it, nothing references it, and nothing is stubbed. See
> [`06-reference/63-implementation-status.md`](../06-reference/63-implementation-status.md).

## Position

The ecosystem has good pieces (`apalis` is Tower-based, multi-backend, and mature enough) but they
are un-wired: you assemble the backend, the worker, the retry policy, the serialisation, and the
observability yourself. Moso ships an opinionated layer with a **Moso-owned `Job` trait** so the
backend can change without breaking user code, and with the two features that matter most in
practice: **transactional enqueue** and **a real dashboard**.

## Defining a job

```rust
// example - src/jobs/welcome.rs
use moso::jobs::prelude::*;

#[job(
    queue = "mail",
    retries = 5,
    backoff = "exponential(30s, max = 1h)",
    timeout = "2m",
    unique_for = "10m",              // dedupe identical payloads within a window
)]
pub async fn send_welcome_email(
    args: SendWelcome,               // the payload: any Serialize + DeserializeOwned
    Inject(db): Inject<Db>,          // same DI as handlers
    Inject(mail): Inject<dyn Mailer>,
    ctx: JobCtx,                     // attempt number, job id, cancellation, heartbeat
) -> Result<()> {
    let user = User::find(args.user_id).fetch_one(&db).await?;
    mail.send(WelcomeEmail { user: &user }).await?;
    Ok(())
}

#[derive(Serialize, Deserialize)]
pub struct SendWelcome { pub user_id: Id<User> }
```

The macro generates a `SendWelcomeEmailJob` type implementing `Job`, registers a deserialiser under
a stable name, and produces a typed enqueue API. Payload types are versioned: adding a field with a
serde default is safe; the docs cover the deploy-ordering rules for payload changes, because "we
deployed and 40k queued jobs failed to deserialise" is a real incident class.

```rust
// spec
pub trait Job: Send + Sync + 'static {
    type Args: Serialize + DeserializeOwned + Send + Sync;
    const NAME: &'static str;                // stable wire name, not the Rust path
    const QUEUE: &'static str;
    const RETRIES: u32;
    const TIMEOUT: Duration;
    fn backoff(attempt: u32) -> Duration;
    fn run(args: Self::Args, ctx: JobCtx) -> impl Future<Output = Result<()>> + Send;
}
```

## Enqueuing

```rust
// example
// fire and forget
SendWelcomeEmail::enqueue(&jobs, SendWelcome { user_id }).await?;

// with options
SendWelcomeEmail::enqueue(&jobs, args)
    .delay(Duration::from_secs(60))
    .priority(Priority::High)
    .queue("mail-urgent")
    .unique_key(format!("welcome:{user_id}"))
    .spawn().await?;

// TRANSACTIONAL - the important one
db.transaction(|tx| async move {
    let user = User::insert(new).fetch_one(tx).await?;
    tx.enqueue(SendWelcomeEmail, SendWelcome { user_id: user.id }).await?;
    Ok(user)
}).await?;
```

**Transactional enqueue is the headline feature.** With the Postgres backend the job row is written
inside the same transaction, so it is impossible to send a welcome email for a user whose creation
rolled back - the single most common bug in Rails/Celery/Sidekiq applications. With the Redis
backend, Moso uses the transactional outbox pattern automatically (write to an outbox table in the
transaction, a relay moves it to Redis), and the docs are explicit that this is what is happening
and what it costs.

## Workers

```
$ moso worker --queues default,mail --concurrency 8
$ shop worker --queues mail --concurrency 32          # the app binary, same code path
```

```rust
// example - src/jobs/mod.rs
pub fn registry() -> JobRegistry {
    JobRegistry::new()
        .register::<SendWelcomeEmail>()
        .register::<GenerateInvoice>()
        .register::<ReindexSearch>()
        .schedule(Cron::new("0 3 * * *", NightlyCleanup, ()))          // 03:00 daily
        .schedule(Every::new(Duration::from_secs(300), PollFeeds, ()))
}
```

Registration is explicit (no link-time magic), so `App::build()` can verify that every job type
enqueued anywhere in the codebase is registered - a **boot error** otherwise:

```
✗ job `ReindexSearch` is enqueued but not registered
    enqueued at  src/services/search.rs:42
    fix          add `.register::<ReindexSearch>()` in src/jobs/mod.rs
```

Worker behaviour:
- Concurrency is per-worker-process and per-queue-weighted; a slow queue cannot starve a fast one.
- Graceful shutdown: stop fetching, finish in-flight up to a grace period, then re-queue what is
  left (at-least-once, and the docs state that plainly).
- Heartbeats: long jobs call `ctx.heartbeat()` to extend the lease; a worker that dies has its jobs
  reclaimed after the lease expires rather than after a fixed timeout.
- Cancellation: `ctx.cancelled()` is a future; the framework cancels on shutdown and on an operator
  cancel from the dashboard.
- Backpressure: a queue depth threshold can pause enqueueing of low-priority jobs, exposed as a
  metric and an alertable condition.

## Reliability semantics (stated honestly)

| Property | Guarantee |
| --- | --- |
| Delivery | **At-least-once.** Jobs must be idempotent. The docs lead with this, with a worked idempotency-key example. |
| Ordering | Not guaranteed across jobs. `serial = true` gives one running instance of a job *type* at a time, fleet-wide; a `unique_key` chain gives the same per payload. |
| Exactly-once side effects | Achieved by the *user* with an idempotency key + a unique constraint. Moso provides `ctx.once(key, || …)` as a helper backed by a table. |
| Durability | Postgres backend: full. Redis backend: subject to Redis persistence config, and the boot log says so if AOF is off. |
| Retry | Exponential with jitter; `Error::retryable()` decides. Non-retryable errors go straight to the DLQ. |

## Failure handling

- Failures are recorded with the error chain, attempt number, and worker identity.
- After `RETRIES`, the job moves to the **dead-letter queue**, retaining the payload so it can be
  retried after a fix.
- `moso jobs retry --dlq --job SendWelcomeEmail --since 1h` re-enqueues in bulk.
- An `on_failure` hook per job (alerting, compensating action) and a global one.
- A poison-payload guard: a job that fails deserialisation goes straight to the DLQ rather than
  retrying five times.

## Scheduling

```rust
// spec
JobRegistry::schedule(Cron::new("0 3 * * *", NightlyCleanup, ()))
    .timezone("Europe/Rome")
    .catch_up(false)              // don't run missed occurrences after downtime
    .overlap(Overlap::Skip)       // Skip | Queue | Allow
```

The scheduler elects a leader via a database advisory lock, so running 20 web pods does not run the
nightly job 20 times. This is the second most common jobs bug and it is solved by default, not by a
config option the user must discover.

## Observability

Every job execution creates a tracing span linked to the enqueueing request's trace (the trace
context is stored in the job row), so a distributed trace spans `HTTP request → job → outbound
email`. This is a genuinely rare capability and it makes debugging async workflows tractable.

Metrics: `moso_jobs_enqueued_total{job,queue}`, `moso_jobs_duration_seconds{job,status}`,
`moso_jobs_queue_depth{queue}`, `moso_jobs_latency_seconds{queue}` (enqueue→start),
`moso_jobs_retries_total{job,reason}`, `moso_jobs_dlq_total{job}`.

## Dashboard

Part of `moso-admin` (`33-admin.md`), gated on `Perm::AdminAccess`:
queues with depth and latency, running jobs with elapsed time and a cancel button, recent failures
with full error chains, the DLQ with bulk retry/discard, the schedule with next/last run, and a
per-job throughput chart. Also available standalone at `/_jobs` without the full admin.

## Backends

| Backend | Feature | Notes |
| --- | --- | --- |
| Postgres | `jobs-pg` (default) | `SELECT … FOR UPDATE SKIP LOCKED`, `LISTEN/NOTIFY` for low latency, transactional enqueue, partitioned by state for a bounded hot table. Handles low thousands/s - enough for the overwhelming majority. |
| Redis | `jobs-redis` | Higher throughput, outbox-based transactional enqueue, requires persistence for durability. |
| In-memory | `jobs-memory` | Tests and `moso dev`. Same semantics, no external service. |

`apalis` is used as the execution substrate where it fits; the `Job` trait is ours so that is an
implementation detail. **TODO(agent):** benchmark the Postgres backend at 1k/s sustained before
committing to build vs. wrap; if `apalis`'s Postgres storage meets the semantics above, wrap it.

## Testing jobs

```rust
// example
let app = TestApp::spawn().await?;
app.post("/users").json(&new_user).send().await?.assert_status(201);

app.jobs().assert_enqueued::<SendWelcomeEmail>(1);
app.jobs().drain().await?;                  // run everything inline, deterministically
app.mail().assert_sent_to("new@example.com");
```

`drain()` runs jobs inline in the test process with real DI, so job code is covered by integration
tests without a worker process. Time is controllable (`app.advance_time(1.hour())`) so scheduled and
delayed jobs are testable without sleeping.

## Acceptance criteria (WP-19)

1. Transactional enqueue: a rolled-back transaction leaves no job (test with a forced rollback).
2. Redis backend outbox relays within 100 ms p99 and never loses a job across a relay restart.
3. Enqueuing an unregistered job is a boot error naming the enqueue site.
4. A worker killed mid-job has the job reclaimed and retried after the lease, exactly once.
5. Scheduler leader election: 10 processes run a cron job exactly once per occurrence.
6. Retry backoff matches the declared policy within jitter bounds; non-retryable errors skip to DLQ.
7. Trace context propagates from request → job → outbound call (asserted on span parentage).
8. `drain()` executes jobs with the same DI graph as a real worker.
9. Postgres backend sustains 1000 jobs/s enqueue+execute on the reference machine with a bounded
   table size.
