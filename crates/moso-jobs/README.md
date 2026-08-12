# moso-jobs

Moso's background-job battery: a framework-owned `Job` trait, transactional enqueue, retries with a
dead-letter queue, cron and interval scheduling with leader election, and workers.

Part of [Moso](https://github.com/lowsbarrel/moso). See `docs/03-batteries/32-jobs.md` for the design.

```rust,ignore
db.transaction(|tx| async move {
    let user = User::insert(new).fetch_one(tx).await?;
    tx.enqueue(SendWelcomeEmail, SendWelcome { user_id: user.id }).await?;
    Ok(user)
}).await?;
```

## Status

**Implemented.** Transactional enqueue, retries with a dead-letter queue, cron and interval
scheduling with leader election, and the workers all ship, over Redis, PostgreSQL and in-memory
backends. No `todo!()` remains, and the SQL-backend tests pass against real Postgres.

## Licence

MIT — see the root [`LICENSE`](../../LICENSE).
