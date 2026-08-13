//! The dead-letter queue: what happens to work that could not be done.
//!
//! A job out of retries keeps its payload. That is the whole point — the fix is
//! usually a deploy, and after the deploy the work still needs doing.
//! `moso jobs retry --dlq --job SendWelcomeEmail --since 1h` is the other half.

use chrono::{DateTime, Utc};
use moso_core::BoxFuture;
use serde::{Deserialize, Serialize};

use crate::{JobId, Result};

/// A job that will not be retried.
///
/// ```no_run
/// use moso_jobs::DeadLetter;
///
/// # fn f(d: &DeadLetter) {
/// let _ = &d.last_error;
/// # }
/// ```
#[derive(Clone, Debug, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DeadLetter {
    /// Which job row.
    pub id: JobId,
    /// The job's wire name.
    pub name: String,
    /// Which queue it was on.
    pub queue: String,
    /// The payload, kept intact so the job can be retried after a fix.
    pub payload: serde_json::Value,
    /// How many attempts were made.
    pub attempts: u32,
    /// The last failure's whole error chain.
    pub last_error: String,
    /// When it was first enqueued.
    pub enqueued_at: DateTime<Utc>,
    /// When it gave up.
    pub failed_at: DateTime<Utc>,
    /// The trace context of the request that enqueued it, so the failure joins
    /// to the request that caused it however long ago that was.
    pub trace_parent: Option<String>,
    /// Which worker made the last attempt.
    pub worker: Option<crate::WorkerId>,
    /// The opaque identity of whoever enqueued it, so a buried job can still be
    /// attributed to the subject that scheduled it — which is exactly the
    /// attribution an audit of a failed automated action needs. An identity,
    /// never a credential; decode it with `moso-authz`'s
    /// `ActorIdentity::from_wire`. `#[serde(default)]` so a letter written
    /// before this field existed still decodes.
    #[serde(default)]
    pub actor: Option<String>,
}

/// Which dead letters to act on.
///
/// Every field is optional and they are combined with `AND`. An empty filter
/// matches everything, which is why every bulk operation that takes one also
/// takes an explicit limit.
///
/// ```no_run
/// use moso_jobs::DlqFilter;
///
/// let filter = DlqFilter::new().job("send_welcome_email").since(chrono::Utc::now());
/// let _ = filter;
/// ```
#[derive(Clone, Debug, Default)]
pub struct DlqFilter {
    /// Only this job's wire name.
    job: Option<String>,
    /// Only this queue.
    queue: Option<String>,
    /// Only failures at or after this time.
    since: Option<DateTime<Utc>>,
    /// Only failures before this time.
    until: Option<DateTime<Utc>>,
    /// Only failures whose error chain contains this substring.
    error_contains: Option<String>,
}

impl DlqFilter {
    /// Match everything.
    ///
    /// ```no_run
    /// use moso_jobs::DlqFilter;
    ///
    /// let _ = DlqFilter::new();
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Only this job's wire name.
    ///
    /// ```no_run
    /// # use moso_jobs::DlqFilter;
    /// # fn f(d: DlqFilter) { let _ = d.job("send_welcome_email"); }
    /// ```
    #[must_use]
    pub fn job(mut self, name: impl Into<String>) -> Self {
        self.job = Some(name.into());
        self
    }

    /// Only this queue.
    ///
    /// ```no_run
    /// # use moso_jobs::DlqFilter;
    /// # fn f(d: DlqFilter) { let _ = d.queue("mail"); }
    /// ```
    #[must_use]
    pub fn queue(mut self, queue: impl Into<String>) -> Self {
        self.queue = Some(queue.into());
        self
    }

    /// Only failures at or after `since`.
    ///
    /// ```no_run
    /// # use moso_jobs::DlqFilter;
    /// # fn f(d: DlqFilter) { let _ = d.since(chrono::Utc::now()); }
    /// ```
    #[must_use]
    pub fn since(mut self, since: DateTime<Utc>) -> Self {
        self.since = Some(since);
        self
    }

    /// Only failures before `until`.
    ///
    /// ```no_run
    /// # use moso_jobs::DlqFilter;
    /// # fn f(d: DlqFilter) { let _ = d.until(chrono::Utc::now()); }
    /// ```
    #[must_use]
    pub fn until(mut self, until: DateTime<Utc>) -> Self {
        self.until = Some(until);
        self
    }

    /// Only failures whose error chain contains `needle`.
    ///
    /// How an operator retries "everything that failed because the payment
    /// gateway was down" without touching everything that failed for real
    /// reasons.
    ///
    /// ```no_run
    /// # use moso_jobs::DlqFilter;
    /// # fn f(d: DlqFilter) { let _ = d.error_contains("connection refused"); }
    /// ```
    #[must_use]
    pub fn error_contains(mut self, needle: impl Into<String>) -> Self {
        self.error_contains = Some(needle.into());
        self
    }

    /// Whether `letter` satisfies every set field.
    ///
    /// The in-memory half of the filter. The SQL backends translate the same
    /// fields into a `where` clause; this is what the memory and Redis backends
    /// evaluate, and what a test asserts against without a database.
    ///
    /// ```
    /// use moso_jobs::DlqFilter;
    ///
    /// let filter = DlqFilter::new().job("send_welcome_email");
    /// assert!(!filter.matches(&moso_jobs::DlqFilter::example()));
    /// ```
    #[must_use]
    pub fn matches(&self, letter: &DeadLetter) -> bool {
        if self.job.as_deref().is_some_and(|job| job != letter.name) {
            return false;
        }
        if self
            .queue
            .as_deref()
            .is_some_and(|queue| queue != letter.queue)
        {
            return false;
        }
        if self.since.is_some_and(|since| letter.failed_at < since) {
            return false;
        }
        if self.until.is_some_and(|until| letter.failed_at >= until) {
            return false;
        }
        if self
            .error_contains
            .as_deref()
            .is_some_and(|needle| !letter.last_error.contains(needle))
        {
            return false;
        }
        true
    }

    /// The job wire name this filter matches, if any.
    ///
    /// For a backend translating the filter into its own query language.
    ///
    /// ```
    /// # use moso_jobs::DlqFilter;
    /// assert_eq!(DlqFilter::new().job("x").job_name(), Some("x"));
    /// ```
    #[must_use]
    pub fn job_name(&self) -> Option<&str> {
        self.job.as_deref()
    }

    /// The queue this filter matches, if any.
    ///
    /// ```
    /// # use moso_jobs::DlqFilter;
    /// assert_eq!(DlqFilter::new().queue("mail").queue_name(), Some("mail"));
    /// ```
    #[must_use]
    pub fn queue_name(&self) -> Option<&str> {
        self.queue.as_deref()
    }

    /// The lower bound, if any.
    ///
    /// ```
    /// # use moso_jobs::DlqFilter;
    /// assert!(DlqFilter::new().since_at().is_none());
    /// ```
    #[must_use]
    pub fn since_at(&self) -> Option<DateTime<Utc>> {
        self.since
    }

    /// The upper bound, if any.
    ///
    /// ```
    /// # use moso_jobs::DlqFilter;
    /// assert!(DlqFilter::new().until_at().is_none());
    /// ```
    #[must_use]
    pub fn until_at(&self) -> Option<DateTime<Utc>> {
        self.until
    }

    /// The error substring, if any.
    ///
    /// ```
    /// # use moso_jobs::DlqFilter;
    /// assert_eq!(DlqFilter::new().error_contains("refused").error_needle(), Some("refused"));
    /// ```
    #[must_use]
    pub fn error_needle(&self) -> Option<&str> {
        self.error_contains.as_deref()
    }

    /// A dead letter to try a filter against, for a doctest.
    ///
    /// Not `#[cfg(test)]`: the examples on this type are the documentation, and
    /// an example that cannot be written is an example nobody checks.
    ///
    /// ```
    /// use moso_jobs::DlqFilter;
    ///
    /// let letter = DlqFilter::example();
    /// assert_eq!(letter.name, "generate_invoice");
    /// assert!(DlqFilter::new().matches(&letter), "an empty filter matches everything");
    /// ```
    #[must_use]
    pub fn example() -> DeadLetter {
        DeadLetter {
            id: JobId::new(),
            name: "generate_invoice".to_owned(),
            queue: "billing".to_owned(),
            payload: serde_json::Value::Null,
            attempts: 25,
            last_error: "connection refused".to_owned(),
            enqueued_at: Utc::now(),
            failed_at: Utc::now(),
            trace_parent: None,
            worker: None,
            actor: None,
        }
    }
}

/// How much is in the dead-letter queue, by job.
///
/// ```no_run
/// use moso_jobs::DlqStats;
///
/// # fn f(s: &DlqStats) {
/// let _ = s.total;
/// # }
/// ```
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DlqStats {
    /// How many dead letters there are in total.
    pub total: u64,
    /// How many per job wire name, most first.
    pub by_job: Vec<(String, u64)>,
    /// The oldest failure still held.
    pub oldest: Option<DateTime<Utc>>,
}

/// Reading and acting on the dead-letter queue.
///
/// A separate trait from [`Queue`](crate::Queue) because the operations are an
/// operator's, not a worker's: a backend can be a perfectly good queue and have
/// no dashboard, and splitting the traits keeps that honest.
///
/// ```no_run
/// use moso_jobs::{DeadLetterQueue, DlqFilter};
///
/// async fn count(dlq: &dyn DeadLetterQueue) -> moso_jobs::Result<u64> {
///     Ok(dlq.stats().await?.total)
/// }
/// ```
#[diagnostic::on_unimplemented(
    message = "`{Self}` has no dead-letter queue",
    label = "not a dead-letter queue",
    note = "a dead-letter queue implements `list`, `get`, `retry`, `discard` and `stats`",
    note = "help: every shipped backend has one; a custom `Queue` can leave it unimplemented, \
            in which case `moso jobs` and the dashboard show nothing for it"
)]
pub trait DeadLetterQueue: Send + Sync + 'static {
    /// Page through dead letters, newest failure first.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn list<'a>(
        &'a self,
        filter: &'a DlqFilter,
        cursor: Option<&'a str>,
        limit: u32,
    ) -> BoxFuture<'a, Result<(Vec<DeadLetter>, Option<String>)>>;

    /// One dead letter in full.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn get<'a>(&'a self, id: JobId) -> BoxFuture<'a, Result<Option<DeadLetter>>>;

    /// Re-enqueue everything matching, resetting the attempt counter.
    ///
    /// Returns how many were re-enqueued. `limit` is mandatory: a bulk retry
    /// over an unbounded filter is how a fix becomes an outage.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn retry<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>>;

    /// Delete everything matching. Returns how many.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn discard<'a>(&'a self, filter: &'a DlqFilter, limit: u32) -> BoxFuture<'a, Result<u64>>;

    /// How much is in there.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`](crate::Error::Unavailable).
    fn stats(&self) -> BoxFuture<'_, Result<DlqStats>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn letter() -> DeadLetter {
        DlqFilter::example()
    }

    /// An empty filter matching everything is exactly why every bulk operation
    /// that takes one also takes a mandatory limit.
    #[test]
    fn an_empty_filter_matches_everything() {
        assert!(DlqFilter::new().matches(&letter()));
    }

    /// Every field, one at a time, matching and not matching — because a filter
    /// that silently matches too much turns a fix into an outage.
    #[test]
    fn each_field_narrows_on_its_own() {
        let letter = letter();

        assert!(DlqFilter::new().job("generate_invoice").matches(&letter));
        assert!(!DlqFilter::new().job("send_welcome_email").matches(&letter));

        assert!(DlqFilter::new().queue("billing").matches(&letter));
        assert!(!DlqFilter::new().queue("mail").matches(&letter));

        assert!(
            DlqFilter::new()
                .since(letter.failed_at - chrono::Duration::hours(1))
                .matches(&letter)
        );
        assert!(
            !DlqFilter::new()
                .since(letter.failed_at + chrono::Duration::hours(1))
                .matches(&letter)
        );

        assert!(
            DlqFilter::new()
                .until(letter.failed_at + chrono::Duration::hours(1))
                .matches(&letter)
        );
        assert!(!DlqFilter::new().until(letter.failed_at).matches(&letter));

        assert!(DlqFilter::new().error_contains("refused").matches(&letter));
        assert!(
            !DlqFilter::new()
                .error_contains("timed out")
                .matches(&letter)
        );
    }

    /// The fields combine with `and`, so one mismatch is enough to exclude.
    #[test]
    fn the_fields_combine_with_and() {
        let letter = letter();
        assert!(
            DlqFilter::new()
                .job("generate_invoice")
                .queue("billing")
                .error_contains("refused")
                .matches(&letter)
        );
        assert!(
            !DlqFilter::new()
                .job("generate_invoice")
                .queue("mail")
                .matches(&letter)
        );
    }

    /// `since` is inclusive and `until` is exclusive, so two windows that touch
    /// do not both claim the same failure.
    #[test]
    fn the_bounds_do_not_overlap() {
        let letter = letter();
        assert!(DlqFilter::new().since(letter.failed_at).matches(&letter));
        assert!(!DlqFilter::new().until(letter.failed_at).matches(&letter));
    }

    /// A backend translating the filter reads it back through these, so what
    /// goes in has to come out.
    #[test]
    fn a_backend_can_read_every_field_back() {
        let now = Utc::now();
        let filter = DlqFilter::new()
            .job("j")
            .queue("q")
            .since(now)
            .until(now)
            .error_contains("needle");
        assert_eq!(filter.job_name(), Some("j"));
        assert_eq!(filter.queue_name(), Some("q"));
        assert_eq!(filter.since_at(), Some(now));
        assert_eq!(filter.until_at(), Some(now));
        assert_eq!(filter.error_needle(), Some("needle"));
    }
}
