---
title: Background jobs
description: Define jobs with the job attribute, enqueue them inside the transaction that caused them, run workers with retries and a dead letter queue, and watch the queue from metrics and a dashboard.
order: 29
status: shipped
---

`moso-jobs` is the background work battery: a framework-owned `Job` trait, an enqueue that can join
the transaction that caused it, a worker with leases and graceful shutdown, retries with jitter, a
dead letter queue you can page through and bulk retry, and Prometheus metrics plus a JSON dashboard
for the operator. The queue lives behind a `Queue` trait, so the same job code runs against
PostgreSQL, SQLite, Redis or an in-process map.

The headline is transactional enqueue. `tx.enqueue(SendWelcomeEmail, args)` writes the job row inside
your own transaction, so a rollback leaves no job. That is the bug this crate exists to kill: a
welcome email sent for a user whose creation never committed.

> [!IMPORTANT]
> **A worker is your own binary, by design, and there is no `moso worker` command.** A worker is a
> long-lived process that loads your job bodies, holds leases, and is deployed, scaled and drained
> alongside the web process, so it lives next to the `App` it shares a registry with. You write the
> ten lines under [running a worker](#running-a-worker).
>
> **`moso jobs` does exist**, for the operator's half: `list`, `status`, `schedules`, `dlq`, `retry`
> and `discard`. It needs six lines in your `src/dump.rs`. See [the CLI](#the-cli) below.
>
> `TestApp` now exposes a `jobs()` accessor behind the `moso-test` feature; the lower-level
> `MemoryQueue` plus `Jobs::drain()` recipe still works and is what the DI-graph examples below use.
> `App::build()` cannot see the registry (`moso-core` does not depend on `moso-jobs`), but you no
> longer have to remember to check it yourself: `Worker::run` validates and refuses to start.

## Turning it on

The `#[job]` macro expands to paths inside the `moso` facade, so it needs the facade's `jobs`
feature, which implies `orm`, which pulls a database driver.

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1", features = ["jobs"] }
```

The backends are features on `moso-jobs` itself and the facade does not forward them. Add the crate
directly when you want Redis or the outbox.

| Feature | Default | Adds | Needs |
| --- | --- | --- | --- |
| `jobs-pg` | yes | `backend::PgQueue`, a `sqlx` dependency for the listener | a `moso::db::Db` on PostgreSQL or SQLite |
| `jobs-memory` | yes | `backend::MemoryQueue` | nothing |
| `jobs-redis` | no | `backend::RedisQueue`, enables `moso-kv/redis` | a running Redis |
| `jobs-redis` and `jobs-pg` together | no | `backend::Outbox` | Redis and a database |

```toml title="Cargo.toml"
[dependencies]
moso = { version = "0.1", features = ["jobs"] }
moso-jobs = { version = "0.1", features = ["jobs-redis"] }
```

`sqlx` appears only under `jobs-pg`, and only as the `LISTEN`/`NOTIFY` listener. Every query goes
through `moso-orm`.

## Defining a job

Put `#[job]` on an `async fn`. The function is emitted unchanged, so it stays directly callable from
a test, and a unit struct is generated beside it carrying the attribute's values as associated
constants.

```rust title="src/jobs/mail.rs"
use moso::jobs::prelude::*;
use moso::prelude::Inject;
use moso::db::Db;

#[job(
    queue = "mail",
    retries = 5,
    backoff = "exponential(30s, max = 1h)",
    timeout = "2m",
    unique_for = "10m",
)]
pub async fn send_welcome_email(
    args: SendWelcome,
    Inject(db): Inject<Db>,
    Inject(mail): Inject<dyn Mailer>,
    ctx: JobCtx,
) -> Result {
    let user = User::find(args.user_id).fetch_one(&db).await?;
    mail.send(WelcomeEmail { user: &user }).await?;
    Ok(())
}
```

That generates `pub struct SendWelcomeEmail;` with `NAME = "send_welcome_email"`, `QUEUE = "mail"`,
`RETRIES = 5`, a two minute timeout, a ten minute deduplication window and an exponential ladder.

> [!NOTE]
> `moso::jobs::prelude` does not export `Inject` or `Db`. Import `Inject` from `moso::prelude` and
> `Db` from `moso::db`. Do not glob-import both preludes: they each export `Error` and `Result`, and
> the first time you write `Result` the compiler reports an ambiguity.

### Attribute keys

| Key | Value | Effect |
| --- | --- | --- |
| `name = "..."` | string | the wire name. Defaults to the function name |
| `type_name = "..."` | string | the generated struct's name. Defaults to the function name in PascalCase |
| `queue = "..."` | string | `Job::QUEUE`. Defaults to `"default"` |
| `retries = 5` | integer | `Job::RETRIES`. Defaults to 25 |
| `backoff = "..."` | string | `immediate`, `fixed(30s)`, `linear(30s, max = 1h)`, `exponential(30s, max = 1h)` |
| `timeout = "2m"` | humantime | how long one attempt may take. Defaults to 300 seconds |
| `unique_for = "10m"` | humantime | deduplicate identical payloads for this long. Defaults to off |
| `priority = "high"` | string | one of `low`, `normal`, `high`, `critical` |
| `serial` | bare flag | sets `Job::SERIAL`: never two instances of this job at once, fleet-wide |

Durations and the backoff spec are parsed at macro expansion time, so `timeout = "2 minuts"` is a
compile error on the attribute rather than a surprise at the first retry. An unknown key gets a "did
you mean" and a `help:` line listing all nine.

### Signature rules

The payload comes first, then any number of `Inject(..)` parameters, then an optional `ctx: JobCtx`.
There is no request here, so there is nothing to extract: `Json`, `Query`, `Path` and `Depends` are
rejected with a message telling you the value belongs in the payload, which is the only thing the
queue row carries. `Inject(dyn Trait)` works exactly as it does in a handler and resolves against the
same [dependency graph](./dependency-injection.md).

The generated struct takes the function's visibility, widened to `pub(crate)` when the function is
private, because a job is almost always registered from a different module than the one that defines
it.

### Writing the trait by hand

`#[job]` is a convenience. The trait is public and there is nothing magic in the expansion.

```rust title="src/jobs/welcome.rs"
use moso_jobs::{Job, JobCtx, Result};
use serde::{Deserialize, Serialize};

/// Which account to greet.
#[derive(Serialize, Deserialize)]
pub struct SendWelcome {
    /// The new account.
    pub user_id: u64,
}

/// Greets a new account.
#[derive(Clone, Copy, Debug, Default)]
pub struct SendWelcomeEmail;

impl Job for SendWelcomeEmail {
    type Args = SendWelcome;
    const NAME: &'static str = "send_welcome_email";

    async fn run(args: SendWelcome, _ctx: JobCtx) -> Result {
        let _ = args.user_id;
        Ok(())
    }
}
```

`Job` also has an `on_failure(args, error, ctx)` hook. It runs after **every** failed attempt, before
the retry is scheduled, with the decoded payload in hand, and it cannot itself fail (a failing
failure hook is a loop), so it returns nothing and logs its own problems. Gate it on
`ctx.is_last_attempt()` when you only want to alert once. A global hook across all jobs does not
exist.

### The wire name is not the Rust path

`Job::NAME` is what goes in the row. It is deliberately decoupled from the module path so that moving
`send_welcome_email` into another file does not orphan the rows already queued under the old name.
When you move a function, pin the name with `#[job(name = "send_welcome_email")]`.

The same rule applies to the payload. Adding a field with a serde default is safe for rows already in
the queue. Renaming or removing one is not: the old rows fail to deserialise and go straight to the
dead letter queue.

## Registering jobs and schedules

There is no link-time registry. You list every job once, which is what makes the set printable and
lets two applications live in one process. See [routing](./routing.md) for the same argument
applied to routes.

```rust title="src/jobs/mod.rs"
fn registry() -> moso_jobs::JobRegistry {
    moso_jobs::JobRegistry::new()
        .register::<SendWelcomeEmail>()
        .register::<NightlyCleanup>()
        .schedule(
            moso_jobs::Cron::new::<NightlyCleanup>("0 3 * * *", ())
                .timezone("Europe/Rome")
                .catch_up(false)
                .overlap(moso_jobs::Overlap::Skip),
        )
        .schedule(moso_jobs::Every::new::<NightlyCleanup>(
            Duration::from_secs(300),
            (),
        ))
}
```

`JobRegistry::validate()` returns `moso_core::error::BootErrors` and reports, in one pass: duplicate
wire names (naming both Rust types), a `backoff()` override that disagrees with `BACKOFF`, a retry
budget of zero, a schedule whose expression or timezone does not parse, a schedule naming an
unregistered job with a "did you mean", and two schedules sharing a key.

`BootErrors` implements `std::error::Error` and renders a grouped report, so a boot function can end
the process on it:

```rust
// in `fn main() -> Result<(), Box<dyn std::error::Error>>`
registry.validate().into_result()?;
```

`Worker::validate()` is the same report plus the two questions only a worker can ask: whether the
backend can keep the promises the registry makes (a `serial` job on a backend whose
`QueueCapabilities::serial_chains` is false), and whether this worker listens to a queue no
registered job uses.

> [!NOTE]
> `Worker::run` and `Worker::drain_inline` call `Worker::validate()` and **refuse to start** on a
> non-empty report, so a misconfigured registry cannot reach production through a worker binary. It
> is one `Error::Config` carrying the whole grouped report, not the first problem: an operator who
> has three of these wants three, not three restarts. `moso-core` still does not depend on
> `moso-jobs`, so `App::build()` cannot see the registry. Call `registry.validate()` in the
> composition root too if the process only enqueues and never runs a worker.

## Wiring the composition root

The short version, when you do not need the dead letter views:

```rust title="src/jobs/mod.rs"
use std::sync::Arc;

use moso::db::Db;
use moso::jobs::{Jobs, JobsConfig};

pub fn build(db: Db, resolver: moso::Resolver) -> moso::jobs::Result<Jobs> {
    let registry = Arc::new(registry());
    let config = JobsConfig::from_env()?;
    config.validate_against(&registry)?;

    let queue = config.build(Some(db))?;
    let jobs = Jobs::new(queue, registry).with_resolver(resolver);
    jobs.install();
    Ok(jobs)
}
```

`JobsConfig::build` hands back an `Arc<dyn Queue>`, and a trait object cannot be asked whether it is
also a `DeadLetterQueue`. So when you want `/_jobs/dead` or `DlqFilter` to work, build the concrete
backend yourself and say so:

```rust title="src/jobs/mod.rs"
use moso_jobs::{DeadLetterQueue, Queue, backend::PgQueue};

let queue = Arc::new(PgQueue::new(db.clone()));
let jobs = Jobs::new(
    Arc::clone(&queue) as Arc<dyn Queue>,
    Arc::clone(&registry),
)
.with_dead_letters(Arc::clone(&queue) as Arc<dyn DeadLetterQueue>)
.with_resolver(resolver);
```

Without `with_dead_letters`, every dead letter operation fails by name rather than returning an empty
list that would read as "nothing has failed".

`App::resolver()` gives you the graph the handlers use, and `App::shutdown_signal()` gives you the
signal the HTTP server drains on. Provide the `Jobs` handle back into the application so handlers can
take `Inject<Jobs>`.

### Configuration from the environment

`JobsConfig::from_env()` reads exactly six variables.

| Variable | Meaning |
| --- | --- |
| `JOBS_BACKEND` | `postgres`, `postgresql`, `pg`, `redis`, `memory`, `in-memory` or `inmemory` |
| `JOBS_URL` | the backend URL. Only Redis needs one |
| `JOBS_CONCURRENCY` | a whole number |
| `JOBS_QUEUES` | comma separated queue names |
| `JOBS_SCHEDULER` | truthy is `1`, `true`, `yes` or `on`. Anything else is false |
| `JOBS_DASHBOARD` | same truthiness rule |

`validate()` refuses a backend whose cargo feature is off, Redis with no URL, a concurrency of zero,
a grace longer than the lease and a lease of zero. `validate_against(&registry)` adds a `queues` list
naming a queue no registered job uses, because a worker listening to a queue nothing writes to looks
exactly like a broken one.

`JobsConfig` has public fields for `backpressure` and `drain` with no builder methods. Assign them
after `new(..)`.

## Enqueuing

The plain form takes the `Jobs` handle. It is a builder that is also a future, so you can await it
directly or configure it first.

```rust
async fn enqueue_is_a_future(jobs: &Jobs, user_id: u64) -> Result<JobId> {
    SendWelcomeEmail::enqueue(
        jobs,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .await
}

async fn enqueue_with_options(jobs: &Jobs, user_id: u64) -> Result<JobId> {
    SendWelcomeEmail::enqueue(
        jobs,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .delay(Duration::from_secs(60))
    .priority(Priority::High)
    .queue("mail-urgent")
    .unique_key(format!("welcome:{user_id}"))
    .spawn()
    .await
}
```

| Builder method | Effect |
| --- | --- |
| `delay(Duration)` | run no earlier than now plus this |
| `at(DateTime<Utc>)` | run no earlier than this instant. A time in the past means now |
| `priority(Priority)` | `Low`, `Normal`, `High` or `Critical`. Overrides `Job::PRIORITY` |
| `queue(impl Into<String>)` | override `Job::QUEUE` for this one enqueue |
| `unique_key(impl Into<String>)` | deduplicate on this key instead of a payload hash |
| `retries(u32)` | override the budget for this row only |
| `trace_parent(impl Into<String>)` | set the W3C traceparent explicitly |
| `spawn()` | enqueue on the handle's queue |
| `spawn_in(&Tx)` | enqueue inside a transaction |

### Transactionally

```rust
async fn transactional_enqueue(tx: &moso_orm::Tx, user_id: u64) -> Result<JobId> {
    tx.enqueue(
        SendWelcomeEmail,
        SendWelcome {
            user_id,
            locale: None,
        },
    )
    .await
}
```

`Enqueue` extends `moso_orm::Executor`, so this compiles on `&Tx`, `&mut Tx` and `&Db`. A service
function generic over its executor can enqueue. The blanket implementation inspects the executor for
an ambient transaction: with one, it routes to `spawn_in`; without one, to `spawn`.

> [!CAUTION]
> `db.enqueue(..)` compiles and is **not** transactional, because there is no transaction to join.
> This is deliberate and it is the easiest way to lose the guarantee by accident. If the job must
> commit with the work, the receiver has to be a `Tx`. It is not silent: the non-transactional branch
> logs at `DEBUG` on `moso::jobs`, naming the file and line of the enqueue, so
> `RUST_LOG=moso::jobs=debug` finds every one of them in a codebase. `DEBUG` and not `WARN` because
> the call is legitimate in a service that genuinely has no transaction: a warning nobody can act on
> is a warning everybody filters. There is no compile-time distinction available: `Enqueue` is a
> blanket implementation over the sealed `moso_orm::Executor`, and splitting it so `Db` and `Tx` got
> different methods would be a breaking API change.

`tx.enqueue(..)` finds the handle through `Jobs::install()`, a process-wide `OnceLock`. A second
`install()` is a no-op returning `false`, so two applications in one process cannot each install one,
and calling `tx.enqueue(..)` before any `install()` returns an `Error::Config` naming the statement
to add. It is not a compile error. `EnqueueBuilder::spawn_in(tx)` takes the handle explicitly and
runs the identical code path, so prefer it in library code and in tests.

### Deduplicating

Set `unique_for` on the job to deduplicate on a hash of the payload, or call `unique_key(..)` on the
builder for an explicit key. A duplicate enqueue inside the window is a **successful no-op** that
returns a `JobId`, not an error, because the caller asked for the work to happen once and it will.

On the PostgreSQL backend the uniqueness comes from a partial unique index over the active states.
A finished row keeps its key until the sweeper takes it, so `PgQueue::keep_done` (one hour by
default) is the effective floor on the deduplication window. Set it at or above the longest
`unique_for` in your application.

### One at a time

`#[job(serial)]` sets `Job::SERIAL`, and the worker honours it: **two instances of that job type
never run at once, anywhere in the fleet.** It is per *job type*, not per payload: two enqueues with
different arguments still run one after the other. Per-payload exclusion is what `unique_key` already
does, and the two compose: `serial` orders the type, `unique_key` collapses the duplicates.

```rust title="src/jobs/rebuild.rs"
#[job(serial, queue = "index")]
pub async fn rebuild_search_index(args: Rebuild, ctx: JobCtx) -> Result {
    // Two of these at once would fight over the same index.
    Ok(())
}
```

The claim is the job's own **lease**, not a second lock: there is nothing to renew, nothing to
release and nothing to leak, and a worker that dies frees the chain exactly when its lease expires.
On the SQL backend it is one extra clause on the pull statement: a `not exists` against the running
rows, a `min(id)` so one statement cannot lease two of them, and `pg_try_advisory_xact_lock` held for
the length of the statement so two workers pulling in the same instant cannot both win. On Redis it
is a `SET NX` with the lease as its TTL, the same primitive deduplication uses. On `MemoryQueue` the
whole pull runs under one lock, so there is nothing to race.

`Jobs::new` is where the backend learns which names are serial: `SERIAL` is a property of the type, a
backend sees only rows, and that constructor is the one place holding both. A backend that cannot
serialise answers `QueueCapabilities::serial_chains = false`, and `Worker::validate()` turns a serial
job on such a backend into a boot problem rather than a promise that quietly does not hold.

### From inside another job

`ctx.jobs()` hands back the same handle, so a job can enqueue follow-on work.

```rust
async fn fan_out(ctx: &JobCtx, ids: Vec<u64>) -> Result {
    for user_id in ids {
        SendWelcomeEmail::enqueue(ctx.jobs(), SendWelcome { user_id, locale: None }).await?;
    }
    Ok(())
}
```

## Choosing a backend

Every backend implements the same `Queue` trait and answers `capabilities()` honestly. Optional
operations fail by name rather than panicking or silently doing nothing.

| Backend | Transactional enqueue | Push notify | Durable | Cancel | Notes |
| --- | --- | --- | --- | --- | --- |
| `PgQueue` on PostgreSQL | yes | yes | yes | yes | `FOR UPDATE SKIP LOCKED`, `LISTEN`/`NOTIFY` |
| `PgQueue` on SQLite | yes | no | yes | yes | polls instead of listening; SQLite serialises writers |
| `RedisQueue` | no | no | yes | yes | wrap in `Outbox` for the guarantee |
| `MemoryQueue` | no | yes | no | yes | tests and local development |
| `Outbox` | yes | inner's | inner's | inner's | staging table plus a relay in front of Redis |

Pick PostgreSQL unless you have a reason not to. The queue lives in the same database as the work,
which is what makes `tx.enqueue(..)` a single insert in the caller's transaction. `PgQueue` creates
its table, its dead letter table and five indexes on first use. That is deliberately not a migration
(there is no application-visible shape and no foreign keys), but it does mean the tables appear
without appearing in the [migration ledger](./migrations.md).

`RedisQueue` is written against `moso_kv::KvStore` rather than a Redis client, so the same code runs
against the in-memory store. Two consequences worth knowing: `ack` deletes the job blob outright
(a Redis instance that grows without bound is an outage waiting), so there is no history, and
`Priority` is a time-weighted score rather than a strict order. One priority step is worth a day, so
a `Normal` job ready for more than a day beats a `High` one. That is intentional anti-starvation
behaviour and it differs from the PostgreSQL backend's strict `ORDER BY priority DESC, run_at`.

`Outbox` gives Redis the transactional guarantee by staging the row in a database table inside your
transaction and relaying it. It costs a table, a relay to run, and one relay interval (50ms by
default) of latency. The relay pushes first and deletes second, so a crash between the two replays
one job and the inner queue's deduplication collapses it. Running the relay in every process is safe.
Watch `moso_jobs_outbox_lag_seconds`: a stalled relay leaves jobs in a table nobody else is looking
at, and the queue's own numbers will not show it.

## Running a worker

There is no `moso worker` command, and that is a decision rather than a gap. A worker links your job
bodies, so a CLI that ran one would have to link your crate, which ADR-0004 says it cannot. It is
also a process with its own lifecycle: concurrency, lease duration, drain mode and queue weights are
deployment decisions, and hiding them behind a subcommand would mean re-exposing every one of them
as a flag. Write the binary; it is shorter than the flags would be.

```rust title="src/bin/worker.rs"
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = myapp::connect().await?;
    let app = myapp::build(db.clone()).await?;

    let jobs = myapp::jobs::build(db, app.resolver())?;
    let registry = jobs.shared_registry();

    let worker = moso_jobs::Worker::new(jobs, registry)
        .concurrency(8)
        .weighted_queues([
            moso_jobs::QueueWeight::new("default", 3),
            moso_jobs::QueueWeight::new("mail", 1),
        ])
        .lease(Duration::from_secs(120))
        .grace(Duration::from_secs(30))
        .drain_mode(moso_jobs::DrainMode::Requeue)
        .backpressure(Some(50_000));

    worker.run(app.shutdown_signal()).await?;
    Ok(())
}
```

| Method | Default | Effect |
| --- | --- | --- |
| `concurrency(usize)` | one per core, fallback 4 | in-flight jobs. Zero is raised to one |
| `queues([..])` | every queue the registry knows | which queues to pull, all weighted 1 |
| `weighted_queues([QueueWeight])` | all 1 | weighted round-robin. A zero weight is raised to one |
| `lease(Duration)` | 60s | how long a pulled row is owned. Floor of 1s |
| `grace(Duration)` | 25s | how long shutdown waits for in-flight work |
| `poll(Duration)` | 1s | how often to look when the queue cannot push |
| `reclaim_interval(Duration)` | 30s | how often expired leases are reclaimed and depth sampled |
| `drain_mode(DrainMode)` | `Requeue` | what happens to work still running when the grace expires |
| `backpressure(Option<u64>)` | `None` | queue depth above which low-priority enqueues are refused |
| `with_id(WorkerId)` | the hostname | who owns a lease, and what appears in the dashboard |
| `validate()` | | every problem with the registry and the backend, as `BootErrors` |

Queue selection is weighted round-robin with a rotating start offset, so a saturated queue cannot
starve another one.

### Leases, heartbeats and cancellation

A pulled row is leased. A background task renews the lease at one third of its length for as long as
the job runs, so a five minute timeout is safe under a sixty second lease. If the worker dies, the
lease expires and another worker reclaims the row on its reclaim tick. The dead worker's
acknowledgement is refused afterwards.

A long job should race cancellation so it can exit cleanly on shutdown or an operator cancel:

```rust
struct RunsForever;
impl Job for RunsForever {
    type Args = ();
    const NAME: &'static str = "runs_forever";
    const TIMEOUT: Duration = Duration::from_secs(600);
    async fn run(_args: (), ctx: JobCtx) -> Result {
        tokio::select! {
            () = ctx.cancelled() => Err(Error::retry("cancelled")),
            () = tokio::time::sleep(Duration::from_secs(600)) => Ok(()),
        }
    }
}
```

`ctx.is_cancelled()` is the non-awaiting form, and `ctx.heartbeat()` extends the lease explicitly.
Under an inline drain `heartbeat()` succeeds silently, because a drain holds no lease. That is
deliberate so a job body needs no separate code path in tests.

> [!WARNING]
> A blocking job body defeats automatic lease renewal. The heartbeat is a spawned task, and a body
> doing synchronous work with no `.await` never lets it run. Use `moso::task::blocking`, exactly as
> you would in a handler.

### Shutdown and drain

On the signal the worker stops fetching and waits up to the grace by acquiring every semaphore
permit, with no polling and no sleeping. Whatever is still running when the grace expires is
cancelled. `DrainMode::Requeue` then waits up to two further seconds for the cancelled jobs to nack
themselves; `DrainMode::Abandon` does not and lets the leases expire. Both cancel. The difference is
smaller than the names suggest.

### Deterministic runs

`worker.run_once()` performs a single pull-and-execute pass and returns how many jobs ran.
`jobs.drain()` and `worker.drain_inline()` run everything inline until the queue is empty, bounded at
1,000 passes so a self-enqueuing job fails a test with a message instead of hanging it.

## Retries and backoff

`Job::RETRIES` defaults to 25, which under the default ladder spans about eighteen hours. That is
long enough to sit out a third party's overnight incident and short enough that a genuinely broken
job reaches the dead letter queue while somebody still remembers deploying it.

| Backoff | Spec string | Shape |
| --- | --- | --- |
| `Backoff::Immediate` | `immediate` | retry as soon as the worker picks it up |
| `Backoff::Fixed { delay }` | `fixed(30s)` | the same delay every attempt |
| `Backoff::Linear { base, max }` | `linear(30s, max = 1h)` | `base * attempt`, capped |
| `Backoff::Exponential { base, max }` | `exponential(30s, max = 1h)` | doubling, capped. The default is 30s to 1h |

Every delay the worker actually uses is **fully jittered**: sampled uniformly in `[0, delay]`, not in
a narrow band around it. The failure being avoided is a thousand jobs that failed together retrying
together, and a narrow band does not break up a herd. `Backoff::delay(attempt)` is the deterministic
value for a test; `delay_jittered(attempt)` is what runs.

The retry policy travels on the row, not on the type. It is copied onto every enqueued row and the
worker reads the row's copy, so a deploy that changes `BACKOFF` does not retroactively change what an
already-queued row promised. The trap this creates is overriding `fn backoff` without changing
`const BACKOFF`; `JobRegistry::validate()` compares the first eight steps of both and reports the
disagreement at boot.

### Deciding retry per error

| Error | Retried | Meaning |
| --- | --- | --- |
| `Error::retry(detail)` | yes | a transient failure |
| `Error::permanent(detail)` | no | straight to the dead letter queue |
| `Error::Timeout` | yes | the attempt exceeded `Job::TIMEOUT` |
| `Error::Unavailable` | yes | the backend is down |
| `Error::Payload` | skips the budget | the row does not deserialise |
| `Error::Unregistered` | skips the budget | the wire name is not in this build's registry |

`?` on a `moso_core::Error` or a `moso_orm::Error` picks the right one for you. A 503 keeps its retry
advice across the conversion and a 422 does not. A unique violation is permanent; a lost connection
or a statement timeout is retryable. `error.chain()` renders the whole source chain on one line
without repeats, and that is what lands in the dead letter row.

Two failures bypass the budget entirely. A payload that does not deserialise will never deserialise,
so retrying it twenty-five times wastes a day. And a job name this build does not know, which is what
a rolling deploy produces when the old pods drain a queue the new code renamed, is dead-lettered with
its payload intact so you can retry it once the deploy finishes.

## The dead letter queue

Every job out of budget lands there with its payload, attempt count, full error chain, enqueue time,
failure time, trace context and worker identity.

```rust
pub struct DeadLetter {
    pub id: JobId,
    pub name: String,
    pub queue: String,
    pub payload: serde_json::Value,
    pub attempts: u32,
    pub last_error: String,
    pub enqueued_at: DateTime<Utc>,
    pub failed_at: DateTime<Utc>,
    pub trace_parent: Option<String>,
    pub worker: Option<WorkerId>,
}
```

`DlqFilter` narrows by job name, queue, a half-open time window and an error substring, all combined
with `AND`. Every field is optional, so an empty filter matches everything. That is why `retry` and
`discard` both take a mandatory limit.

```rust
assert_eq!(
    queue
        .retry(&DlqFilter::new().job("something_else"), 100)
        .await
        .expect("filtered"),
    0
);

assert_eq!(
    queue
        .retry(&DlqFilter::new().job("fails_permanently"), 2)
        .await
        .expect("retried"),
    2
);
```

`list(&filter, cursor, limit)` pages with an opaque cursor, `get(id)` fetches one, and `stats()`
returns the total, per-job counts sorted with the worst first, and the oldest failure time.

`DlqFilter::error_contains` matches the needle **literally**. On the SQL backends `%` and `_` are
escaped with a backslash and the statement declares `ESCAPE '\'` explicitly on both dialects (a
default escape is a PostgreSQL habit and SQLite has none), so searching for `50% full_up` finds the
rows that say `50% full_up` and not the rows that merely look like them.

## Idempotency

`ctx.once(key, body)` runs the body at most once for that key across every attempt and every worker.
It is backed by a table when a `Db` is in the graph and by the key-value store's compare-and-set
otherwise. With neither, it returns an `Error::Config` naming both providers.

```rust
async fn f(ctx: &JobCtx) -> Result {
    ctx.once("charge:invoice_42", || async { Ok(()) }).await
}
```

Read the boundary precisely, because it is documented precisely. The claim and the body are **not**
one transaction: `body` is an opaque future and there is nothing to enrol it in. A process killed
between claiming and recording leaves a claim with no outcome, and the next call retries the body
after `JobCtx::ONCE_ORPHAN_AFTER`, which is one hour. So it is at-least-once inside that window and
exactly-once outside it. A side effect that must be atomic with its claim belongs in a transaction
you own, with a unique constraint.

The ledger's table name is fixed and its surface is private, on purpose: an idempotency ledger with a
public API is one an application will reach into, and the moment it does the schema is frozen. Its
key-value namespace fails closed, so a store that is down stops the job rather than letting the side
effect happen twice.

## Observing the queue

### Metrics

Eight series plus a running gauge, rendered as Prometheus exposition text by `metrics::snapshot()`.

| Metric | Kind | Labels |
| --- | --- | --- |
| `moso_jobs_enqueued_total` | counter | `job`, `queue` |
| `moso_jobs_duration_seconds` | histogram | `job`, `status` |
| `moso_jobs_queue_depth` | gauge | `queue` |
| `moso_jobs_latency_seconds` | histogram | `queue` |
| `moso_jobs_retries_total` | counter | `job`, `reason` |
| `moso_jobs_dlq_total` | counter | `job` |
| `moso_jobs_backpressure_active` | gauge | `queue` |
| `moso_jobs_outbox_lag_seconds` | gauge | none |
| `moso_jobs_running` | gauge | none |

The `reason` label is mapped onto a closed set of five (`failed`, `timeout`, `unavailable`,
`reclaimed`, `requeued`), because a metric label built from an error message is how a metrics backend
falls over. `requeued` is the one a deploy produces: a job still running when the shutdown grace
expires under `DrainMode::Requeue` is cancelled and put back, and that retry is labelled `requeued`
rather than `failed` so a rolling deploy does not read as an outage. The registry is one per process,
which matters in tests: `metrics::reset()` exists but two tests that reset and then assert on
`snapshot()` cannot run in parallel. Assert on the presence of a series rather than a count.

These live in `moso-jobs` rather than in the HTTP `MetricsRecorder`, because that trait takes a
`RequestSample` and a job is not a request. See [observability](./observability.md) for the request
side.

### The worker health listener

A worker process has no HTTP server, so in Kubernetes it has no liveness probe, no readiness probe
and no metrics endpoint. `WorkerHealth` serves all three on its own socket, or hands you a
`moso::Router` to mount on an existing application.

```rust
let health = moso_jobs::health::WorkerHealth::new(jobs.clone())
    .scheduler(scheduler.readiness())
    .queues(["default", "mail"]);

tokio::spawn(health.serve("0.0.0.0:9090".parse()?, shutdown.clone()));
```

| Route | Behaviour |
| --- | --- |
| `GET /healthz` | 200 while alive and not shutting down. Never touches the queue |
| `GET /readyz` | 503 while shutting down, when the queue probe fails, or while leader election is unresolved |
| `GET /metrics` | Prometheus text, `text/plain; version=0.0.4; charset=utf-8` |

Liveness deliberately does not touch the queue: restarting a worker whose database is down turns one
outage into two. Readiness gates on leader election because the window between "new pod ready" and
"new pod holds the lease" is a window with zero schedulers.

`JobsHealthCheck` is the other direction: a `moso::HealthCheck` you register on the application's own
[`/readyz`](./health-and-shutdown.md), critical by default, with an optional `depth_warning`.

### The dashboard

`moso_jobs::dashboard::routes(jobs)` returns a `moso::Router` mounted at `/_jobs`, and
`Dashboard::new(jobs)` is the same thing with room for the optional pieces. It is off by default in
`JobsConfig` and the whole router is `.hidden()`, so none of it reaches your
[OpenAPI document](./openapi.md).

| Method and path | Shows |
| --- | --- |
| `GET /_jobs` | the backend name, registered job count, schedule count and the other routes |
| `GET /_jobs/queues` | depth and latency per queue |
| `GET /_jobs/schedules` | expression, timezone, next and last run, and who leads |
| `GET /_jobs/dead` | dead letters with full error chains and payloads |
| `POST /_jobs/jobs/{id}/cancel` | ask a running job to stop |
| `POST /_jobs/dead/retry` | bulk retry against a filter |
| `POST /_jobs/dead/discard` | bulk discard against a filter |

The three dead letter routes take `job`, `queue`, `error`, `cursor` and `limit` as query parameters.
`limit` defaults to 50 and is clamped to `1..=200`.

> [!CAUTION]
> These routes show payloads, and a payload carries identifiers, addresses and occasionally tokens.
> The router applies no authorization of its own. Mount it behind a
> [permission check](./permissions.md) or on an internal-only listener. The boot log warns when it
> goes up unguarded.

There is no `GET /_jobs/jobs`: running jobs are visible as the `running` count on `/_jobs/queues`,
and the crate's own route table says the same seven routes this one does.

`GET /_jobs/schedules` answers two different questions from two different places, which is why it has
two fields for them. `last_run` and `leader` come out of the **queue backend**, which every process
in the fleet shares, so every pod answers them the same way: `leader` is the process that enqueued
the last occurrence. `leader_here` is about the process serving the request, so it is `null` unless
that process runs a scheduler *and* you wired it:

```rust title="src/jobs/mod.rs"
use moso_jobs::dashboard::Dashboard;

let routes = Dashboard::new(jobs.clone())
    .scheduler(scheduler.leadership())
    .routes();
```

Without `.scheduler(..)` the field is `null` rather than `false`, because "this pod is not the
leader" and "this pod cannot know" are different answers and only one of them is true.

### The CLI

`moso jobs` is the same data from a terminal, and it is the shape you reach for when the dashboard is
behind a VPN you are not on.

```text
moso jobs list                    # the registered job types
moso jobs status                  # depth, in flight, retrying, dead, and oldest-ready latency
moso jobs schedules               # the cron table with each entry's next occurrence
moso jobs dlq --job send_welcome  # page through the dead letters
moso jobs retry --job send_welcome --limit 50
moso jobs discard --queue mail --limit 50 --yes
```

Every one of them takes `--json`, exits 1 on a real failure, and `discard` asks before it runs unless
`--yes` is given. `--limit` defaults to 50 and is capped at 10,000, for the same reason the HTTP
routes clamp theirs: a bulk operation over an unbounded filter is how a fix becomes an outage.

The CLI does not link your crate, so it asks your binary and reads one JSON document off standard
output, the same protocol `moso routes` uses. The application's half is `fn jobs` in `src/dump.rs`,
which `moso new` writes as a stub that answers "this project does not use moso-jobs". Replace it once
you have a `Jobs` handle:

```rust title="src/dump.rs"
fn jobs(request: &Value) -> Value {
    let Some(jobs) = moso::jobs::Jobs::installed() else {
        return json!({ "available": false, "reason": "no `Jobs` handle is installed" });
    };
    // `stats`, `schedule_runs` and the dead-letter calls are async, and `run` is
    // called from a synchronous branch of `main`, so reach the runtime under you.
    let handle = moso::deps::tokio::runtime::Handle::current();
    let queues = moso::deps::tokio::task::block_in_place(|| handle.block_on(jobs.stats()));
    // …then match on `request["view"]`: "registry", "queues", "schedules", "dead".
    json!({ "available": true, "backend": jobs.queue().name(), /* … */ })
}
```

The request document carries the view and, for the dead-letter operations, the filter, the limit and
the cursor, so adding a filter later is a field rather than a new flag on both sides. The stub in
your project spells out the whole shape in a comment above the function.

The one field the CLI branches on is `available`. A project that has not wired the battery gets a
sentence naming the feature to enable and exit code 1, not an empty table that reads as "your queues
are fine".

### Traces

The W3C traceparent is picked up from the enqueueing request, written onto the job row, and restored
in the worker as a **child** span, so one trace spans the request, the job and whatever the job calls.
Every job gets a `tracing` span carrying job name, queue, id, attempt, worker, trace id, span id and
parent span id. There is no OpenTelemetry dependency here: this crate owns the propagation
(identifiers, header format, parentage) and an application that wires an exporter reads
`trace::current()` and attaches it.

## Backpressure

Above the threshold you set with `Worker::backpressure(Some(n))`, the worker logs at WARN, sets
`moso_jobs_backpressure_active{queue}` and refuses further `Priority::Low` enqueues on that queue
with a **retryable** error. It gates the enqueue rather than the pull, because a worker that stops
pulling a deep queue only makes it deeper. Refused rather than silently dropped, because a discarded
job is the worst of the three outcomes.

## Testing jobs

`TestApp::jobs()` ships behind the `moso-test` feature. The lower-level substitute shown here is
`MemoryQueue` plus `Jobs::drain()`, which is the same DI graph a real worker uses.

```rust
fn harness_with(extra: Option<String>) -> (Jobs, Arc<MemoryQueue>, Arc<Runs>) {
    let queue = Arc::new(MemoryQueue::new());
    let runs = Arc::new(Runs::default());

    let mut providers = moso_core::di::ProviderMapBuilder::new();
    providers.insert_arc(Arc::clone(&runs));
    if let Some(value) = extra {
        providers.insert(value);
    }

    let jobs = Jobs::new(Arc::clone(&queue) as Arc<dyn Queue>, Arc::new(registry()))
        .with_dead_letters(Arc::clone(&queue) as Arc<dyn DeadLetterQueue>)
        .with_resolver(moso_core::Resolver::new(providers.build()));
    (jobs, queue, runs)
}
```

Then `jobs.drain().await?` runs everything inline, `queue.enqueued("send_welcome_email")` returns the
rows for one job name, `queue.all()` returns every row, and `queue.advance(Duration::from_secs(31))`
moves `run_at` and `locked_until` backwards so delays and lease expiries happen without sleeping.
`MemoryQueue` is capped at 100,000 jobs and returns an `Error::Unavailable` with a `help:` line past
that, so a runaway test fails with a message instead of exhausting the machine. Raise it with
`MemoryQueue::new().max_jobs(..)`.

Because `#[job]` leaves the function untouched, the fastest unit test of a job body is to call the
function directly. See [testing](./testing.md) for the rest of the harness.

## Failure modes

- **`db.enqueue(..)` instead of `tx.enqueue(..)`.** Compiles, runs, and loses the guarantee.
- **`tx.enqueue(..)` before `Jobs::install()`.** An `Error::Config` at runtime naming the statement
  to add, not a compile error. Prefer `spawn_in(tx)`.
- **Two applications in one process.** `install()` is a process-wide `OnceLock`; the second call
  returns `false` and does nothing.
- **A renamed payload field.** Rows already queued fail to deserialise, skip the retry budget and go
  to the dead letter queue. Add fields with serde defaults; never rename.
- **`unique_for` longer than `keep_done`.** Deduplication silently stops working past the sweep
  window on PostgreSQL.
- **`Outbox` or `Jobs` without `with_dead_letters(..)`.** Dead letter operations fail by name,
  `/_jobs/dead` returns 501, and `moso jobs dlq` reports the same sentence and exits 1.
- **`moso jobs` says the project does not use `moso-jobs`.** `fn jobs` in `src/dump.rs` is still the
  stub `moso new` wrote. The CLI cannot see your registry. It only sees what that function answers.
- **A self-enqueuing job under `drain_inline`.** Errors after 1,000 passes with a message saying so.
- **`Job::SERIAL` on a backend that cannot serialise.** A boot problem from `Worker::validate()`
  naming the job and the backend, rather than a promise that quietly does not hold.

## See also

- [Scheduled jobs](./scheduled-jobs.md) for cron, intervals and leader election.
- [Transactions and pooling](./transactions.md) for the `Tx` that `tx.enqueue(..)` writes into.
- [Dependency injection](./dependency-injection.md) for the graph `Inject(..)` and `JobCtx::inject`
  resolve against.
- [Errors](./errors.md) for the `moso_core::Error` whose retry advice survives the conversion.
- [Observability](./observability.md) for tracing and metrics on the request side.
- [Sending mail](./mail.md), which travels as a job payload rather than through a dependency edge.
