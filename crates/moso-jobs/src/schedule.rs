//! Running work on a clock: [`Cron`], [`Every`], and the leader election that
//! stops twenty pods running the nightly job twenty times.
//!
//! Leader election is on by default rather than being a configuration option
//! somebody has to discover. It is the second most common jobs bug after
//! non-transactional enqueue, and the failure — a nightly billing run charging
//! everybody twenty times — is not one a framework should leave to a footnote.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::cron::{Expression, Timezone};
use crate::{Job, Overlap, Priority, Result};

/// A schedule's name, which is also its leader-election key.
///
/// Derived from the job's wire name and the expression, so two different
/// schedules of the same job elect leaders independently.
///
/// ```
/// use moso_jobs::ScheduleId;
///
/// let id = ScheduleId::new("nightly_cleanup", "0 3 * * *");
/// assert!(!id.as_str().is_empty());
/// ```
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScheduleId(String);

impl ScheduleId {
    /// Derive an identifier from a job name and an expression.
    ///
    /// The identifier becomes a lease key, so it is reduced to characters a key
    /// scheme can hold: anything else is replaced by a hyphen and the whole
    /// expression is fingerprinted, which keeps `0 3 * * *` and `0 4 * * *`
    /// apart without letting either of them put a `:` in a key.
    ///
    /// ```
    /// use moso_jobs::ScheduleId;
    ///
    /// let nightly = ScheduleId::new("poll_feeds", "every:300s");
    /// let hourly = ScheduleId::new("poll_feeds", "every:3600s");
    /// assert_ne!(nightly, hourly);
    /// assert!(!nightly.as_str().contains(':'));
    /// ```
    #[must_use]
    pub fn new(job: &str, expression: &str) -> Self {
        let safe: String = expression
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(32)
            .collect();
        // The fingerprint is what keeps two expressions apart once the unsafe
        // characters have been flattened into hyphens.
        let fingerprint = crate::job::payload_fingerprint(
            expression,
            &serde_json::Value::String(expression.to_owned()),
        );
        let tail = fingerprint.rsplit(':').next().unwrap_or_default();
        Self(format!("{job}-{safe}-{tail}"))
    }

    /// Wrap a key read back out of a store.
    ///
    /// The inverse of storing [`as_str`](ScheduleId::as_str): a backend that
    /// wrote the key into a column reads it back through here rather than
    /// re-deriving it from a job name and an expression it no longer has.
    ///
    /// ```
    /// use moso_jobs::ScheduleId;
    ///
    /// let id = ScheduleId::new("nightly_cleanup", "0 3 * * *");
    /// assert_eq!(ScheduleId::from_key(id.as_str()), id);
    /// ```
    #[must_use]
    pub fn from_key(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    /// The identifier.
    ///
    /// ```
    /// # use moso_jobs::ScheduleId;
    /// assert!(ScheduleId::new("j", "0 3 * * *").as_str().starts_with("j-"));
    /// ```
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl core::fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One schedule's last occurrence, as the queue backend durably holds it.
///
/// Leadership is per process and the dashboard is served by whichever process
/// the request reached, so neither "when did this last run" nor "who is leading
/// it" has an in-process answer worth printing. Both are written here, by the
/// leader, into the one store the whole fleet already shares — which is what
/// makes `GET /_jobs/schedules` answer the same way from every pod.
///
/// ```
/// use moso_jobs::{ScheduleId, ScheduleRun, WorkerId};
///
/// let run = ScheduleRun::new(
///     ScheduleId::new("nightly_cleanup", "0 3 * * *"),
///     "nightly_cleanup",
///     WorkerId::new("pod-7"),
///     chrono::Utc::now(),
/// );
/// assert_eq!(run.job, "nightly_cleanup");
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ScheduleRun {
    /// Which schedule, by its leader-election key.
    pub schedule: ScheduleId,
    /// The wire name of the job it enqueued.
    pub job: String,
    /// The process that enqueued that occurrence.
    ///
    /// The fleet's answer to "who is leading this schedule", accurate as of
    /// [`ran_at`](ScheduleRun::ran_at) — a schedule that fires nightly has a
    /// nightly-fresh answer, and [`Scheduler::is_leader`] is the live one for
    /// the process you are asking.
    pub leader: crate::WorkerId,
    /// When that occurrence was enqueued.
    pub ran_at: DateTime<Utc>,
}

impl ScheduleRun {
    /// Record `leader` firing `schedule` at `ran_at`.
    ///
    /// ```
    /// # use moso_jobs::{ScheduleId, ScheduleRun, WorkerId};
    /// let run = ScheduleRun::new(
    ///     ScheduleId::new("j", "0 3 * * *"),
    ///     "j",
    ///     WorkerId::new("pod-1"),
    ///     chrono::Utc::now(),
    /// );
    /// assert_eq!(run.leader.as_str(), "pod-1");
    /// ```
    #[must_use]
    pub fn new(
        schedule: ScheduleId,
        job: impl Into<String>,
        leader: crate::WorkerId,
        ran_at: DateTime<Utc>,
    ) -> Self {
        Self {
            schedule,
            job: job.into(),
            leader,
            ran_at,
        }
    }
}

/// Something that runs on a clock: a [`Cron`] or an [`Every`], erased.
///
/// ```no_run
/// use moso_jobs::{Cron, Schedule};
///
/// # fn f(c: Cron) {
/// let schedule: Schedule = c.into();
/// let _ = schedule.id();
/// # }
/// ```
#[derive(Debug)]
pub struct Schedule {
    /// Its leader-election key.
    id: ScheduleId,
    /// The wire name of the job it enqueues.
    job: &'static str,
    /// The queue to enqueue on.
    queue: &'static str,
    /// The serialised payload, built once when the schedule was declared.
    payload: serde_json::Value,
    /// How the next occurrence is computed.
    kind: ScheduleKind,
    /// The parsed expression, when it parsed.
    parsed: Option<Expression>,
    /// The zone the expression is evaluated in.
    zone: Timezone,
    /// What to do when the previous run is still going.
    overlap: Overlap,
    /// Whether occurrences missed during downtime are run on restart.
    catch_up: bool,
    /// At most this many missed occurrences are replayed in one pass.
    catch_up_limit: usize,
    /// How urgent.
    priority: Priority,
    /// Up to this much random delay, so twenty schedules at 03:00 do not all
    /// fire in the same second.
    jitter: Duration,
    /// A parse failure, held until the registry can turn it into a boot error.
    ///
    /// `Cron::new` is infallible so a registry reads as a list of declarations;
    /// the error surfaces from `JobRegistry::validate`, with every other boot
    /// problem, rather than as a `?` in the middle of a builder chain.
    error: Option<String>,
}

/// How a schedule's next occurrence is computed.
///
/// ```no_run
/// use moso_jobs::ScheduleKind;
///
/// # fn f(k: &ScheduleKind) {
/// let _ = matches!(k, ScheduleKind::Cron { .. });
/// # }
/// ```
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ScheduleKind {
    /// A five-field cron expression in a named timezone.
    Cron {
        /// The expression, as written.
        expression: String,
        /// The IANA timezone name. `"UTC"` unless set.
        timezone: String,
    },
    /// A fixed interval from the last run.
    Every {
        /// How long between runs.
        period: Duration,
    },
}

impl Schedule {
    /// Its leader-election key.
    ///
    /// ```no_run
    /// # use moso_jobs::{Schedule, ScheduleId};
    /// # fn f(s: &Schedule) { let _: &ScheduleId = s.id(); }
    /// ```
    #[must_use]
    pub fn id(&self) -> &ScheduleId {
        &self.id
    }

    /// The wire name of the job it enqueues.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: &'static str = s.job(); }
    /// ```
    #[must_use]
    pub fn job(&self) -> &'static str {
        self.job
    }

    /// The queue it enqueues on.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: &'static str = s.queue(); }
    /// ```
    #[must_use]
    pub fn queue(&self) -> &'static str {
        self.queue
    }

    /// The payload every occurrence carries.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _ = s.payload(); }
    /// ```
    #[must_use]
    pub fn payload(&self) -> &serde_json::Value {
        &self.payload
    }

    /// How the next occurrence is computed.
    ///
    /// ```no_run
    /// # use moso_jobs::{Schedule, ScheduleKind};
    /// # fn f(s: &Schedule) { let _: &ScheduleKind = s.kind(); }
    /// ```
    #[must_use]
    pub fn kind(&self) -> &ScheduleKind {
        &self.kind
    }

    /// What to do when the previous run is still going.
    ///
    /// ```no_run
    /// # use moso_jobs::{Overlap, Schedule};
    /// # fn f(s: &Schedule) { let _: Overlap = s.overlap(); }
    /// ```
    #[must_use]
    pub fn overlap(&self) -> Overlap {
        self.overlap
    }

    /// Whether occurrences missed while nothing was running are run on restart.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: bool = s.catches_up(); }
    /// ```
    #[must_use]
    pub fn catches_up(&self) -> bool {
        self.catch_up
    }

    /// How many missed occurrences one catch-up pass may replay.
    ///
    /// Only consulted when [`catches_up`](Schedule::catches_up) is on.
    ///
    /// ```
    /// # use moso_jobs::{Cron, Job, JobCtx, Result, Schedule};
    /// # struct Nightly;
    /// # impl Job for Nightly {
    /// #     type Args = ();
    /// #     const NAME: &'static str = "nightly";
    /// #     async fn run(_a: (), _c: JobCtx) -> Result { Ok(()) }
    /// # }
    /// let schedule: Schedule = Cron::new::<Nightly>("0 * * * *", ()).into();
    /// assert_eq!(schedule.catch_up_limit(), moso_jobs::schedule::DEFAULT_CATCH_UP_LIMIT);
    /// ```
    #[must_use]
    pub fn catch_up_limit(&self) -> usize {
        self.catch_up_limit
    }

    /// How urgent an occurrence is.
    ///
    /// ```no_run
    /// # use moso_jobs::{Priority, Schedule};
    /// # fn f(s: &Schedule) { let _: Priority = s.priority(); }
    /// ```
    #[must_use]
    pub fn priority(&self) -> Priority {
        self.priority
    }

    /// How much random delay is spread over the start.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: std::time::Duration = s.jitter(); }
    /// ```
    #[must_use]
    pub fn jitter(&self) -> Duration {
        self.jitter
    }

    /// The next time this should run, after `after`.
    ///
    /// `None` when the expression never matches again, which a cron expression
    /// can genuinely be — `0 0 30 2 *` is the 30th of February.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) when the expression did not
    /// parse. Callers reach this only if `validate` was skipped.
    ///
    /// ```no_run
    /// # use chrono::{DateTime, Utc};
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule, at: DateTime<Utc>) -> moso_jobs::Result<Option<DateTime<Utc>>> {
    /// s.next_after(at)
    /// # }
    /// ```
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        if let Some(detail) = &self.error {
            return Err(crate::Error::config(detail.clone()));
        }
        match &self.kind {
            ScheduleKind::Cron { expression, .. } => {
                let parsed = self.parsed.as_ref().ok_or_else(|| {
                    crate::Error::config(format!("`{expression}` was never parsed"))
                })?;
                Ok(parsed.next_after(after, self.zone))
            }
            ScheduleKind::Every { period } => Ok(chrono::Duration::from_std(*period)
                .ok()
                .map(|period| after + period)),
        }
    }

    /// The parse failure this schedule is carrying, when it has one.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: Option<&str> = s.error(); }
    /// ```
    #[must_use]
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// The expression, as an operator reads it in the dashboard.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: String = s.expression(); }
    /// ```
    #[must_use]
    pub fn expression(&self) -> String {
        match &self.kind {
            ScheduleKind::Cron { expression, .. } => expression.clone(),
            ScheduleKind::Every { period } => {
                format!("every {}", humantime::format_duration(*period))
            }
        }
    }

    /// The timezone the expression is evaluated in.
    ///
    /// ```no_run
    /// # use moso_jobs::Schedule;
    /// # fn f(s: &Schedule) { let _: &str = s.timezone(); }
    /// ```
    #[must_use]
    pub fn timezone(&self) -> &str {
        self.zone.name()
    }

    /// The shape both builders produce.
    fn build<J: Job>(kind: ScheduleKind, expression: &str, args: J::Args) -> Self {
        let (payload, error) = match serde_json::to_value(&args) {
            Ok(payload) => (payload, None),
            Err(error) => (
                serde_json::Value::Null,
                Some(format!(
                    "the payload for `{}` does not serialise: {error}",
                    J::NAME
                )),
            ),
        };
        let parsed = match &kind {
            ScheduleKind::Cron { expression, .. } => Expression::parse(expression).ok(),
            ScheduleKind::Every { .. } => None,
        };
        let error = error.or_else(|| match &kind {
            ScheduleKind::Cron { expression, .. } if parsed.is_none() => {
                Expression::parse(expression).err().map(|e| e.to_string())
            }
            _ => None,
        });

        Self {
            id: ScheduleId::new(J::NAME, expression),
            job: J::NAME,
            queue: J::QUEUE,
            payload,
            kind,
            parsed,
            zone: Timezone::utc(),
            overlap: Overlap::default(),
            catch_up: false,
            catch_up_limit: DEFAULT_CATCH_UP_LIMIT,
            priority: J::PRIORITY,
            jitter: Duration::ZERO,
            error,
        }
    }
}

/// A job on a cron expression.
///
/// ```no_run
/// use moso_jobs::{Cron, Job, Overlap};
///
/// fn nightly<J: Job>(args: J::Args) -> Cron {
///     Cron::new::<J>("0 3 * * *", args)
///         .timezone("Europe/Rome")
///         .catch_up(false)
///         .overlap(Overlap::Skip)
/// }
/// ```
///
/// # Two things that surprise people
///
/// **`day-of-month` and `day-of-week` are or-ed** when both are restricted, as
/// `cron(5)` specifies: `0 0 1 * mon` fires on the first of the month *and* on
/// every Monday.
///
/// **`catch_up` is off by default.** A service down for a day should not run
/// twenty-four hourly reports the moment it comes back.
#[derive(Debug)]
pub struct Cron(Schedule);

impl Cron {
    /// Run `J` on `expression`, with `args`.
    ///
    /// Infallible: a malformed expression is held and reported by
    /// [`JobRegistry::validate`](crate::JobRegistry::validate) with every other
    /// boot problem. A `?` in the middle of a registry declaration would put one
    /// problem per restart in front of the operator instead of all of them.
    ///
    /// ```no_run
    /// # use moso_jobs::{Cron, Job};
    /// # fn f<J: Job>(args: J::Args) { let _ = Cron::new::<J>("0 3 * * *", args); }
    /// ```
    #[must_use]
    pub fn new<J: Job>(expression: &str, args: J::Args) -> Self {
        Self(Schedule::build::<J>(
            ScheduleKind::Cron {
                expression: expression.to_owned(),
                timezone: "UTC".to_owned(),
            },
            expression,
            args,
        ))
    }

    /// Evaluate the expression in this timezone. `"UTC"` by default.
    ///
    /// Named, not an offset: `Europe/Rome` handles daylight saving and `+01:00`
    /// does not, and a nightly job that runs at 02:00 in summer is a support
    /// ticket.
    ///
    /// ```no_run
    /// # use moso_jobs::Cron;
    /// # fn f(c: Cron) { let _ = c.timezone("Europe/Rome"); }
    /// ```
    #[must_use]
    pub fn timezone(mut self, timezone: impl Into<String>) -> Self {
        let name = timezone.into();
        match Timezone::parse(&name) {
            Ok(zone) => {
                self.0.zone = zone;
                if let ScheduleKind::Cron { timezone, .. } = &mut self.0.kind {
                    *timezone = name;
                }
            }
            Err(error) => {
                // Held rather than returned, for the same reason a bad
                // expression is: one boot report, not one problem per restart.
                self.0.error.get_or_insert(error.to_string());
            }
        }
        self
    }

    /// Run occurrences missed while nothing was running. `false` by default.
    ///
    /// The default is off because catching up is almost never what anyone
    /// wants: a service down for a day should not run twenty-four hourly
    /// reports the moment it comes back.
    ///
    /// With it **on**, *every* missed occurrence is replayed as its own row —
    /// each with its own occurrence timestamp, so the dedup key still makes two
    /// schedulers produce one row per occurrence — up to
    /// [`catch_up_limit`](Cron::catch_up_limit). Past that the oldest are
    /// dropped, and the scheduler logs at `WARN` naming how many.
    ///
    /// ```no_run
    /// # use moso_jobs::Cron;
    /// # fn f(c: Cron) { let _ = c.catch_up(true); }
    /// ```
    #[must_use]
    pub fn catch_up(mut self, catch_up: bool) -> Self {
        self.0.catch_up = catch_up;
        self
    }

    /// At most this many missed occurrences per catch-up pass.
    ///
    /// A cap and not a courtesy: a per-minute schedule whose scheduler was down
    /// for a week has 10,080 missed occurrences, and enqueuing all of them the
    /// instant the pod comes back is a self-inflicted thundering herd against
    /// whatever the job talks to. [`DEFAULT_CATCH_UP_LIMIT`] is the default;
    /// zero is raised to one, because a catch-up that replays nothing is
    /// `catch_up(false)` spelled confusingly.
    ///
    /// **A limit of 1 is the pre-cap behaviour**: one missed occurrence
    /// replayed, the rest dropped with a `WARN` saying so.
    ///
    /// ```no_run
    /// # use moso_jobs::Cron;
    /// # fn f(c: Cron) { let _ = c.catch_up(true).catch_up_limit(12); }
    /// ```
    #[must_use]
    pub fn catch_up_limit(mut self, limit: usize) -> Self {
        self.0.catch_up_limit = limit.max(1);
        self
    }

    /// What to do when the previous run is still going.
    ///
    /// ```no_run
    /// # use moso_jobs::{Cron, Overlap};
    /// # fn f(c: Cron) { let _ = c.overlap(Overlap::Queue); }
    /// ```
    #[must_use]
    pub fn overlap(mut self, overlap: Overlap) -> Self {
        self.0.overlap = overlap;
        self
    }

    /// Spread the start over up to this long.
    ///
    /// ```no_run
    /// # use moso_jobs::Cron;
    /// # fn f(c: Cron) { let _ = c.jitter(std::time::Duration::from_secs(30)); }
    /// ```
    #[must_use]
    pub fn jitter(mut self, jitter: Duration) -> Self {
        self.0.jitter = jitter;
        self
    }

    /// Override the priority the enqueued job gets.
    ///
    /// ```no_run
    /// # use moso_jobs::{Cron, Priority};
    /// # fn f(c: Cron) { let _ = c.priority(Priority::Low); }
    /// ```
    #[must_use]
    pub fn priority(mut self, priority: Priority) -> Self {
        self.0.priority = priority;
        self
    }
}

impl From<Cron> for Schedule {
    fn from(cron: Cron) -> Self {
        cron.0
    }
}

/// A job on a fixed interval.
///
/// ```no_run
/// use moso_jobs::{Every, Job};
/// use std::time::Duration;
///
/// fn poll<J: Job>(args: J::Args) -> Every {
///     Every::new::<J>(Duration::from_secs(300), args)
/// }
/// ```
#[derive(Debug)]
pub struct Every(Schedule);

impl Every {
    /// Run `J` every `period`, with `args`.
    ///
    /// The interval is measured from the previous **enqueue**, not from the
    /// previous completion: the scheduler sets the next occurrence to now plus
    /// the period at the moment it fires. A job that takes longer than its
    /// period therefore comes due again while it is still running, which is
    /// what [`Overlap::Skip`] is for.
    ///
    /// ```no_run
    /// # use moso_jobs::{Every, Job};
    /// # fn f<J: Job>(args: J::Args) {
    /// let _ = Every::new::<J>(std::time::Duration::from_secs(60), args);
    /// # }
    /// ```
    #[must_use]
    pub fn new<J: Job>(period: Duration, args: J::Args) -> Self {
        let expression = format!("every:{}s", period.as_secs());
        let mut schedule = Schedule::build::<J>(ScheduleKind::Every { period }, &expression, args);
        if period.is_zero() {
            schedule.error.get_or_insert_with(|| {
                format!(
                    "the interval for `{}` is zero\n\
                     help: an interval of zero would enqueue as fast as the scheduler ticks; \
                     write the shortest period the work actually needs",
                    schedule.job
                )
            });
        }
        Self(schedule)
    }

    /// What to do when the previous run is still going.
    ///
    /// ```no_run
    /// # use moso_jobs::{Every, Overlap};
    /// # fn f(e: Every) { let _ = e.overlap(Overlap::Allow); }
    /// ```
    #[must_use]
    pub fn overlap(mut self, overlap: Overlap) -> Self {
        self.0.overlap = overlap;
        self
    }

    /// Spread the start over up to this long.
    ///
    /// ```no_run
    /// # use moso_jobs::Every;
    /// # fn f(e: Every) { let _ = e.jitter(std::time::Duration::from_secs(5)); }
    /// ```
    #[must_use]
    pub fn jitter(mut self, jitter: Duration) -> Self {
        self.0.jitter = jitter;
        self
    }

    /// Override the priority the enqueued job gets.
    ///
    /// ```no_run
    /// # use moso_jobs::{Every, Priority};
    /// # fn f(e: Every) { let _ = e.priority(Priority::Low); }
    /// ```
    #[must_use]
    pub fn priority(mut self, priority: Priority) -> Self {
        self.0.priority = priority;
        self
    }
}

impl From<Every> for Schedule {
    fn from(every: Every) -> Self {
        every.0
    }
}

/// Where the leadership lease lives.
///
/// Two ways to elect one leader out of twenty pods, and they are not
/// interchangeable:
///
/// - A **key-value lease** works for any deployment, including one whose queue
///   is Redis and whose database is somewhere else. It is a compare-and-swap
///   with a TTL, so the upper bound on how late an occurrence can be after a
///   leader dies is the lease.
/// - A **PostgreSQL advisory lock** is held by a *session*, so it is released
///   the instant the process dies rather than when a TTL expires — no lost
///   occurrence, no waiting. It needs a PostgreSQL connection dedicated to
///   holding it.
///
/// `docs/03-batteries/32-jobs.md` specifies the advisory lock; the key-value
/// lease is what makes the same code work for a deployment that has no
/// PostgreSQL. Both are here, and [`Scheduler::advisory_lock`] chooses.
enum Election {
    /// A compare-and-swap lease in the key-value store.
    Lease(moso_kv::Kv),
    /// A PostgreSQL session-level advisory lock.
    Advisory(moso_orm::Db),
}

impl core::fmt::Debug for Election {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Lease(_) => f.write_str("Lease"),
            Self::Advisory(_) => f.write_str("Advisory"),
        }
    }
}

/// Runs the schedules, on whichever process holds the lease.
///
/// One per process. Every instance tries to take the lease for each schedule;
/// exactly one wins, and it renews while it lives. A process that dies loses the
/// lease when it expires, and another takes over — so an occurrence can be
/// delayed by the lease TTL, never duplicated and never dropped.
///
/// ```no_run
/// use std::sync::Arc;
///
/// use moso_jobs::{JobRegistry, Jobs, Scheduler};
/// use moso_kv::Kv;
///
/// fn build(jobs: Jobs, registry: Arc<JobRegistry>, kv: Kv) -> Scheduler {
///     Scheduler::new(jobs, registry, kv)
/// }
/// ```
pub struct Scheduler {
    /// Where occurrences are enqueued.
    jobs: crate::Jobs,
    /// What to run.
    registry: std::sync::Arc<crate::JobRegistry>,
    /// How a leader is chosen.
    election: Election,
    /// How long a lease lasts.
    lease: Duration,
    /// How often to look for due occurrences.
    tick: Duration,
    /// Which schedules this process currently leads.
    held: std::sync::Arc<std::sync::RwLock<std::collections::BTreeSet<ScheduleId>>>,
    /// Whether the first election has finished, so `/readyz` can gate on it.
    resolved: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Who this process is, in the durable record of who fired what.
    id: crate::WorkerId,
}

/// The default leadership lease.
///
/// The upper bound on how late an occurrence can be after a leader dies.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_jobs::schedule::DEFAULT_LEASE, Duration::from_secs(60));
/// ```
pub const DEFAULT_LEASE: Duration = Duration::from_secs(60);

/// How often the scheduler looks for due occurrences.
///
/// ```
/// use std::time::Duration;
///
/// assert_eq!(moso_jobs::schedule::DEFAULT_TICK, Duration::from_secs(5));
/// ```
pub const DEFAULT_TICK: Duration = Duration::from_secs(5);

/// How many missed occurrences one [`Cron::catch_up`] pass replays.
///
/// Sixty is a day and a half of an hourly schedule, two months of a daily one,
/// and one hour of a per-minute one. It is chosen against the outcome that hurts
/// — a scheduler down for a week enqueuing 10,080 rows in one tick — rather than
/// against the outcome that annoys, and [`Cron::catch_up_limit`] moves it.
///
/// ```
/// assert_eq!(moso_jobs::schedule::DEFAULT_CATCH_UP_LIMIT, 60);
/// ```
pub const DEFAULT_CATCH_UP_LIMIT: usize = 60;

/// How far back one catch-up pass will *count* before it stops counting.
///
/// The cap decides how many missed occurrences are replayed; this decides how
/// many are counted for the `WARN` that names the ones dropped. A schedule that
/// missed more than this reports "at least" rather than an exact number,
/// because walking a per-minute expression back through a year of downtime is
/// half a million searches nobody is waiting for.
const MAX_CATCH_UP_SCAN: usize = 10_000;

impl Scheduler {
    /// A scheduler over `registry`, electing through `kv`.
    ///
    /// The lease lives in the KV store rather than in a table because the
    /// scheduler must work for a deployment whose queue is Redis and whose
    /// database is elsewhere. A `FailureMode::Fail` namespace: losing the lease
    /// store must stop the scheduler, not make every pod think it is the leader.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobRegistry, Jobs, Scheduler};
    /// # use moso_kv::Kv;
    /// # fn f(j: Jobs, r: Arc<JobRegistry>, k: Kv) { let _ = Scheduler::new(j, r, k); }
    /// ```
    #[must_use]
    pub fn new(
        jobs: crate::Jobs,
        registry: std::sync::Arc<crate::JobRegistry>,
        kv: moso_kv::Kv,
    ) -> Self {
        Self {
            jobs,
            registry,
            election: Election::Lease(kv),
            lease: DEFAULT_LEASE,
            tick: DEFAULT_TICK,
            held: std::sync::Arc::default(),
            resolved: std::sync::Arc::default(),
            id: crate::WorkerId::local(),
        }
    }

    /// Override the identity this process records against the schedules it
    /// fires.
    ///
    /// The hostname by default, exactly as a [`Worker`](crate::Worker) does, so
    /// `GET /_jobs/schedules` names a pod an operator can `kubectl logs`.
    ///
    /// ```no_run
    /// # use moso_jobs::{Scheduler, WorkerId};
    /// # fn f(s: Scheduler) { let _ = s.with_id(WorkerId::new("scheduler-0")); }
    /// ```
    #[must_use]
    pub fn with_id(mut self, id: crate::WorkerId) -> Self {
        self.id = id;
        self
    }

    /// This process's identity in the durable schedule record.
    ///
    /// ```no_run
    /// # use moso_jobs::{Scheduler, WorkerId};
    /// # fn f(s: &Scheduler) { let _: &WorkerId = s.id(); }
    /// ```
    #[must_use]
    pub fn id(&self) -> &crate::WorkerId {
        &self.id
    }

    /// Elect through a PostgreSQL advisory lock instead of a lease.
    ///
    /// What `docs/03-batteries/32-jobs.md` specifies. An advisory lock is held
    /// by a *session*, so a process that dies releases it immediately rather
    /// than after a TTL — the occurrence is picked up by the next process on the
    /// next tick instead of after a lease. The cost is one PostgreSQL connection
    /// per schedule this process leads, held for as long as it leads it.
    ///
    /// ```no_run
    /// # use std::sync::Arc;
    /// # use moso_jobs::{JobRegistry, Jobs, Scheduler};
    /// # use moso_kv::Kv;
    /// # use moso_orm::Db;
    /// # fn f(j: Jobs, r: Arc<JobRegistry>, k: Kv, db: Db) {
    /// let _ = Scheduler::new(j, r, k).advisory_lock(db);
    /// # }
    /// ```
    #[must_use]
    pub fn advisory_lock(mut self, db: moso_orm::Db) -> Self {
        self.election = Election::Advisory(db);
        self
    }

    /// How long a leadership lease lasts. Default 60 seconds.
    ///
    /// The upper bound on how late an occurrence can be after a leader dies.
    /// Ignored under [`advisory_lock`](Scheduler::advisory_lock), which has no
    /// TTL to expire.
    ///
    /// ```no_run
    /// # use moso_jobs::Scheduler;
    /// # fn f(s: Scheduler) { let _ = s.lease(std::time::Duration::from_secs(30)); }
    /// ```
    #[must_use]
    pub fn lease(mut self, lease: Duration) -> Self {
        self.lease = lease.max(Duration::from_secs(1));
        self
    }

    /// How often the scheduler looks for due occurrences. Default 5 seconds.
    ///
    /// ```no_run
    /// # use moso_jobs::Scheduler;
    /// # fn f(s: Scheduler) { let _ = s.tick(std::time::Duration::from_secs(1)); }
    /// ```
    #[must_use]
    pub fn tick(mut self, tick: Duration) -> Self {
        self.tick = tick.max(Duration::from_millis(100));
        self
    }

    /// Whether the first election has finished.
    ///
    /// What `/readyz` gates on, so a rolling deploy never reports every pod
    /// ready while none of them has established who runs the nightly job.
    ///
    /// ```no_run
    /// # use moso_jobs::Scheduler;
    /// # fn f(s: &Scheduler) { let _: bool = s.is_resolved(); }
    /// ```
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        self.resolved.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A handle to the readiness flag, for a health check that outlives this
    /// value.
    ///
    /// ```no_run
    /// # use moso_jobs::Scheduler;
    /// # fn f(s: &Scheduler) { let _ = s.readiness(); }
    /// ```
    #[must_use]
    pub fn readiness(&self) -> SchedulerReadiness {
        SchedulerReadiness {
            resolved: std::sync::Arc::clone(&self.resolved),
        }
    }

    /// A handle to what this process leads, for a dashboard that outlives it.
    ///
    /// The honest half of `leader_here`: mounted in the process that runs the
    /// scheduler it answers `true` or `false`, and everywhere else the field is
    /// absent rather than a `false` that means "nobody asked me".
    ///
    /// ```no_run
    /// # use moso_jobs::Scheduler;
    /// # fn f(s: &Scheduler) { let _ = s.leadership(); }
    /// ```
    #[must_use]
    pub fn leadership(&self) -> SchedulerLeadership {
        SchedulerLeadership {
            held: std::sync::Arc::clone(&self.held),
        }
    }

    /// Run until the shutdown signal fires.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the lease store
    /// cannot be reached at startup. Failures *during* the loop are logged and
    /// retried — a scheduler that exits on a transient error is a scheduler
    /// that stops scheduling.
    ///
    /// ```no_run
    /// # use moso_core::shutdown::Signal;
    /// # use moso_jobs::Scheduler;
    /// # async fn f(s: Scheduler, sig: Signal) -> moso_jobs::Result { s.run(sig).await }
    /// ```
    pub async fn run(self, shutdown: moso_core::shutdown::Signal) -> Result {
        if self.registry.schedules().is_empty() {
            // Nothing to lead. Resolving immediately is right: a pod with no
            // schedules must not hold `/readyz` open waiting for an election
            // that will never happen.
            self.resolved
                .store(true, std::sync::atomic::Ordering::SeqCst);
            tracing::debug!(target: "moso::jobs", "no schedules; the scheduler has nothing to do");
            shutdown.recv().await;
            return Ok(());
        }

        // Prove the lease store is reachable before claiming to be a scheduler.
        self.probe().await?;

        let mut state: std::collections::BTreeMap<ScheduleId, State> =
            std::collections::BTreeMap::new();
        let now = Utc::now();
        for schedule in self.registry.schedules() {
            let next = schedule.next_after(now).ok().flatten();
            state.insert(
                schedule.id().clone(),
                State {
                    next,
                    running: None,
                },
            );
        }

        tracing::info!(
            target: "moso::jobs",
            schedules = self.registry.schedules().len(),
            election = ?self.election,
            "the scheduler is running"
        );

        let mut guards: std::collections::BTreeMap<ScheduleId, Guard> =
            std::collections::BTreeMap::new();

        while !shutdown.is_shutting_down() {
            self.pass(&mut state, &mut guards).await;
            self.resolved
                .store(true, std::sync::atomic::Ordering::SeqCst);

            tokio::select! {
                () = tokio::time::sleep(self.tick) => {}
                () = shutdown.recv() => break,
            }
        }

        // Give the leases back rather than making the next process wait out a
        // TTL for a leader that has already left.
        for (_, guard) in guards {
            guard.release().await;
        }
        self.held
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        Ok(())
    }

    /// One tick: renew or take each lease, then enqueue what is due.
    async fn pass(
        &self,
        state: &mut std::collections::BTreeMap<ScheduleId, State>,
        guards: &mut std::collections::BTreeMap<ScheduleId, Guard>,
    ) {
        let now = Utc::now();
        for schedule in self.registry.schedules() {
            if schedule.error().is_some() {
                // Reported at boot; skipped here rather than logged every tick.
                continue;
            }
            let id = schedule.id().clone();

            // Renew first: a leader that cannot renew must stop leading before
            // anything else takes the lease.
            let leading = match guards.get(&id) {
                Some(guard) => match guard.renew().await {
                    Ok(true) => true,
                    Ok(false) | Err(_) => {
                        tracing::warn!(
                            target: "moso::jobs",
                            schedule = %id,
                            "lost the leadership lease; another process will take over"
                        );
                        guards.remove(&id);
                        false
                    }
                },
                None => match self.acquire(&id).await {
                    Ok(Some(guard)) => {
                        tracing::info!(
                            target: "moso::jobs",
                            schedule = %id,
                            "elected leader"
                        );
                        guards.insert(id.clone(), guard);
                        true
                    }
                    Ok(None) => false,
                    Err(error) => {
                        tracing::warn!(
                            target: "moso::jobs",
                            schedule = %id,
                            error = %error.chain(),
                            "could not run the election; will try again"
                        );
                        false
                    }
                },
            };

            {
                let mut held = self
                    .held
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if leading {
                    held.insert(id.clone());
                } else {
                    held.remove(&id);
                }
            }

            if !leading {
                continue;
            }

            let Some(entry) = state.get_mut(&id) else {
                continue;
            };
            let Some(due) = entry.next else {
                continue;
            };
            if due > now {
                continue;
            }

            let plan = self.plan(schedule, due, now);
            entry.next = plan.next;

            if plan.due.is_empty() {
                tracing::info!(
                    target: "moso::jobs",
                    schedule = %id,
                    "skipped an occurrence missed while nothing was running (catch_up is off)"
                );
                continue;
            }
            // Overlap is asked once per pass, not once per occurrence: a
            // catch-up replay is a deliberate backfill of occurrences that are
            // all in the past, and asking again for each of them would skip
            // every one after the first — which is the behaviour the cap exists
            // to replace.
            if schedule.overlap() != Overlap::Allow
                && let Some(previous) = entry.running
                && self.previous_is_unfinished(schedule, previous).await
            {
                if schedule.overlap() == Overlap::Skip {
                    tracing::info!(
                        target: "moso::jobs",
                        schedule = %id,
                        previous = %previous,
                        "this schedule's previous occurrence has not finished; skipping"
                    );
                    continue;
                }
                tracing::info!(
                    target: "moso::jobs",
                    schedule = %id,
                    previous = %previous,
                    "this schedule's previous occurrence has not finished; enqueuing anyway \
                     (Overlap::Queue)"
                );
            }

            if plan.dropped > 0 {
                // AGENTS.md forbids a silent truncation, and this one is
                // load-bearing: an operator reading "the nightly report ran" has
                // to know that six nights of it did not.
                tracing::warn!(
                    target: "moso::jobs",
                    schedule = %id,
                    job = schedule.job(),
                    dropped = plan.dropped,
                    at_least = plan.dropped_is_a_floor,
                    replayed = plan.due.len(),
                    limit = schedule.catch_up_limit(),
                    "catch-up dropped missed occurrences to stay under the limit; raise \
                     `catch_up_limit` if every period has to be accounted for"
                );
            }

            for occurrence in plan.due {
                if let Err(error) = self.enqueue_occurrence(schedule, entry, occurrence).await {
                    tracing::warn!(
                        target: "moso::jobs",
                        schedule = %id,
                        error = %error.chain(),
                        "could not enqueue an occurrence"
                    );
                    break;
                }
            }
        }
    }

    /// Which occurrences this pass owes, and where the cursor lands.
    ///
    /// Split out of [`pass`](Scheduler::pass) because it is the whole of the
    /// catch-up decision and it is pure: given "the occurrence that came due"
    /// and "now", it answers with the occurrences to enqueue, how many were
    /// dropped to stay under the cap, and the next occurrence to wait for.
    fn plan(&self, schedule: &Schedule, due: DateTime<Utc>, now: DateTime<Utc>) -> Plan {
        let limit = schedule.catch_up_limit();

        if !schedule.catches_up() {
            // With catch-up off, an occurrence more than four ticks late is one
            // nothing was running for, and it is skipped rather than replayed:
            // a service down for a day must not run twenty-four hourly reports
            // on the way back up.
            let stale = (now - due) > chrono::Duration::from_std(self.tick * 4).unwrap_or_default();
            return Plan {
                due: if stale { Vec::new() } else { vec![due] },
                dropped: 0,
                dropped_is_a_floor: false,
                next: schedule.next_after(now).ok().flatten(),
            };
        }

        // Every occurrence between `due` and now, oldest first — bounded twice:
        // by the cap, which decides how many are *replayed*, and by the scan
        // bound, which decides how many are *counted* for the warning.
        let mut occurrences = vec![due];
        let mut cursor = due;
        let mut scanned = 0;
        let mut dropped_is_a_floor = false;
        while let Some(next) = schedule.next_after(cursor).ok().flatten() {
            if next > now {
                break;
            }
            occurrences.push(next);
            cursor = next;
            scanned += 1;
            if scanned >= MAX_CATCH_UP_SCAN {
                dropped_is_a_floor = true;
                break;
            }
        }

        let next = schedule.next_after(cursor.max(now)).ok().flatten();
        let dropped = occurrences.len().saturating_sub(limit);
        if dropped > 0 {
            // The newest occurrences are the ones worth running: a report for
            // last night is more use than a report for six nights ago, and the
            // dropped ones are named in the log rather than vanishing.
            occurrences.drain(..dropped);
        }
        Plan {
            due: occurrences,
            dropped,
            dropped_is_a_floor,
            next,
        }
    }

    /// Enqueue one occurrence.
    ///
    /// The overlap decision belongs to [`pass`](Scheduler::pass), which asks it
    /// once per pass; this enqueues what that decided to enqueue.
    async fn enqueue_occurrence(
        &self,
        schedule: &Schedule,
        entry: &mut State,
        occurrence: DateTime<Utc>,
    ) -> Result<bool> {
        let Some(registered) = self.registry.get(schedule.job()) else {
            // Reported at boot; nothing useful to do here.
            return Ok(false);
        };

        let jitter = crate::rng::duration_below(schedule.jitter());
        let row = crate::QueuedJob {
            id: crate::JobId::new(),
            name: schedule.job().to_owned(),
            queue: schedule.queue().to_owned(),
            payload: schedule.payload().clone(),
            state: crate::JobState::Ready,
            priority: schedule.priority(),
            attempt: 1,
            retry: registered.retry(),
            run_at: Utc::now() + chrono::Duration::from_std(jitter).unwrap_or_default(),
            enqueued_at: Utc::now(),
            // The occurrence's own key, and the occurrence is the one that came
            // due rather than the one coming next: two schedulers that both
            // believed they were the leader for one tick produce one row, and a
            // catch-up pass replaying six missed nights produces six.
            unique_key: Some(format!(
                "schedule:{}:{}",
                schedule.id(),
                occurrence.timestamp()
            )),
            trace_parent: None,
            // A schedule fires from the scheduler itself, not from a request, so
            // there is no enqueueing actor to attribute the occurrence to.
            actor: None,
            last_error: None,
            locked_by: None,
            locked_until: None,
        };
        let id = row.id;

        crate::metrics::enqueued(schedule.job(), schedule.queue());
        self.jobs.queue().push(row).await?;
        entry.running = Some(id);
        tracing::info!(
            target: "moso::jobs",
            schedule = %schedule.id(),
            job = schedule.job(),
            occurrence = %occurrence,
            id = %id,
            "enqueued a scheduled occurrence"
        );
        self.record(schedule).await;
        Ok(true)
    }

    /// Whether the row this schedule enqueued last time is still unfinished.
    ///
    /// Scoped to the schedule's **own** previous occurrence, by identifier.
    /// Asking the queue whether anything is running would answer a different
    /// question — a schedule sharing a busy queue with unrelated work would skip
    /// occurrences it did not need to.
    ///
    /// "Unfinished" and not "running": a previous occurrence still sitting
    /// *ready* on a backed-up queue has not done its work either, and enqueuing
    /// another is exactly the accumulation [`Overlap::Skip`] exists to prevent.
    ///
    /// A backend that cannot look a row up by identifier answers "finished", so
    /// [`Overlap::Skip`] degrades to [`Overlap::Allow`] rather than to a
    /// schedule that silently stops firing. It says so at `WARN`, once per
    /// occurrence rather than once per tick.
    async fn previous_is_unfinished(&self, schedule: &Schedule, previous: crate::JobId) -> bool {
        match self.jobs.queue().find(previous).await {
            Ok(Some(row)) => row.state.is_active(),
            // Gone means finished and swept, which is finished.
            Ok(None) => false,
            Err(error) => {
                tracing::warn!(
                    target: "moso::jobs",
                    schedule = %schedule.id(),
                    backend = self.jobs.queue().name(),
                    error = %error.chain(),
                    "the queue cannot look a job up by id, so `Overlap::Skip` cannot be scoped \
                     to this schedule; enqueuing the occurrence anyway"
                );
                false
            }
        }
    }

    /// Write down that this process fired `schedule`, for every other process.
    ///
    /// Bookkeeping, not delivery: a backend that keeps no schedule state logs
    /// once at `DEBUG` and the dashboard reports `last_run: null`, which is the
    /// truth. Failing the occurrence over it would trade a missing dashboard
    /// field for a job that did not run.
    async fn record(&self, schedule: &Schedule) {
        let run = ScheduleRun::new(
            schedule.id().clone(),
            schedule.job(),
            self.id.clone(),
            Utc::now(),
        );
        if let Err(error) = self.jobs.queue().record_schedule_run(&run).await {
            tracing::debug!(
                target: "moso::jobs",
                schedule = %schedule.id(),
                backend = self.jobs.queue().name(),
                error = %error.chain(),
                "could not record the occurrence; `/_jobs/schedules` will show no last run"
            );
        }
    }

    /// Take the lease for `id`, if nobody else has it.
    async fn acquire(&self, id: &ScheduleId) -> Result<Option<Guard>> {
        match &self.election {
            Election::Lease(kv) => {
                let guard = kv.try_lock(id.as_str(), self.lease).await?;
                Ok(guard.map(Guard::Lease))
            }
            Election::Advisory(db) => {
                let key = moso_orm::db::AdvisoryKey::hashed(id.as_str());
                let lock = db.try_advisory_lock(key).await?;
                Ok(lock.map(Guard::Advisory))
            }
        }
    }

    /// Prove the election store is reachable.
    async fn probe(&self) -> Result {
        match &self.election {
            Election::Lease(kv) => {
                let status = kv.store().health().await;
                if status.is_up() {
                    Ok(())
                } else {
                    Err(crate::Error::unavailable(
                        "scheduler lease store",
                        status.render(),
                    ))
                }
            }
            Election::Advisory(db) => db.ping().await.map_err(crate::Error::from),
        }
    }

    /// Whether this process currently holds the lease for `schedule`.
    ///
    /// For the dashboard, and for a test that wants to assert exactly one
    /// leader across ten processes.
    ///
    /// ```no_run
    /// # use moso_jobs::{ScheduleId, Scheduler};
    /// # fn f(s: &Scheduler, id: &ScheduleId) { let _: bool = s.is_leader(id); }
    /// ```
    #[must_use]
    pub fn is_leader(&self, schedule: &ScheduleId) -> bool {
        self.held
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(schedule)
    }

    /// Try to become the leader for `schedule`, once, and hold it.
    ///
    /// What a test uses to prove that ten processes elect one leader without
    /// running ten schedulers. Returns whether this process won; the lease is
    /// held until the returned guard is dropped or released.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable) when the election store
    /// cannot be reached.
    ///
    /// ```no_run
    /// # use moso_jobs::{ScheduleId, Scheduler};
    /// # async fn f(s: &Scheduler, id: &ScheduleId) -> moso_jobs::Result<bool> {
    /// Ok(s.try_lead(id).await?.is_some())
    /// # }
    /// ```
    pub async fn try_lead(&self, schedule: &ScheduleId) -> Result<Option<Leadership>> {
        let Some(guard) = self.acquire(schedule).await? else {
            return Ok(None);
        };
        // Recorded in the same set `pass` writes, so `is_leader` and the
        // dashboard's `leader_here` answer the same way however leadership was
        // taken. Without this, a process holding a lease it took by hand would
        // report that it leads nothing.
        self.held
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(schedule.clone());
        Ok(Some(Leadership {
            guard: Some(guard),
            held: std::sync::Arc::clone(&self.held),
            schedule: schedule.clone(),
        }))
    }
}

/// One schedule's place in the clock.
struct State {
    /// When it next fires.
    next: Option<DateTime<Utc>>,
    /// The row the last occurrence enqueued, for the overlap policy.
    ///
    /// The **identifier**, not a timestamp: the overlap question is "is this
    /// schedule's own previous occurrence still going", and only the row can
    /// answer it.
    running: Option<crate::JobId>,
}

/// What one pass owes a schedule: which occurrences, and where the cursor lands.
struct Plan {
    /// The occurrences to enqueue, oldest first.
    due: Vec<DateTime<Utc>>,
    /// How many were dropped to stay under the catch-up limit.
    dropped: usize,
    /// Whether `dropped` is a floor rather than an exact count, because the
    /// backlog was longer than the scan bound.
    dropped_is_a_floor: bool,
    /// The next occurrence after now.
    next: Option<DateTime<Utc>>,
}

/// Which schedules a process currently leads, as a handle that outlives it.
///
/// The dashboard is served by whichever process the request reached, so
/// `leader_here` can only be answered by the process that runs the scheduler.
/// This is the seam: hand it to
/// [`Dashboard::scheduler`](crate::dashboard::Dashboard::scheduler) in the
/// process that runs one, and `GET /_jobs/schedules` answers `true` or `false`
/// there and `null` — "this process runs no scheduler" — everywhere else.
///
/// ```
/// use moso_jobs::{ScheduleId, schedule::SchedulerLeadership};
///
/// let none = SchedulerLeadership::none();
/// assert!(!none.leads(&ScheduleId::new("j", "0 3 * * *")));
/// ```
#[derive(Clone, Debug, Default)]
pub struct SchedulerLeadership {
    /// Shared with the scheduler that writes it.
    held: std::sync::Arc<std::sync::RwLock<std::collections::BTreeSet<ScheduleId>>>,
}

impl SchedulerLeadership {
    /// A handle that leads nothing, for a process with no scheduler.
    ///
    /// ```
    /// # use moso_jobs::schedule::SchedulerLeadership;
    /// let _ = SchedulerLeadership::none();
    /// ```
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// Whether the process this came from currently holds `schedule`'s lease.
    ///
    /// ```
    /// # use moso_jobs::{ScheduleId, schedule::SchedulerLeadership};
    /// assert!(!SchedulerLeadership::none().leads(&ScheduleId::new("j", "@daily")));
    /// ```
    #[must_use]
    pub fn leads(&self, schedule: &ScheduleId) -> bool {
        self.held
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(schedule)
    }
}

/// A held leadership lease, whichever mechanism holds it.
enum Guard {
    /// A key-value lease.
    Lease(moso_kv::LockGuard),
    /// A PostgreSQL advisory lock.
    Advisory(moso_orm::db::AdvisoryLock),
}

impl Guard {
    /// Extend the lease. `false` means it was lost.
    async fn renew(&self) -> Result<bool> {
        match self {
            Self::Lease(lock) => match lock.renew().await {
                Ok(()) => Ok(true),
                // A lost lease is not an error: another process has it, and
                // this one has to stop leading rather than stop running.
                Err(moso_kv::Error::LockLost { .. }) => Ok(false),
                Err(error) => Err(error.into()),
            },
            // A session-level advisory lock is held for as long as the
            // connection lives; there is nothing to renew.
            Self::Advisory(lock) => Ok(lock.is_held()),
        }
    }

    /// Hand it back.
    async fn release(self) {
        match self {
            Self::Lease(lock) => {
                let _ = lock.release().await;
            }
            Self::Advisory(lock) => {
                let _ = lock.unlock().await;
            }
        }
    }
}

/// Leadership of one schedule, held until this is dropped.
///
/// ```no_run
/// # use moso_jobs::schedule::Leadership;
/// # async fn f(l: Leadership) { l.resign().await; }
/// ```
pub struct Leadership {
    /// The lease. `None` after [`Leadership::resign`].
    guard: Option<Guard>,
    /// The scheduler's own record of what it leads, so
    /// [`Scheduler::is_leader`] and this agree.
    held: std::sync::Arc<std::sync::RwLock<std::collections::BTreeSet<ScheduleId>>>,
    /// Which schedule, so dropping this can take it back out of that record.
    schedule: ScheduleId,
}

impl Leadership {
    /// Give the leadership back now, rather than waiting for the lease.
    ///
    /// ```no_run
    /// # use moso_jobs::schedule::Leadership;
    /// # async fn f(l: Leadership) { l.resign().await; }
    /// ```
    pub async fn resign(mut self) {
        if let Some(guard) = self.guard.take() {
            guard.release().await;
        }
    }
}

impl Drop for Leadership {
    /// Stop *claiming* leadership even when nobody resigned.
    ///
    /// Releasing the lease itself needs an `await` and a destructor has none,
    /// so a dropped guard leaves the lease to expire on its TTL — which is the
    /// documented worst case. What must not survive the drop is the process
    /// telling `/_jobs/schedules` that it still leads a schedule it stopped
    /// leading, and that part is a synchronous set removal.
    fn drop(&mut self) {
        self.held
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.schedule);
    }
}

impl core::fmt::Debug for Leadership {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Leadership").finish_non_exhaustive()
    }
}

/// Whether the scheduler has finished its first election.
///
/// Handed to a health check, so `/readyz` can gate on leader-election-resolved
/// and a rolling deploy never has zero schedulers.
///
/// ```no_run
/// # use moso_jobs::schedule::SchedulerReadiness;
/// # fn f(r: &SchedulerReadiness) { let _: bool = r.is_resolved(); }
/// ```
#[derive(Clone, Debug)]
pub struct SchedulerReadiness {
    /// Shared with the scheduler that will set it.
    resolved: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SchedulerReadiness {
    /// A flag nothing has resolved yet.
    ///
    /// For a process that runs no scheduler and still wants the health check
    /// mounted; call [`resolve`](SchedulerReadiness::resolve) at boot.
    ///
    /// ```
    /// use moso_jobs::schedule::SchedulerReadiness;
    ///
    /// let readiness = SchedulerReadiness::pending();
    /// assert!(!readiness.is_resolved());
    /// readiness.resolve();
    /// assert!(readiness.is_resolved());
    /// ```
    #[must_use]
    pub fn pending() -> Self {
        Self {
            resolved: std::sync::Arc::default(),
        }
    }

    /// Whether the first election has finished.
    ///
    /// ```
    /// # use moso_jobs::schedule::SchedulerReadiness;
    /// assert!(!SchedulerReadiness::pending().is_resolved());
    /// ```
    #[must_use]
    pub fn is_resolved(&self) -> bool {
        self.resolved.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Declare the election finished.
    ///
    /// ```
    /// # use moso_jobs::schedule::SchedulerReadiness;
    /// let readiness = SchedulerReadiness::pending();
    /// readiness.resolve();
    /// assert!(readiness.is_resolved());
    /// ```
    pub fn resolve(&self) {
        self.resolved
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl core::fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Scheduler")
            .field("schedules", &self.registry.schedules().len())
            .field("election", &self.election)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{JobCtx, JobRegistry, Jobs};

    struct Cleanup;
    impl Job for Cleanup {
        type Args = ();
        const NAME: &'static str = "nightly_cleanup";
        async fn run(_args: (), _ctx: JobCtx) -> Result {
            Ok(())
        }
    }

    /// Two schedules of the same job must elect leaders independently, so the
    /// identifier has to distinguish their expressions.
    #[test]
    fn two_schedules_of_one_job_get_different_keys() {
        let nightly: Schedule = Cron::new::<Cleanup>("0 3 * * *", ()).into();
        let hourly: Schedule = Cron::new::<Cleanup>("0 * * * *", ()).into();
        assert_ne!(nightly.id(), hourly.id());
        assert!(nightly.id().as_str().starts_with("nightly_cleanup-"));
    }

    /// The identifier becomes a key-value key, so it must not contain anything
    /// a key scheme reserves.
    #[test]
    fn the_identifier_is_safe_as_a_key() {
        let id = ScheduleId::new("job", "*/5 * * * *");
        assert!(!id.as_str().contains(':'));
        assert!(!id.as_str().contains(' '));
        assert!(!id.as_str().contains('*'));
    }

    /// A cron schedule computes its next occurrence in its own timezone.
    #[test]
    fn a_cron_schedule_computes_its_next_occurrence() {
        use chrono::TimeZone as _;

        let schedule: Schedule = Cron::new::<Cleanup>("0 3 * * *", ())
            .timezone("Europe/Rome")
            .into();
        assert!(schedule.error().is_none(), "{:?}", schedule.error());
        assert_eq!(schedule.timezone(), "Europe/Rome");

        let from = Utc
            .with_ymd_and_hms(2026, 1, 1, 12, 0, 0)
            .single()
            .expect("a real time");
        let next = schedule
            .next_after(from)
            .expect("parses")
            .expect("there is one");
        // 03:00 CET is 02:00 UTC.
        assert_eq!(
            next,
            Utc.with_ymd_and_hms(2026, 1, 2, 2, 0, 0)
                .single()
                .expect("a real time")
        );
    }

    /// An interval schedule measures from now, so a job that runs long does not
    /// immediately re-enqueue.
    #[test]
    fn an_interval_schedule_measures_from_the_previous_run() {
        let schedule: Schedule = Every::new::<Cleanup>(Duration::from_secs(300), ()).into();
        let now = Utc::now();
        let next = schedule
            .next_after(now)
            .expect("valid")
            .expect("there is one");
        assert_eq!((next - now).num_seconds(), 300);
        assert_eq!(schedule.expression(), "every 5m");
    }

    /// A malformed expression is held, not returned, so a registry reads as a
    /// list of declarations and the boot report has every problem at once.
    #[test]
    fn a_bad_expression_is_held_until_boot() {
        let schedule: Schedule = Cron::new::<Cleanup>("not a cron expression", ()).into();
        let error = schedule.error().expect("held");
        assert!(error.contains("field"), "{error}");
        assert!(schedule.next_after(Utc::now()).is_err());
    }

    /// A timezone nobody has heard of is held the same way, with a message that
    /// says what a name looks like.
    #[test]
    fn an_unknown_timezone_is_held_until_boot() {
        let schedule: Schedule = Cron::new::<Cleanup>("0 3 * * *", ())
            .timezone("Middle/Earth")
            .into();
        let error = schedule.error().expect("held");
        assert!(error.contains("is not an IANA timezone"), "{error}");
        assert!(error.contains("Europe/Rome"), "{error}");
    }

    /// An interval of zero would enqueue as fast as the scheduler ticks.
    #[test]
    fn a_zero_interval_is_held_until_boot() {
        let schedule: Schedule = Every::new::<Cleanup>(Duration::ZERO, ()).into();
        assert!(schedule.error().expect("held").contains("is zero"));
    }

    /// The documented defaults, since every one of them is a decision somebody
    /// would otherwise have to discover.
    #[test]
    fn the_defaults_are_the_safe_ones() {
        let schedule: Schedule = Cron::new::<Cleanup>("0 3 * * *", ()).into();
        assert_eq!(schedule.overlap(), Overlap::Skip);
        assert!(!schedule.catches_up(), "catching up is off by default");
        assert_eq!(schedule.timezone(), "UTC");
        assert_eq!(schedule.jitter(), Duration::ZERO);
        assert_eq!(schedule.priority(), Priority::Normal);
    }

    /// The builders set what they say they set.
    #[test]
    fn the_builders_set_what_they_name() {
        let schedule: Schedule = Cron::new::<Cleanup>("0 3 * * *", ())
            .catch_up(true)
            .overlap(Overlap::Queue)
            .jitter(Duration::from_secs(30))
            .priority(Priority::Low)
            .into();
        assert!(schedule.catches_up());
        assert_eq!(schedule.overlap(), Overlap::Queue);
        assert_eq!(schedule.jitter(), Duration::from_secs(30));
        assert_eq!(schedule.priority(), Priority::Low);

        let every: Schedule = Every::new::<Cleanup>(Duration::from_secs(60), ())
            .overlap(Overlap::Allow)
            .jitter(Duration::from_secs(5))
            .priority(Priority::High)
            .into();
        assert_eq!(every.overlap(), Overlap::Allow);
        assert_eq!(every.jitter(), Duration::from_secs(5));
        assert_eq!(every.priority(), Priority::High);
    }

    /// Acceptance criterion 5, without ten processes: ten *contenders* for one
    /// schedule, and exactly one of them holds the lease.
    #[tokio::test]
    async fn ten_contenders_elect_exactly_one_leader() {
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

        // The leader resigning lets the next one in — which is what makes a
        // rolling deploy work rather than leaving the schedule leaderless.
        for leadership in held {
            leadership.resign().await;
        }
        let scheduler = scheduler(kv);
        assert!(
            scheduler.try_lead(&id).await.expect("up").is_some(),
            "a resigned lease must be takeable"
        );
    }

    /// A pod with no schedules must not hold `/readyz` open waiting for an
    /// election that will never happen.
    #[tokio::test]
    async fn a_scheduler_with_nothing_to_do_is_immediately_ready() {
        let kv = moso_kv::Kv::in_memory("scheduler-empty").expect("in-memory kv");
        let scheduler = scheduler(kv);
        let readiness = scheduler.readiness();
        assert!(!readiness.is_resolved());

        let shutdown = moso_core::shutdown::Signal::new();
        shutdown.trigger();
        scheduler.run(shutdown).await.expect("clean exit");
        assert!(readiness.is_resolved());
    }

    fn scheduler(kv: moso_kv::Kv) -> Scheduler {
        let jobs = Jobs::new(
            std::sync::Arc::new(crate::backend::MemoryQueue::new()),
            std::sync::Arc::new(JobRegistry::new().register::<Cleanup>()),
        );
        let registry = jobs.shared_registry();
        Scheduler::new(jobs, registry, kv)
    }

    /// An hour of the day, in UTC, for the catch-up arithmetic.
    fn at(hour: u32) -> DateTime<Utc> {
        use chrono::TimeZone as _;

        Utc.with_ymd_and_hms(2026, 1, 1, hour, 0, 0)
            .single()
            .expect("a real time")
    }

    // ── catching up ─────────────────────────────────────────────────────────

    /// The gap this closes: catching up used to replay one occurrence and
    /// silently drop the rest. Six hours of downtime on an hourly schedule is
    /// seven occurrences, and all seven are now enqueued.
    #[test]
    fn catching_up_replays_every_missed_occurrence() {
        let scheduler = scheduler(moso_kv::Kv::in_memory("catch-up").expect("kv"));
        let schedule: Schedule = Cron::new::<Cleanup>("0 * * * *", ()).catch_up(true).into();

        let plan = scheduler.plan(&schedule, at(0), at(6));
        assert_eq!(plan.due.len(), 7, "00:00 through 06:00 inclusive");
        assert_eq!(plan.due[0], at(0));
        assert_eq!(plan.due[6], at(6));
        assert_eq!(plan.dropped, 0);
        assert_eq!(plan.next, Some(at(7)), "the cursor lands after now");
    }

    /// The cap is what stops a week of downtime becoming ten thousand rows in
    /// one tick, and the newest occurrences are the ones worth running.
    #[test]
    fn the_catch_up_limit_drops_the_oldest_and_counts_them() {
        let scheduler = scheduler(moso_kv::Kv::in_memory("catch-up-limit").expect("kv"));
        let schedule: Schedule = Cron::new::<Cleanup>("0 * * * *", ())
            .catch_up(true)
            .catch_up_limit(3)
            .into();

        let plan = scheduler.plan(&schedule, at(0), at(6));
        assert_eq!(plan.due, vec![at(4), at(5), at(6)]);
        assert_eq!(plan.dropped, 4, "and the warning names four");
        assert!(!plan.dropped_is_a_floor, "four is exact, not a floor");
    }

    /// A cap of one is the behaviour this replaced, still reachable — one
    /// occurrence replayed and the rest reported.
    #[test]
    fn a_catch_up_limit_of_one_is_the_old_behaviour() {
        let scheduler = scheduler(moso_kv::Kv::in_memory("catch-up-one").expect("kv"));
        let schedule: Schedule = Cron::new::<Cleanup>("0 * * * *", ())
            .catch_up(true)
            .catch_up_limit(1)
            .into();

        let plan = scheduler.plan(&schedule, at(0), at(6));
        assert_eq!(plan.due, vec![at(6)]);
        assert_eq!(plan.dropped, 6);
    }

    /// Zero would be `catch_up(false)` spelled confusingly, so it is raised.
    #[test]
    fn a_catch_up_limit_of_zero_still_replays_one() {
        let schedule: Schedule = Cron::new::<Cleanup>("0 * * * *", ())
            .catch_up(true)
            .catch_up_limit(0)
            .into();
        assert_eq!(schedule.catch_up_limit(), 1);
    }

    /// With catch-up off nothing changes: an occurrence nothing was running for
    /// is skipped, and one that is merely a tick late still fires.
    #[test]
    fn with_catch_up_off_a_stale_occurrence_is_still_skipped() {
        let scheduler = scheduler(moso_kv::Kv::in_memory("catch-up-off").expect("kv"));
        let schedule: Schedule = Cron::new::<Cleanup>("0 * * * *", ()).into();
        assert!(!schedule.catches_up());

        assert!(
            scheduler.plan(&schedule, at(0), at(6)).due.is_empty(),
            "six hours late is an occurrence nothing was running for"
        );

        let now = Utc::now();
        let due = now - chrono::Duration::seconds(1);
        assert_eq!(
            scheduler.plan(&schedule, due, now).due,
            vec![due],
            "a second late is just a tick"
        );
    }

    // ── overlap ─────────────────────────────────────────────────────────────

    /// One scheduler over one schedule, and a handle on the queue behind it.
    fn scheduled(
        namespace: &str,
        schedule: Cron,
    ) -> (
        Scheduler,
        std::sync::Arc<crate::backend::MemoryQueue>,
        ScheduleId,
    ) {
        let queue = std::sync::Arc::new(crate::backend::MemoryQueue::new());
        let registry =
            std::sync::Arc::new(JobRegistry::new().register::<Cleanup>().schedule(schedule));
        let id = registry.schedules()[0].id().clone();
        let jobs = Jobs::new(
            std::sync::Arc::clone(&queue) as std::sync::Arc<dyn crate::Queue>,
            std::sync::Arc::clone(&registry),
        );
        let kv = moso_kv::Kv::in_memory(namespace).expect("in-memory kv");
        let scheduler = Scheduler::new(jobs, registry, kv).with_id(crate::WorkerId::new("pod-1"));
        (scheduler, queue, id)
    }

    /// An occurrence that came due `seconds` ago.
    ///
    /// Not [`at`], which is a fixed instant months in the past: with catch-up
    /// off, an occurrence more than four ticks late is one nothing was running
    /// for and the scheduler is right to skip it. Successive occurrences in a
    /// test have to be *seconds* apart rather than milliseconds, because the
    /// deduplication key carries the occurrence's whole-second timestamp — two
    /// occurrences inside one second are one occurrence.
    fn moments_ago(seconds: i64) -> DateTime<Utc> {
        Utc::now() - chrono::Duration::seconds(seconds)
    }

    /// Run one tick with `id` already due, and hand back what it left behind.
    async fn tick(
        scheduler: &Scheduler,
        id: &ScheduleId,
        state: &mut std::collections::BTreeMap<ScheduleId, State>,
        guards: &mut std::collections::BTreeMap<ScheduleId, Guard>,
        due: DateTime<Utc>,
    ) {
        state
            .entry(id.clone())
            .or_insert(State {
                next: None,
                running: None,
            })
            .next = Some(due);
        scheduler.pass(state, guards).await;
    }

    /// Lease and finish whatever is ready on `queue`, as a worker would.
    async fn work_off(queue: &crate::backend::MemoryQueue, queues: &[String]) -> usize {
        let leased = crate::Queue::pull(
            queue,
            queues,
            100,
            Duration::from_secs(30),
            crate::WorkerId::new("worker"),
        )
        .await
        .expect("leased");
        let count = leased.len();
        for (_, lease) in leased {
            crate::Queue::ack(queue, lease).await.expect("finished");
        }
        count
    }

    /// The gap this closes: `Overlap::Skip` used to ask whether the schedule's
    /// *queue* had anything running, so a schedule sharing a queue with
    /// unrelated work skipped occurrences it did not need to.
    #[tokio::test]
    async fn overlap_ignores_unrelated_work_on_the_same_queue() {
        let (scheduler, queue, id) =
            scheduled("overlap-unrelated", Cron::new::<Cleanup>("0 3 * * *", ()));
        let mut state = std::collections::BTreeMap::new();
        let mut guards = std::collections::BTreeMap::new();

        // One occurrence, run to completion, so there is a previous occurrence
        // to be asked about at all.
        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(9)).await;
        assert_eq!(work_off(&queue, &["default".to_owned()]).await, 1);

        // Somebody else's job, leased and running on the same queue.
        crate::Queue::push(
            queue.as_ref(),
            crate::QueuedJob::new("unrelated", "default", serde_json::Value::Null),
        )
        .await
        .expect("pushed");
        let busy = crate::Queue::pull(
            queue.as_ref(),
            &["default".to_owned()],
            10,
            Duration::from_secs(30),
            crate::WorkerId::new("busy"),
        )
        .await
        .expect("leased");
        assert_eq!(busy.len(), 1, "the queue is busy with something else");

        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(5)).await;
        assert_eq!(
            queue.enqueued("nightly_cleanup").len(),
            2,
            "an unrelated running job must not skip this schedule"
        );
    }

    /// And the other half: this schedule's *own* previous occurrence does hold
    /// the next one back, until it finishes.
    #[tokio::test]
    async fn overlap_skips_while_this_schedules_own_occurrence_is_unfinished() {
        let (scheduler, queue, id) =
            scheduled("overlap-own", Cron::new::<Cleanup>("0 3 * * *", ()));
        let mut state = std::collections::BTreeMap::new();
        let mut guards = std::collections::BTreeMap::new();

        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(9)).await;
        assert_eq!(queue.enqueued("nightly_cleanup").len(), 1);

        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(5)).await;
        assert_eq!(
            queue.enqueued("nightly_cleanup").len(),
            1,
            "a previous occurrence still sitting ready has not done its work either"
        );

        assert_eq!(work_off(&queue, &["default".to_owned()]).await, 1);
        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(1)).await;
        assert_eq!(
            queue.enqueued("nightly_cleanup").len(),
            2,
            "and once it finishes the schedule fires again"
        );
    }

    /// `Overlap::Allow` asks nothing, so an unfinished previous occurrence does
    /// not hold anything back.
    #[tokio::test]
    async fn overlap_allow_never_holds_an_occurrence_back() {
        let (scheduler, queue, id) = scheduled(
            "overlap-allow",
            Cron::new::<Cleanup>("0 3 * * *", ()).overlap(Overlap::Allow),
        );
        let mut state = std::collections::BTreeMap::new();
        let mut guards = std::collections::BTreeMap::new();

        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(9)).await;
        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(5)).await;
        assert_eq!(queue.enqueued("nightly_cleanup").len(), 2);
    }

    /// A catch-up pass is a backfill of occurrences that are all in the past,
    /// so the overlap question is asked once for the pass rather than once per
    /// occurrence — which would skip every one after the first.
    #[tokio::test]
    async fn a_catch_up_pass_enqueues_one_row_per_missed_occurrence() {
        let (scheduler, queue, id) = scheduled(
            "catch-up-pass",
            Cron::new::<Cleanup>("0 * * * *", ()).catch_up(true),
        );
        let mut state = std::collections::BTreeMap::new();
        let mut guards = std::collections::BTreeMap::new();

        // Six hours of downtime on an hourly schedule, replayed in one tick.
        let now = Utc::now();
        let due = now - chrono::Duration::hours(6);
        tick(&scheduler, &id, &mut state, &mut guards, due).await;

        let rows = queue.enqueued("nightly_cleanup");
        assert!(
            rows.len() >= 6,
            "every missed occurrence gets a row, got {}",
            rows.len()
        );
        let keys: std::collections::BTreeSet<Option<String>> =
            rows.iter().map(|row| row.unique_key.clone()).collect();
        assert_eq!(
            keys.len(),
            rows.len(),
            "each occurrence carries its own deduplication key: {keys:?}"
        );
    }

    /// Firing records who fired it and when, in the queue every process shares
    /// — which is the only way a dashboard on another pod can answer either
    /// question.
    #[tokio::test]
    async fn firing_records_the_occurrence_in_the_queue_for_every_process() {
        let (scheduler, queue, id) = scheduled("record-run", Cron::new::<Cleanup>("0 3 * * *", ()));
        let mut state = std::collections::BTreeMap::new();
        let mut guards = std::collections::BTreeMap::new();
        tick(&scheduler, &id, &mut state, &mut guards, moments_ago(9)).await;

        let runs = crate::Queue::schedule_runs(queue.as_ref())
            .await
            .expect("the memory queue keeps them");
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].schedule, id);
        assert_eq!(runs[0].job, "nightly_cleanup");
        assert_eq!(runs[0].leader.as_str(), "pod-1");
    }

    /// Taking leadership by hand has to show up in `is_leader`, or a process
    /// reports that it leads nothing while holding the lease.
    #[tokio::test]
    async fn taking_a_lease_by_hand_is_visible_to_is_leader() {
        let kv = moso_kv::Kv::in_memory("scheduler-visible").expect("in-memory kv");
        let scheduler = scheduler(kv);
        let id = ScheduleId::new("nightly_cleanup", "0 3 * * *");
        assert!(!scheduler.is_leader(&id));

        let leadership = scheduler
            .try_lead(&id)
            .await
            .expect("the store is up")
            .expect("nobody else is leading");
        assert!(scheduler.is_leader(&id));
        assert!(scheduler.leadership().leads(&id));

        leadership.resign().await;
        assert!(!scheduler.is_leader(&id), "and resigning takes it back");
    }
}
