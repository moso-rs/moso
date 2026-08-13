//! The shipped [`Queue`](crate::Queue) implementations.
//!
//! | Backend | Feature | Notes |
//! | --- | --- | --- |
//! | `PgQueue` | `jobs-pg` (default) | `SELECT … FOR UPDATE SKIP LOCKED`, `LISTEN`/`NOTIFY`, transactional enqueue, partitioned by state so the hot table stays small |
//! | `RedisQueue` | `jobs-redis` | higher throughput; transactional enqueue through `Outbox`, durability subject to the Redis persistence configuration |
//! | `MemoryQueue` | `jobs-memory` (default) | tests and `moso dev`; the same semantics, none of the durability |

#[cfg(feature = "jobs-memory")]
#[cfg_attr(docsrs, doc(cfg(feature = "jobs-memory")))]
mod memory;
#[cfg(all(feature = "jobs-redis", feature = "jobs-pg"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "jobs-redis", feature = "jobs-pg"))))]
mod outbox;
#[cfg(feature = "jobs-pg")]
#[cfg_attr(docsrs, doc(cfg(feature = "jobs-pg")))]
mod pg;
#[cfg(feature = "jobs-redis")]
#[cfg_attr(docsrs, doc(cfg(feature = "jobs-redis")))]
mod redis;

#[cfg(feature = "jobs-memory")]
#[cfg_attr(docsrs, doc(cfg(feature = "jobs-memory")))]
pub use self::memory::MemoryQueue;
#[cfg(all(feature = "jobs-redis", feature = "jobs-pg"))]
#[cfg_attr(docsrs, doc(cfg(all(feature = "jobs-redis", feature = "jobs-pg"))))]
pub use self::outbox::Outbox;
#[cfg(feature = "jobs-pg")]
#[cfg_attr(docsrs, doc(cfg(feature = "jobs-pg")))]
pub use self::pg::PgQueue;
#[cfg(feature = "jobs-redis")]
#[cfg_attr(docsrs, doc(cfg(feature = "jobs-redis")))]
pub use self::redis::RedisQueue;

/// The wire form every durable backend stores, and reads back.
///
/// One shape for three storage engines: the Postgres backend writes these
/// columns, the Redis backend writes this JSON, and the dead-letter tables hold
/// the same fields again. Keeping the mapping in one place is what stops the
/// three from drifting into three slightly different notions of "a queued job".
#[cfg(any(feature = "jobs-pg", feature = "jobs-redis"))]
pub(crate) mod wire {
    use chrono::{DateTime, Utc};

    use crate::{JobState, Priority, QueuedJob, Result, RetryPolicy};

    /// The columns a queue row has, in the order every statement lists them.
    ///
    /// A `const` rather than a string built per call: the order is load-bearing
    /// — `decode` reads by index — and a `select *` would let a migration
    /// reorder the columns underneath it.
    pub(crate) const COLUMNS: &str = "id, name, queue, payload, state, priority, attempt, \
                                      max_attempts, backoff, run_at, enqueued_at, unique_key, \
                                      trace_parent, last_error, locked_by, locked_until, \
                                      lock_token, actor";

    /// How many columns [`COLUMNS`] names.
    pub(crate) const COLUMN_COUNT: usize = 18;

    /// Read one row back into a [`QueuedJob`], plus its lease token.
    pub(crate) fn decode(row: &moso_orm::Row) -> Result<(QueuedJob, Option<String>)> {
        let job = QueuedJob {
            id: row.get_string(0)?.parse()?,
            name: row.get_string(1)?,
            queue: row.get_string(2)?,
            payload: serde_json::from_str(&row.get_string(3)?)?,
            state: state_from_str(&row.get_string(4)?),
            priority: Priority::from_i16(row.get_i16(5)?),
            attempt: u32::try_from(row.get_i64(6)?).unwrap_or(1),
            retry: RetryPolicy::new(
                u32::try_from(row.get_i64(7)?).unwrap_or(0),
                serde_json::from_str(&row.get_string(8)?)?,
            ),
            run_at: row.get_timestamp(9)?,
            enqueued_at: row.get_timestamp(10)?,
            unique_key: row.get_opt::<String>(11)?,
            trace_parent: row.get_opt::<String>(12)?,
            last_error: row.get_opt::<String>(13)?,
            locked_by: row.get_opt::<String>(14)?.map(crate::WorkerId::new),
            locked_until: row.get_opt::<DateTime<Utc>>(15)?,
            // Index 16 is `lock_token`, read out separately as the lease token
            // below; `actor` is the last wire column, index 17.
            actor: row.get_opt::<String>(17)?,
        };
        Ok((job, row.get_opt::<String>(16)?))
    }

    /// The state name a column holds.
    ///
    /// Unknown values decode as [`JobState::Ready`] rather than failing: a row
    /// written by a newer deploy with a state this build does not know is a job
    /// somebody still wants run, and refusing to read it would strand it.
    pub(crate) fn state_from_str(text: &str) -> JobState {
        match text {
            "running" => JobState::Running,
            "retrying" => JobState::Retrying,
            "done" => JobState::Done,
            "dead" => JobState::Dead,
            "cancelled" => JobState::Cancelled,
            _ => JobState::Ready,
        }
    }
}
