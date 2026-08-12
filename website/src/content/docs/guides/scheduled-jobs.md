---
title: Scheduled jobs
description: Run work on a cron expression or a fixed interval, in a named time zone, with leader election so twenty pods fire an occurrence once and not twenty times.
order: 30
status: shipped
---

A schedule enqueues a job on a clock. You declare it on the same `JobRegistry` that holds your jobs,
a `Scheduler` runs it, and leader election is on by default so twenty pods do not run the nightly
billing job twenty times. That is the second most common jobs bug after non-transactional enqueue,
and the failure mode (everybody charged twenty times) is not one a framework should leave to a
configuration option somebody has to discover.

This page covers declaring cron and interval schedules, the expression grammar, why the time zone is
a name and never an offset, the two leader election mechanisms and when to pick each, what "runs
once" actually guarantees, and how to test a schedule without waiting a day for it. Read
[background jobs](./jobs.md) first: a schedule enqueues a `Job`, so everything about payloads,
retries and the dead letter queue applies unchanged.

> [!IMPORTANT]
> One shape worth knowing before you start: firing an occurrence is driven through leadership rather
> than a "fire this occurrence now" shortcut, so a test takes leadership deliberately with `try_lead`,
> enqueues the payload itself and drains. [Testing a schedule](#testing-a-schedule) is the whole
> recipe.

## Declaring a schedule

Schedules go on the registry, next to the jobs they run. `Cron` takes a five field expression;
`Every` takes a fixed period.

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

The second argument is the payload, typed as `J::Args`. It is fixed at declaration time: a schedule
enqueues the same payload on every occurrence. A job whose payload has to vary per run wants a job
that fans out, not a schedule per variant.

`Cron::new` and `Every::new` are infallible. A malformed expression or an unknown time zone is
recorded on the schedule and reported by `JobRegistry::validate()` at boot, alongside every other
problem, instead of blowing up at the first occurrence in the middle of the night. Read
`Schedule::error()` if you want the message directly.

| Builder method | On | Default | Effect |
| --- | --- | --- | --- |
| `timezone(impl Into<String>)` | `Cron` | `"UTC"` | an IANA name, never an offset |
| `catch_up(bool)` | `Cron` | `false` | replay the occurrences missed while nothing was running |
| `catch_up_limit(usize)` | `Cron` | `60` | how many of them one pass may replay |
| `overlap(Overlap)` | both | `Overlap::Skip` | what to do when this schedule's previous occurrence has not finished |
| `jitter(Duration)` | both | zero | spread the start over up to this long |
| `priority(Priority)` | both | `J::PRIORITY` | the priority the enqueued row gets |

Every default is the one chosen against the surprising outcome. A service down for a day should not
run twenty-four hourly reports the moment it comes back, and a nightly cleanup that takes
twenty-five hours should not accumulate.

### Schedule identity

Each schedule has a `ScheduleId` derived from the job's wire name and the expression, so two
schedules of the same job elect leaders independently. The expression is flattened to characters a
key scheme can hold and fingerprinted, which keeps `0 3 * * *` and `0 4 * * *` apart without letting
either put a `:` in a key.

```rust
let id = moso_jobs::ScheduleId::new("nightly_cleanup", "0 3 * * *");
```

`Every` builds its expression as `every:{seconds}s`, so `Every::new::<Poll>(Duration::from_secs(300),
())` and `Every::new::<Poll>(Duration::from_secs(3600), ())` are different schedules with different
leases. Two schedules that end up sharing a key are reported by `JobRegistry::validate()`.

## Cron expressions

Five fields, separated by whitespace: `minute hour day-of-month month day-of-week`.

| Field | Range | Names accepted |
| --- | --- | --- |
| minute | `0-59` | none |
| hour | `0-23` | none |
| day-of-month | `1-31` | none |
| month | `1-12` | `jan` through `dec`, case insensitive |
| day-of-week | `0-7` | `sun` through `sat`, case insensitive. Both `0` and `7` are Sunday |

Per field the parser accepts `*`, a single number, a `first-last` range, a comma separated list of
any of those, and any of them followed by `/step`. A bare start with a step, written `10/20`, means
"from 10 to the end of the range, every 20".

| Expression | Fires |
| --- | --- |
| `0 3 * * *` | 03:00 every day |
| `*/15 * * * *` | every quarter hour |
| `0 9-17 * * mon-fri` | on the hour, 09:00 to 17:00, weekdays |
| `30 2 1 * *` | 02:30 on the first of every month |
| `0 0 1 * mon` | the first of the month **or** any Monday |

That last row is not a typo. When both `day-of-month` and `day-of-week` are restricted they are
or-ed, which is what `cron(5)` specifies and what somebody porting a crontab expects.

The shorthands expand to exactly this:

| Shorthand | Expands to |
| --- | --- |
| `@yearly`, `@annually` | `0 0 1 1 *` |
| `@monthly` | `0 0 1 * *` |
| `@weekly` | `0 0 * * 0` |
| `@daily`, `@midnight` | `0 0 * * *` |
| `@hourly` | `0 * * * *` |

The parser is hand written, about two hundred lines, because five fields, four operators and a
next-occurrence search do not justify a dependency on the critical path of `cargo add moso`. The
time zone database is a dependency, because nobody should hand-roll one.

The next-occurrence search covers four years, which is wide enough to find a leap day from a
non-leap year. An expression that can never fire again, such as `0 0 30 2 *`, returns no next
occurrence rather than looping.

## Time zones

`Cron::timezone` takes an IANA name. A fixed offset like `+01:00` is deliberately rejected, with an
error message that says why: an offset does not follow the clocks changing, and a nightly job that
runs at 02:00 in summer is a support ticket.

```rust
moso_jobs::Cron::new::<NightlyCleanup>("0 3 * * *", ())
    .timezone("Europe/Rome")
```

The behaviour that buys you, pinned by a test across the 2026 Rome transition: 03:00 CET is 02:00
UTC, and after the clocks go forward 03:00 CEST is 01:00 UTC. Different UTC hour, same local hour,
which is what "03:00 every night" means to everybody except a computer.

The default is `"UTC"`. Names are matched case sensitively, so `europe/rome` is an error and
`Europe/Rome` is not. `moso_jobs::cron::Timezone::parse` is public if you want to validate a name
before it reaches a schedule.

> [!NOTE]
> A daily schedule in a zone that observes daylight saving skips one local hour in spring and repeats
> one in autumn. An occurrence at 02:30 in a zone where 02:00 to 03:00 does not exist that day has no
> instant to fire at. Schedule anything that must not be missed at an hour outside the transition
> window, or use UTC.

## Running the scheduler

The scheduler is a separate loop from the worker. Both can live in the same process or in different
ones.

```rust title="src/bin/worker.rs"
use moso_jobs::Scheduler;

let kv = moso_kv::Kv::in_memory("myapp")?;
let scheduler = Scheduler::new(jobs.clone(), registry.clone(), kv)
    .lease(Duration::from_secs(60))
    .tick(Duration::from_secs(5));

let readiness = scheduler.readiness();
tokio::spawn(scheduler.run(shutdown.clone()));
```

| Method | Default | Effect |
| --- | --- | --- |
| `new(jobs, registry, kv)` | | the key-value store holds the leadership leases |
| `advisory_lock(db)` | off | elect through a PostgreSQL session advisory lock instead |
| `lease(Duration)` | 60s | how long a leadership lease lives. Floor of 1s, ignored under `advisory_lock` |
| `tick(Duration)` | 5s | how often the loop wakes. Floor of 100ms |
| `readiness()` | | a handle to the readiness flag, for a health check that outlives the scheduler |
| `leadership()` | | a handle to what this process leads, for the dashboard's `leader_here` |
| `with_id(WorkerId)` | the hostname | who this process says it is in the durable schedule record |
| `run(Signal)` | | the loop, until the shutdown signal fires |

`Kv::in_memory` is fine when the scheduler runs in exactly one process. For a fleet it has to be a
shared store, or you get one leader per pod, which is the failure you were avoiding. Use a Redis
backed `Kv` (see [cache and key value store](./cache.md)) or switch to the advisory lock.

`run` does three things before it starts: if there are no schedules at all it resolves readiness at
once and waits for shutdown, because a pod with nothing to lead must not hold `/readyz` open for an
election that will never happen. Otherwise it probes the lease store and fails at startup rather
than logging a connection error every tick for the life of the pod. Then it computes the first
occurrence of every schedule and enters the loop.

On each tick it renews or takes each schedule's lease, fires whatever is due, and sets its readiness
flag. On shutdown it releases the leases it holds rather than making the next process wait out a
TTL for a leader that has already left.

Each occurrence it fires is also written down (the schedule's key, the job, this process's
`WorkerId` and the time) in the queue backend, which is the one store every process in the fleet
already shares. That is what `GET /_jobs/schedules` reads for `last_run` and `leader`, and it is
bookkeeping rather than delivery: a backend that keeps no schedule state logs once at DEBUG and the
dashboard reports `null`, which is the truth.

Wire `readiness` into the worker's [health listener](./jobs.md) so a rolling deploy never has a
window with zero schedulers:

```rust
let health = moso_jobs::health::WorkerHealth::new(jobs.clone())
    .scheduler(readiness);
```

`GET /readyz` answers 503 until the election has resolved, which is the whole point: the window
between "new pod ready" and "new pod holds the lease" is a window in which a nightly job does not
run.

## Leader election

Two mechanisms ship, and they are not interchangeable.

| | Key-value lease (default) | PostgreSQL advisory lock |
| --- | --- | --- |
| How | compare-and-swap with a TTL | a session-held advisory lock |
| Works when | always, including a Redis queue with the database elsewhere | you have PostgreSQL |
| Released on process death | when the TTL expires | immediately |
| Worst case lateness after a leader dies | one lease | one tick |
| Cost | a key-value round trip per schedule per tick | one PostgreSQL connection per schedule this process leads |

The default is the lease, because it is the one that works everywhere. Choose the advisory lock when
losing an occurrence to a lease timeout matters more than a held connection does:

```rust
let scheduler = Scheduler::new(jobs, registry, kv).advisory_lock(db);
```

`Scheduler::new` still takes a `Kv` under the advisory lock; the parameter is not optional even
though the election no longer uses it.

The loop renews before it does anything else. A leader that cannot renew stops leading immediately,
before another process can take the lease, so the two never overlap through a renewal failure.
Losing a lease logs at WARN and the next pass tries to take it again.

### Asking and taking

```rust
if scheduler.is_leader(&id) {
    // this process is currently leading that schedule
}
```

`try_lead(&id)` takes leadership once and hands back a `Leadership` guard with a `resign()` method.
That is the seam a test uses, and it is what proves the guarantee:

```rust
let kv = moso_kv::Kv::in_memory("scheduler-election").expect("in-memory kv");
let id = ScheduleId::new("nightly_cleanup", "0 3 * * *");

let mut leaders = 0;
let mut held = Vec::new();
for _ in 0..10 {
    let scheduler = scheduler(kv.clone());
    if let Some(leadership) = scheduler.try_lead(&id).await.expect("the store is up") {
        leaders += 1;
        held.push(leadership);
    }
}
assert_eq!(leaders, 1, "exactly one process may lead a schedule");

for leadership in held {
    leadership.resign().await;
}
let scheduler = scheduler(kv);
assert!(
    scheduler.try_lead(&id).await.expect("up").is_some(),
    "a resigned lease must be takeable"
);
```

## Run once semantics

Leader election is the first line. The second is the row itself. Every occurrence is enqueued with a
`unique_key` of `schedule:{schedule_id}:{unix_timestamp_of_the_occurrence}`, so two schedulers that
both believed they led for one tick still produce one job row: the second insert collides and is a
successful no-op.

That gives you at-most-once per occurrence under the deduplication window, plus the queue's own
at-least-once delivery of whatever row exists. Concretely:

- A schedule fires once per occurrence across the fleet, even during a leadership handover.
- The job that occurrence enqueued still retries on failure like any other job, so the **work** can
  run more than once. If it must not, make the body idempotent with `ctx.once(key, ..)`.
- Deduplication on the PostgreSQL backend is bounded by `PgQueue::keep_done` (one hour by default),
  because a finished row keeps its key until the sweeper takes it. A schedule that fires more often
  than once an hour is well inside that; one that fires less often is relying on the occurrence
  timestamp being different anyway.

An occurrence whose leader dies between taking the lease and enqueuing is lost, not replayed, unless
`catch_up` is on. Under the key-value lease the next process picks the schedule up after the lease
expires; under the advisory lock, on the next tick.

## Overlap and catch up

| `Overlap` | Behaviour when this schedule's previous occurrence has not finished |
| --- | --- |
| `Skip` (default) | do not enqueue this occurrence, log at INFO and move on |
| `Queue` | enqueue anyway, and log at INFO that the schedule is overlapping |
| `Allow` | enqueue and ask nothing: no overlap round trip at all |

The question is scoped to the schedule, by the identifier of the row it enqueued last time, through
`Queue::find`. A schedule sharing a busy queue with unrelated work is unaffected by it. "Has not
finished" and not "is running": a previous occurrence still sitting *ready* on a backed-up queue has
not done its work either, and enqueuing another is the accumulation `Skip` exists to prevent.

`Queue` and `Allow` both enqueue. Whether the new occurrence waits behind the running one is the
job's decision and not the schedule's: `#[job(serial)]` is what stops two instances of a job type
running at once, and it is described in [background jobs](./jobs.md). A backend that cannot look a
row up by identifier degrades `Skip` to `Allow` and says so at WARN, rather than becoming a schedule
that silently stops firing.

`catch_up` is off by default. With it off, an occurrence more than four ticks late (twenty seconds at
the default tick) is skipped with a log line rather than replayed, so a service down for a day does
not run twenty-four hourly reports on the way back up.

With it on, **every** missed occurrence is replayed, each as its own row with its own occurrence
timestamp in its deduplication key: six missed nights are six jobs, not one. That is bounded:

```rust title="src/jobs/mod.rs"
moso_jobs::Cron::new::<NightlyReport>("0 * * * *", ())
    .catch_up(true)
    .catch_up_limit(12)
```

`catch_up_limit` defaults to **60** (a day and a half of an hourly schedule, two months of a daily
one, an hour of a per-minute one), because a per-minute schedule whose scheduler was down for a week
has 10,080 missed occurrences and enqueuing all of them in one tick is a self-inflicted thundering
herd. When the cap truncates, the oldest are dropped (last night's report is worth more than the one
from six nights ago) and the scheduler logs at **WARN** naming how many. A limit of `1` reproduces
the pre-cap behaviour exactly: one occurrence replayed, the rest reported.

## Jitter

Twenty schedules all written `0 3 * * *` fire in the same second, and the third party they all call
notices. `jitter(Duration)` spreads the start uniformly over up to that long by pushing the enqueued
row's `run_at` forward:

```rust
moso_jobs::Cron::new::<RefreshFeeds>("0 3 * * *", ())
    .jitter(Duration::from_secs(300))
```

Jitter is applied at enqueue time, so it moves when the job becomes eligible, not when the schedule
fires. It composes with the retry ladder's own full jitter, which is a separate mechanism described
in [background jobs](./jobs.md).

## Intervals

`Every::new::<J>(period, args)` is the fixed-interval form. A zero period is recorded as an error and
reported by `validate()`, because an interval of zero would enqueue as fast as the scheduler ticks.

> [!NOTE]
> `Every` measures from the previous **enqueue**, not from the previous completion. The scheduler
> sets the next occurrence to now plus the period at the moment it fires. A job that takes longer
> than its period will therefore come due again while it is still running, which is what
> `Overlap::Skip` is for.

The interval also has a floor in practice: the scheduler only notices an occurrence on a tick, so a
period shorter than `tick` (5s by default) will not fire faster than the tick.

## Testing a schedule

You do not have to wait for 03:00. Three seams, from cheapest to most complete.

### Compute the next occurrence

`Schedule::next_after(after)` is pure and deterministic. Convert the builder into a `Schedule` and
ask it.

```rust
use chrono::{TimeZone, Utc};

let schedule: moso_jobs::Schedule = moso_jobs::Cron::new::<NightlyCleanup>("0 3 * * *", ())
    .timezone("Europe/Rome")
    .into();

let after = Utc.with_ymd_and_hms(2026, 3, 27, 12, 0, 0).unwrap();
assert_eq!(
    schedule.next_after(after).unwrap(),
    Some(Utc.with_ymd_and_hms(2026, 3, 28, 2, 0, 0).unwrap()),
);
```

For the expression alone, without a job, `moso_jobs::cron::Expression::parse` plus
`Timezone::parse` gives you the same answer:

```rust
use moso_jobs::cron::{Expression, Timezone};

let cron = Expression::parse("0 3 * * *").expect("valid");
let rome = Timezone::parse("Europe/Rome").expect("a real zone");
assert!(cron.next_after(after, rome).is_some());
```

This is the test to write for anything time zone sensitive: pin the UTC instant on both sides of a
daylight saving transition.

### Catch a typo at boot

```rust
let errors = registry().validate();
assert!(errors.is_empty(), "{errors}");
```

`validate()` reports an unparseable expression, an unknown time zone, a schedule naming a job that
is not registered (with a "did you mean"), and two schedules sharing a key. Put this assertion in
your test suite and the class of bug where a nightly job silently never ran stops existing.
`Worker::run` refuses to start on the same report, so a worker binary cannot ship past it either.

### Take leadership and run the job

There is no public "fire this occurrence now" call. What you can do is take leadership deliberately
with `try_lead`, enqueue the payload yourself, and drain:

```rust
let (jobs, queue, runs) = harness();
let id = ScheduleId::new("nightly_cleanup", "0 3 * * *");
let scheduler = Scheduler::new(jobs.clone(), registry.clone(), kv.clone());

let leadership = scheduler.try_lead(&id).await?.expect("nobody else is leading");
NightlyCleanup::enqueue(&jobs, ()).await?;
jobs.drain().await?;
assert_eq!(queue.enqueued("nightly_cleanup").len(), 1);
leadership.resign().await;
```

Holding leadership is visible to the process that took it: `scheduler.is_leader(&id)` is `true`
while the guard lives, and `false` again after `resign()` or a drop, which is the same flag
`Dashboard::scheduler(..)` reads for `leader_here`.

`MemoryQueue::advance(Duration)` moves `run_at` and `locked_until` backwards, so a jittered or
delayed occurrence becomes eligible without a sleep. The full in-memory harness is in
[background jobs](./jobs.md).

## Failure modes

- **`Kv::in_memory` in a fleet.** Every pod elects itself. The lease store has to be shared or you
  need `advisory_lock(db)`.
- **A fixed offset as a time zone.** Rejected at parse, reported by `validate()`. Use the IANA name.
- **A schedule for a job that is not registered.** Reported by `validate()`, and silently skipped
  every tick if you never call it.
- **An occurrence at a local time that does not exist** on the day the clocks go forward. Move the
  hour or use UTC.
- **A period shorter than `tick`.** The schedule fires no faster than the scheduler wakes.
- **A backlog longer than `catch_up_limit`.** The oldest occurrences are dropped and the count is in
  the WARN line. Raise the limit if every period has to be accounted for, or make the job compute the
  range it should cover from your own data.
- **Reading `leader_here` from a pod that runs no scheduler.** It is `null` there, not `false`. Read
  `leader` for the fleet-wide answer, or wire `Dashboard::scheduler(..)` in the process that leads.

## See also

- [Background jobs](./jobs.md) for defining jobs, workers, retries and the dead letter queue.
- [Cache and key value store](./cache.md) for the `Kv` handle the leases live in.
- [Transactions and pooling](./transactions.md) for the `Db` the advisory lock holds a session on.
- [Health and shutdown](./health-and-shutdown.md) for the readiness contract the scheduler feeds.
- [Observability](./observability.md) for the metrics the enqueued occurrences show up in.
