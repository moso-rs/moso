//! The six job metrics `docs/03-batteries/32-jobs.md` names, and the Prometheus
//! text a worker pod serves.
//!
//! | Metric | Kind | Labels |
//! | --- | --- | --- |
//! | `moso_jobs_enqueued_total` | counter | `job`, `queue` |
//! | `moso_jobs_duration_seconds` | histogram | `job`, `status` |
//! | `moso_jobs_queue_depth` | gauge | `queue` |
//! | `moso_jobs_latency_seconds` | histogram | `queue` |
//! | `moso_jobs_retries_total` | counter | `job`, `reason` |
//! | `moso_jobs_dlq_total` | counter | `job` |
//!
//! Plus `moso_jobs_backpressure_active{queue}` and
//! `moso_jobs_outbox_lag_seconds`, both of which the design document asks for by
//! name because their failure is invisible from the queue's own numbers.
//!
//! # Why a registry here and not `MetricsRecorder`
//!
//! `moso_core::middleware::metrics::MetricsRecorder` takes a `RequestSample`: it
//! is the HTTP request's recorder, and a job is not a request. Rather than widen
//! that trait for one caller, this module keeps its own registry — a few atomics
//! and a bounded label map — and renders it as Prometheus text. An application
//! with a real exporter reads [`snapshot`] and forwards it.
//!
//! # Cardinality
//!
//! Every label here comes from a *registered* job's wire name or a *declared*
//! queue, both of which are bounded by the source code. The one unbounded input
//! — a retry `reason` — is mapped onto a closed set of five, because a metric
//! label built from an error message is how a metrics backend falls over.

use std::collections::BTreeMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The histogram buckets, in seconds.
///
/// Chosen for job durations rather than request latencies: the interesting
/// questions are "did it finish inside its timeout" and "is the queue keeping
/// up", both of which live between a second and an hour.
const BUCKETS: [f64; 12] = [
    0.005, 0.05, 0.25, 1.0, 5.0, 15.0, 60.0, 300.0, 900.0, 1800.0, 3600.0, 21600.0,
];

/// Why a job was retried, as a closed set.
///
/// A label built from an error message is unbounded cardinality, which is how a
/// metrics backend falls over. Five reasons is enough to answer "is this a
/// dependency being down or our own bug".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum RetryReason {
    /// The job body returned a retryable error.
    Failed,
    /// The attempt exceeded the job's timeout.
    Timeout,
    /// The queue backend was unreachable.
    Unavailable,
    /// The lease expired and another worker reclaimed the job.
    Reclaimed,
    /// The worker shut down mid-attempt and put the job back.
    Requeued,
}

impl RetryReason {
    /// The label value.
    ///
    /// ```
    /// use moso_jobs::metrics::RetryReason;
    ///
    /// assert_eq!(RetryReason::Timeout.as_str(), "timeout");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Failed => "failed",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::Reclaimed => "reclaimed",
            Self::Requeued => "requeued",
        }
    }

    /// The reason an error implies.
    ///
    /// ```
    /// use moso_jobs::metrics::RetryReason;
    /// use moso_jobs::Error;
    ///
    /// assert_eq!(RetryReason::of(&Error::retry("x")), RetryReason::Failed);
    /// ```
    #[must_use]
    pub fn of(error: &crate::Error) -> Self {
        match error {
            crate::Error::Timeout { .. } => Self::Timeout,
            crate::Error::Unavailable { .. } => Self::Unavailable,
            _ => Self::Failed,
        }
    }
}

/// How a job attempt ended, as a `status` label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// It finished.
    Success,
    /// It failed and will be tried again.
    Retry,
    /// It failed for the last time and went to the dead-letter queue.
    Dead,
}

impl Outcome {
    /// The label value.
    ///
    /// ```
    /// use moso_jobs::metrics::Outcome;
    ///
    /// assert_eq!(Outcome::Success.as_str(), "success");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Retry => "retry",
            Self::Dead => "dead",
        }
    }
}

/// One histogram: bucket counters, a sum and a count.
#[derive(Debug, Default)]
struct Histogram {
    /// Cumulative counts, aligned with [`BUCKETS`].
    buckets: [u64; BUCKETS.len()],
    /// The total of every observation, in seconds.
    sum: f64,
    /// How many observations.
    count: u64,
}

impl Histogram {
    /// Record one observation.
    fn observe(&mut self, seconds: f64) {
        for (index, bound) in BUCKETS.iter().enumerate() {
            if seconds <= *bound {
                self.buckets[index] += 1;
            }
        }
        self.sum += seconds;
        self.count += 1;
    }
}

/// Everything a worker has counted since it started.
#[derive(Debug, Default)]
struct Registry {
    /// `moso_jobs_enqueued_total{job,queue}`.
    enqueued: BTreeMap<(String, String), u64>,
    /// `moso_jobs_duration_seconds{job,status}`.
    duration: BTreeMap<(String, &'static str), Histogram>,
    /// `moso_jobs_queue_depth{queue}`.
    depth: BTreeMap<String, u64>,
    /// `moso_jobs_latency_seconds{queue}`.
    latency: BTreeMap<String, Histogram>,
    /// `moso_jobs_retries_total{job,reason}`.
    retries: BTreeMap<(String, &'static str), u64>,
    /// `moso_jobs_dlq_total{job}`.
    dlq: BTreeMap<String, u64>,
    /// `moso_jobs_backpressure_active{queue}`.
    backpressure: BTreeMap<String, u64>,
    /// `moso_jobs_outbox_lag_seconds`.
    outbox_lag: Option<f64>,
}

/// The one registry per process.
static REGISTRY: RwLock<Option<Registry>> = RwLock::new(None);

/// How many jobs are running right now, across every worker in this process.
///
/// An atomic rather than a map entry because it is read on `/readyz`, which a
/// probe hits every couple of seconds and must never block behind a writer.
static RUNNING: AtomicU64 = AtomicU64::new(0);

/// Do something with the registry, creating it on first use.
fn with<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    let mut guard = REGISTRY.write().unwrap_or_else(|poisoned| {
        // A panic inside a metrics update must not take the worker with it: the
        // numbers are diagnostics, and a poisoned lock here would turn a
        // reporting bug into an outage.
        REGISTRY.clear_poison();
        poisoned.into_inner()
    });
    f(guard.get_or_insert_with(Registry::default))
}

/// Record one enqueue.
///
/// ```
/// moso_jobs::metrics::enqueued("send_welcome_email", "mail");
/// ```
pub fn enqueued(job: &str, queue: &str) {
    with(|registry| {
        *registry
            .enqueued
            .entry((job.to_owned(), queue.to_owned()))
            .or_default() += 1;
    });
}

/// Record one finished attempt.
///
/// ```
/// use moso_jobs::metrics::{Outcome, finished};
/// use std::time::Duration;
///
/// finished("send_welcome_email", Outcome::Success, Duration::from_millis(120));
/// ```
pub fn finished(job: &str, outcome: Outcome, elapsed: Duration) {
    with(|registry| {
        registry
            .duration
            .entry((job.to_owned(), outcome.as_str()))
            .or_default()
            .observe(elapsed.as_secs_f64());
    });
}

/// Record how long a job waited between being enqueued and starting.
///
/// The number that says whether the queue is keeping up, which depth alone does
/// not.
///
/// ```
/// moso_jobs::metrics::started("mail", std::time::Duration::from_millis(30));
/// ```
pub fn started(queue: &str, latency: Duration) {
    with(|registry| {
        registry
            .latency
            .entry(queue.to_owned())
            .or_default()
            .observe(latency.as_secs_f64());
    });
}

/// Record one retry.
///
/// ```
/// use moso_jobs::metrics::{RetryReason, retried};
///
/// retried("send_welcome_email", RetryReason::Timeout);
/// ```
pub fn retried(job: &str, reason: RetryReason) {
    with(|registry| {
        *registry
            .retries
            .entry((job.to_owned(), reason.as_str()))
            .or_default() += 1;
    });
}

/// Record one job giving up.
///
/// ```
/// moso_jobs::metrics::dead_lettered("send_welcome_email");
/// ```
pub fn dead_lettered(job: &str) {
    with(|registry| {
        *registry.dlq.entry(job.to_owned()).or_default() += 1;
    });
}

/// Publish the depth of one queue.
///
/// ```
/// moso_jobs::metrics::depth("mail", 42);
/// ```
pub fn depth(queue: &str, ready: u64) {
    with(|registry| {
        registry.depth.insert(queue.to_owned(), ready);
    });
}

/// Publish whether a queue is under backpressure.
///
/// Alertable, because a worker that has quietly stopped taking bulk work looks
/// identical to one that has nothing to do.
///
/// ```
/// moso_jobs::metrics::backpressure("bulk", true);
/// ```
pub fn backpressure(queue: &str, active: bool) {
    with(|registry| {
        registry
            .backpressure
            .insert(queue.to_owned(), u64::from(active));
    });
}

/// Publish how far behind the outbox relay is.
///
/// The one piece whose failure is invisible from the queue's own metrics: the
/// jobs are sitting in a table nobody is looking at.
///
/// ```
/// moso_jobs::metrics::outbox_lag(Some(std::time::Duration::from_millis(15)));
/// ```
pub fn outbox_lag(lag: Option<Duration>) {
    with(|registry| {
        registry.outbox_lag = lag.map(|lag| lag.as_secs_f64());
    });
}

/// Note that a job started running.
///
/// ```
/// moso_jobs::metrics::running_started();
/// assert!(moso_jobs::metrics::running() >= 1);
/// moso_jobs::metrics::running_finished();
/// ```
pub fn running_started() {
    RUNNING.fetch_add(1, Ordering::Relaxed);
}

/// Note that a job stopped running.
///
/// ```
/// moso_jobs::metrics::running_started();
/// moso_jobs::metrics::running_finished();
/// ```
pub fn running_finished() {
    // Saturating rather than wrapping: an unbalanced pair is a bug, and a
    // gauge that reads 18 quintillion is a bug nobody can diagnose.
    let _ = RUNNING.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(1))
    });
}

/// How many jobs are running in this process right now.
///
/// ```
/// let _: u64 = moso_jobs::metrics::running();
/// ```
#[must_use]
pub fn running() -> u64 {
    RUNNING.load(Ordering::Relaxed)
}

/// Everything counted so far, as Prometheus text.
///
/// What `/metrics` serves on a worker pod. Ends with a newline, as the exposition
/// format requires.
///
/// ```
/// moso_jobs::metrics::enqueued("send_welcome_email", "mail");
/// let text = moso_jobs::metrics::snapshot();
/// assert!(text.contains("moso_jobs_enqueued_total"));
/// assert!(text.ends_with('\n'));
/// ```
#[must_use]
pub fn snapshot() -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2048);

    with(|registry| {
        let _ = writeln!(
            out,
            "# HELP moso_jobs_enqueued_total Jobs put on a queue.\n\
             # TYPE moso_jobs_enqueued_total counter"
        );
        for ((job, queue), count) in &registry.enqueued {
            let _ = writeln!(
                out,
                "moso_jobs_enqueued_total{{job=\"{}\",queue=\"{}\"}} {count}",
                escape(job),
                escape(queue)
            );
        }

        let _ = writeln!(
            out,
            "# HELP moso_jobs_duration_seconds How long one attempt took.\n\
             # TYPE moso_jobs_duration_seconds histogram"
        );
        for ((job, status), histogram) in &registry.duration {
            write_histogram(
                &mut out,
                "moso_jobs_duration_seconds",
                &format!("job=\"{}\",status=\"{status}\"", escape(job)),
                histogram,
            );
        }

        let _ = writeln!(
            out,
            "# HELP moso_jobs_queue_depth Jobs waiting to run.\n\
             # TYPE moso_jobs_queue_depth gauge"
        );
        for (queue, ready) in &registry.depth {
            let _ = writeln!(
                out,
                "moso_jobs_queue_depth{{queue=\"{}\"}} {ready}",
                escape(queue)
            );
        }

        let _ = writeln!(
            out,
            "# HELP moso_jobs_latency_seconds Time between enqueue and start.\n\
             # TYPE moso_jobs_latency_seconds histogram"
        );
        for (queue, histogram) in &registry.latency {
            write_histogram(
                &mut out,
                "moso_jobs_latency_seconds",
                &format!("queue=\"{}\"", escape(queue)),
                histogram,
            );
        }

        let _ = writeln!(
            out,
            "# HELP moso_jobs_retries_total Attempts that will be tried again.\n\
             # TYPE moso_jobs_retries_total counter"
        );
        for ((job, reason), count) in &registry.retries {
            let _ = writeln!(
                out,
                "moso_jobs_retries_total{{job=\"{}\",reason=\"{reason}\"}} {count}",
                escape(job)
            );
        }

        let _ = writeln!(
            out,
            "# HELP moso_jobs_dlq_total Jobs that gave up.\n\
             # TYPE moso_jobs_dlq_total counter"
        );
        for (job, count) in &registry.dlq {
            let _ = writeln!(
                out,
                "moso_jobs_dlq_total{{job=\"{}\"}} {count}",
                escape(job)
            );
        }

        let _ = writeln!(
            out,
            "# HELP moso_jobs_backpressure_active Whether a queue is over its depth threshold.\n\
             # TYPE moso_jobs_backpressure_active gauge"
        );
        for (queue, active) in &registry.backpressure {
            let _ = writeln!(
                out,
                "moso_jobs_backpressure_active{{queue=\"{}\"}} {active}",
                escape(queue)
            );
        }

        if let Some(lag) = registry.outbox_lag {
            let _ = writeln!(
                out,
                "# HELP moso_jobs_outbox_lag_seconds Age of the oldest unrelayed outbox row.\n\
                 # TYPE moso_jobs_outbox_lag_seconds gauge\n\
                 moso_jobs_outbox_lag_seconds {lag}"
            );
        }
    });

    let _ = writeln!(
        out,
        "# HELP moso_jobs_running Jobs executing in this process right now.\n\
         # TYPE moso_jobs_running gauge\n\
         moso_jobs_running {}",
        running()
    );

    out
}

/// Forget everything. For a test that wants a clean registry.
///
/// ```
/// moso_jobs::metrics::reset();
/// ```
pub fn reset() {
    with(|registry| *registry = Registry::default());
    RUNNING.store(0, Ordering::Relaxed);
}

/// Serialises the tests that [`reset`] the process-global [`REGISTRY`].
///
/// There is one registry per process by design, so two `#[test]`s that clear it
/// and then assert on a [`snapshot`] cannot run on parallel threads: whichever
/// resets second wipes the counters the first is about to read. Every test that
/// calls [`reset`] takes this lock first, in `metrics.rs` and in `health.rs`
/// alike. Poisoning is ignored — a panicking test has already failed, and
/// turning that into a cascade of lock failures hides which one it was.
#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Take [`TEST_LOCK`], ignoring poisoning, and reset the registry.
#[cfg(test)]
pub(crate) fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    let guard = TEST_LOCK.lock().unwrap_or_else(|poisoned| {
        TEST_LOCK.clear_poison();
        poisoned.into_inner()
    });
    reset();
    guard
}

/// Render one histogram's buckets, sum and count.
fn write_histogram(out: &mut String, name: &str, labels: &str, histogram: &Histogram) {
    use std::fmt::Write as _;
    for (index, bound) in BUCKETS.iter().enumerate() {
        let _ = writeln!(
            out,
            "{name}_bucket{{{labels},le=\"{bound}\"}} {}",
            histogram.buckets[index]
        );
    }
    let _ = writeln!(
        out,
        "{name}_bucket{{{labels},le=\"+Inf\"}} {}",
        histogram.count
    );
    let _ = writeln!(out, "{name}_sum{{{labels}}} {}", histogram.sum);
    let _ = writeln!(out, "{name}_count{{{labels}}} {}", histogram.count);
}

/// Escape a label value, as the exposition format requires.
fn escape(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The six names the design document promises have to be exactly those,
    /// because a dashboard is written against the string.
    #[test]
    fn the_documented_metric_names_are_all_emitted() {
        let _guard = test_guard();
        enqueued("send_welcome_email", "mail");
        finished(
            "send_welcome_email",
            Outcome::Success,
            Duration::from_secs(1),
        );
        depth("mail", 3);
        started("mail", Duration::from_millis(40));
        retried("send_welcome_email", RetryReason::Timeout);
        dead_lettered("send_welcome_email");
        backpressure("bulk", true);
        outbox_lag(Some(Duration::from_millis(15)));

        let text = snapshot();
        for name in [
            "moso_jobs_enqueued_total",
            "moso_jobs_duration_seconds",
            "moso_jobs_queue_depth",
            "moso_jobs_latency_seconds",
            "moso_jobs_retries_total",
            "moso_jobs_dlq_total",
            "moso_jobs_backpressure_active",
            "moso_jobs_outbox_lag_seconds",
            "moso_jobs_running",
        ] {
            assert!(text.contains(name), "{name} is missing from:\n{text}");
        }
        assert!(
            text.contains(r#"moso_jobs_enqueued_total{job="send_welcome_email",queue="mail"} 1"#)
        );
        assert!(text.contains(r#"moso_jobs_queue_depth{queue="mail"} 3"#));
        assert!(text.contains(r#"moso_jobs_backpressure_active{queue="bulk"} 1"#));
        assert!(text.ends_with('\n'));
        reset();
    }

    /// A histogram whose buckets are not cumulative is a histogram Prometheus
    /// reads as nonsense.
    #[test]
    fn histogram_buckets_are_cumulative_and_end_at_the_count() {
        let mut histogram = Histogram::default();
        histogram.observe(0.01);
        histogram.observe(2.0);
        histogram.observe(100_000.0);

        assert_eq!(histogram.count, 3);
        let mut previous = 0;
        for count in histogram.buckets {
            assert!(
                count >= previous,
                "buckets must not go down: {:?}",
                histogram.buckets
            );
            previous = count;
        }
        // The last observation is past the largest bound, so no bucket holds it.
        assert!(*histogram.buckets.last().expect("buckets") < histogram.count);
        assert!((histogram.sum - 100_002.01).abs() < 1e-6);
    }

    /// A label built from an error message is unbounded cardinality; the reason
    /// set is closed and derived from the error's *variant*.
    #[test]
    fn the_retry_reason_is_a_closed_set() {
        assert_eq!(
            RetryReason::of(&crate::Error::retry("anything")),
            RetryReason::Failed
        );
        assert_eq!(
            RetryReason::of(&crate::Error::Timeout {
                job: "j",
                timeout: Duration::from_secs(1)
            }),
            RetryReason::Timeout
        );
        assert_eq!(
            RetryReason::of(&crate::Error::unavailable("postgres", "down")),
            RetryReason::Unavailable
        );
    }

    /// A quote in a job name would otherwise break the exposition format for
    /// every metric after it.
    #[test]
    fn label_values_are_escaped() {
        assert_eq!(escape(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape("a\\b"), "a\\\\b");
        assert_eq!(escape("a\nb"), "a\\nb");
    }

    /// An unbalanced `running_finished` is a bug; a gauge reading 18
    /// quintillion is a bug nobody can diagnose.
    #[test]
    fn the_running_gauge_never_wraps() {
        let _guard = test_guard();
        running_finished();
        assert_eq!(running(), 0);
        running_started();
        assert_eq!(running(), 1);
        running_finished();
        assert_eq!(running(), 0);
        reset();
    }
}
