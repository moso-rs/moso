//! Priority, backoff, and how a failed attempt becomes the next one.

use std::time::Duration;

use serde::{Deserialize, Serialize};

/// How urgently a job should be picked up.
///
/// Stored as a small integer so a queue can order by it in an index, and named
/// so a call site reads as an intention rather than a magic number.
///
/// ```
/// use moso_jobs::Priority;
///
/// assert!(Priority::High > Priority::Normal);
/// assert_eq!(Priority::default(), Priority::Normal);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(i16)]
pub enum Priority {
    /// Runs when nothing else is waiting. Bulk backfills, analytics rollups.
    Low = -10,
    /// The default.
    #[default]
    Normal = 0,
    /// Ahead of normal work. A password-reset email.
    High = 10,
    /// Ahead of everything. Reserved for work whose delay is a user-visible
    /// outage; using it for everything makes it mean nothing.
    Critical = 20,
}

impl Priority {
    /// The stored integer.
    ///
    /// ```
    /// use moso_jobs::Priority;
    ///
    /// assert_eq!(Priority::High.as_i16(), 10);
    /// ```
    #[must_use]
    pub const fn as_i16(self) -> i16 {
        self as i16
    }

    /// The nearest priority to a stored integer.
    ///
    /// Rounds rather than failing, so a row written by a newer deploy with a
    /// value this build does not know still runs.
    ///
    /// ```
    /// use moso_jobs::Priority;
    ///
    /// assert_eq!(Priority::from_i16(10), Priority::High);
    /// assert_eq!(Priority::from_i16(7), Priority::High);
    /// assert_eq!(Priority::from_i16(1_000), Priority::Critical);
    /// ```
    #[must_use]
    pub fn from_i16(value: i16) -> Self {
        const LADDER: [Priority; 4] = [
            Priority::Low,
            Priority::Normal,
            Priority::High,
            Priority::Critical,
        ];
        LADDER
            .into_iter()
            .min_by_key(|candidate| i32::from(candidate.as_i16()).abs_diff(i32::from(value)))
            .unwrap_or(Priority::Normal)
    }
}

/// How long to wait before the next attempt.
///
/// ```
/// use moso_jobs::Backoff;
/// use std::time::Duration;
///
/// let policy = Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600));
/// assert!(matches!(policy, Backoff::Exponential { .. }));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum Backoff {
    /// The same delay every time.
    Fixed {
        /// How long.
        delay: Duration,
    },
    /// `base * attempt`, capped.
    Linear {
        /// The step per attempt.
        base: Duration,
        /// The ceiling.
        max: Duration,
    },
    /// `base * 2^(attempt - 1)`, capped.
    Exponential {
        /// The first delay.
        base: Duration,
        /// The ceiling.
        max: Duration,
    },
    /// Retry as soon as the worker can take it. For a job whose failure is a
    /// lost race rather than an outage.
    Immediate,
}

impl Backoff {
    /// Exponential from `base`, capped at `max`.
    ///
    /// ```
    /// use moso_jobs::Backoff;
    /// use std::time::Duration;
    ///
    /// let _ = Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600));
    /// ```
    #[must_use]
    pub const fn exponential(base: Duration, max: Duration) -> Self {
        Self::Exponential { base, max }
    }

    /// The default policy: 30 seconds doubling to an hour.
    ///
    /// ```
    /// use moso_jobs::Backoff;
    ///
    /// let _ = Backoff::default_exponential();
    /// ```
    #[must_use]
    pub const fn default_exponential() -> Self {
        Self::Exponential {
            base: Duration::from_secs(30),
            max: Duration::from_secs(3600),
        }
    }

    /// The delay before attempt `attempt` (one-based), **without** jitter.
    ///
    /// Deterministic, so it can be asserted in a test.
    /// [`delay_jittered`](Backoff::delay_jittered) is what a worker calls.
    ///
    /// ```
    /// use moso_jobs::Backoff;
    /// use std::time::Duration;
    ///
    /// let policy = Backoff::default_exponential();
    /// assert_eq!(policy.delay(1), Duration::from_secs(30));
    /// assert_eq!(policy.delay(2), Duration::from_secs(60));
    /// ```
    #[must_use]
    pub fn delay(self, attempt: u32) -> Duration {
        // Attempt numbers are one-based everywhere in this crate; a zero here
        // is a caller's off-by-one, and treating it as the first attempt loses
        // less than panicking inside a worker loop.
        let attempt = attempt.max(1);
        match self {
            Self::Immediate => Duration::ZERO,
            Self::Fixed { delay } => delay,
            Self::Linear { base, max } => base.saturating_mul(attempt).min(max),
            Self::Exponential { base, max } => {
                // `2^(attempt - 1)` saturates at 32 doublings: the smallest
                // representable base is a nanosecond and 2^32 ns is four
                // seconds, so anything past this is already clamped by `max`.
                let doublings = (attempt - 1).min(32);
                base.saturating_mul(1_u32.checked_shl(doublings).unwrap_or(u32::MAX))
                    .min(max)
            }
        }
    }

    /// The delay with full jitter applied: uniform in `[0, delay]`.
    ///
    /// Full jitter and not "delay ± 10%", because the failure mode being
    /// avoided is a thousand jobs that failed together retrying together, and
    /// a narrow band does not break up a herd.
    ///
    /// ```
    /// use moso_jobs::Backoff;
    ///
    /// let policy = Backoff::default_exponential();
    /// assert!(policy.delay_jittered(3) <= policy.delay(3));
    /// ```
    #[must_use]
    pub fn delay_jittered(self, attempt: u32) -> Duration {
        crate::rng::duration_below(self.delay(attempt))
    }

    /// Parse the `#[job(backoff = "…")]` attribute's value.
    ///
    /// Accepts `"immediate"`, `"fixed(30s)"`, `"linear(30s, max = 1h)"` and
    /// `"exponential(30s, max = 1h)"`. Durations go through `humantime`, the
    /// same parser the rest of Moso's configuration uses.
    ///
    /// # Errors
    ///
    /// [`Error::Config`](crate::Error::Config) naming the unparseable part.
    /// Called by the macro, so the failure is a compile error on the attribute.
    ///
    /// ```
    /// use moso_jobs::Backoff;
    /// use std::time::Duration;
    ///
    /// let parsed = Backoff::parse("exponential(30s, max = 1h)")?;
    /// assert_eq!(parsed, Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600)));
    /// assert_eq!(Backoff::parse("immediate")?, Backoff::Immediate);
    /// # Ok::<(), moso_jobs::Error>(())
    /// ```
    pub fn parse(spec: &str) -> crate::Result<Self> {
        let spec = spec.trim();
        if spec.eq_ignore_ascii_case("immediate") {
            return Ok(Self::Immediate);
        }

        let (name, rest) = spec.split_once('(').ok_or_else(|| {
            crate::Error::config(format!(
                "`{spec}` is not a backoff policy\n\
                 help: write one of `immediate`, `fixed(30s)`, `linear(30s, max = 1h)` or \
                 `exponential(30s, max = 1h)`"
            ))
        })?;
        let arguments = rest.strip_suffix(')').ok_or_else(|| {
            crate::Error::config(format!("`{spec}` is missing its closing parenthesis"))
        })?;

        let mut parts = arguments.split(',');
        let base = parse_duration(parts.next().unwrap_or_default(), spec, "the first argument")?;
        let max = match parts.next() {
            Some(part) => {
                let value = part
                    .trim()
                    .strip_prefix("max")
                    .and_then(|rest| rest.trim_start().strip_prefix('='))
                    .ok_or_else(|| {
                        crate::Error::config(format!(
                            "the second argument of `{spec}` must be written `max = 1h`"
                        ))
                    })?;
                Some(parse_duration(value, spec, "`max`")?)
            }
            None => None,
        };
        if parts.next().is_some() {
            return Err(crate::Error::config(format!(
                "`{spec}` has more than two arguments"
            )));
        }

        match name.trim() {
            "fixed" => {
                if max.is_some() {
                    return Err(crate::Error::config(format!(
                        "`fixed` has no ceiling to set, so `max` is meaningless in `{spec}`"
                    )));
                }
                Ok(Self::Fixed { delay: base })
            }
            "linear" => Ok(Self::Linear {
                base,
                max: max.unwrap_or(Duration::from_secs(3600)),
            }),
            "exponential" => Ok(Self::Exponential {
                base,
                max: max.unwrap_or(Duration::from_secs(3600)),
            }),
            other => Err(crate::Error::config(format!(
                "`{other}` is not a backoff policy\n\
                 help: the four are `immediate`, `fixed`, `linear` and `exponential`"
            ))),
        }
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::default_exponential()
    }
}

/// Parses one duration inside a `#[job(backoff = "…")]` value.
///
/// `humantime` is the parser the rest of Moso's configuration uses, so `30s`,
/// `1h 30m` and `500ms` all mean here what they mean in a `.env` file.
fn parse_duration(text: &str, spec: &str, position: &str) -> crate::Result<Duration> {
    humantime::parse_duration(text.trim()).map_err(|error| {
        crate::Error::config(format!(
            "{position} of `{spec}` is not a duration: {error}\n\
             help: durations are written the way the rest of Moso's configuration writes them — \
             `30s`, `1h`, `500ms`"
        ))
    })
}

/// What a scheduled job does when **this schedule's own** previous occurrence is
/// still going.
///
/// The question is scoped to the schedule, by the identifier of the row it
/// enqueued last time — not to the queue, which would answer "is anything at all
/// running here" and make a schedule sharing a busy queue skip occurrences it
/// did not need to.
///
/// ```
/// use moso_jobs::Overlap;
///
/// // Skipping is the safe default: a nightly cleanup that takes 25 hours
/// // should not accumulate.
/// assert_eq!(Overlap::default(), Overlap::Skip);
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    /// Do not enqueue this occurrence. The default.
    #[default]
    Skip,
    /// Enqueue it anyway, and log at `INFO` that the schedule is overlapping.
    ///
    /// Whether the new occurrence *waits behind* the running one is the job's
    /// decision and not the schedule's: `#[job(serial)]` is what stops two
    /// instances running at once. Without it, `Queue` and [`Allow`](Overlap::Allow)
    /// differ only in that this one asks the queue the question and says what it
    /// found.
    Queue,
    /// Enqueue it and ask nothing — no overlap round trip at all.
    Allow,
}

/// The whole retry policy for one job, resolved from its constants.
///
/// Carried on the queued row rather than read from the type, so a policy change
/// applies to jobs enqueued after the deploy and does not retroactively change
/// what a queued row promised.
///
/// ```
/// use moso_jobs::{Backoff, RetryPolicy};
///
/// let policy = RetryPolicy::new(5, Backoff::default_exponential());
/// assert_eq!(policy.max_attempts(), 5);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// How many attempts in total.
    max_attempts: u32,
    /// How long between them.
    backoff: Backoff,
}

impl RetryPolicy {
    /// A policy with `max_attempts` attempts and `backoff` between them.
    ///
    /// ```
    /// use moso_jobs::{Backoff, RetryPolicy};
    ///
    /// let _ = RetryPolicy::new(3, Backoff::Immediate);
    /// ```
    #[must_use]
    pub const fn new(max_attempts: u32, backoff: Backoff) -> Self {
        Self {
            max_attempts,
            backoff,
        }
    }

    /// How many attempts in total.
    ///
    /// ```
    /// # use moso_jobs::{Backoff, RetryPolicy};
    /// assert_eq!(RetryPolicy::new(3, Backoff::Immediate).max_attempts(), 3);
    /// ```
    #[must_use]
    pub const fn max_attempts(self) -> u32 {
        self.max_attempts
    }

    /// The backoff between attempts.
    ///
    /// ```
    /// # use moso_jobs::{Backoff, RetryPolicy};
    /// let _ = RetryPolicy::new(3, Backoff::Immediate).backoff();
    /// ```
    #[must_use]
    pub const fn backoff(self) -> Backoff {
        self.backoff
    }

    /// When attempt `attempt` should run, given that it just failed.
    ///
    /// `None` when the retry budget is exhausted, which is the signal to move
    /// the job to the dead-letter queue.
    ///
    /// ```
    /// use moso_jobs::{Backoff, RetryPolicy};
    ///
    /// let policy = RetryPolicy::new(2, Backoff::Immediate);
    /// assert!(policy.next_delay(1).is_some());
    /// assert!(policy.next_delay(2).is_none(), "the budget is spent");
    /// ```
    #[must_use]
    pub fn next_delay(self, attempt: u32) -> Option<Duration> {
        // `attempt` is the one that just failed, so there is another only while
        // it is strictly below the budget. A `max_attempts` of 1 means "try
        // once", not "try once and then once more".
        if attempt >= self.max_attempts {
            return None;
        }
        Some(self.backoff.delay_jittered(attempt))
    }

    /// The nominal delay before attempt `attempt + 1`, without jitter.
    ///
    /// What a test asserts against, and what the dashboard shows as "next
    /// attempt in about…". `None` on the same condition as
    /// [`next_delay`](RetryPolicy::next_delay).
    ///
    /// ```
    /// use moso_jobs::{Backoff, RetryPolicy};
    /// use std::time::Duration;
    ///
    /// let policy = RetryPolicy::new(5, Backoff::default_exponential());
    /// assert_eq!(policy.nominal_delay(1), Some(Duration::from_secs(30)));
    /// assert_eq!(policy.nominal_delay(2), Some(Duration::from_secs(60)));
    /// ```
    #[must_use]
    pub fn nominal_delay(self, attempt: u32) -> Option<Duration> {
        if attempt >= self.max_attempts {
            return None;
        }
        Some(self.backoff.delay(attempt))
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(crate::DEFAULT_RETRIES, Backoff::default_exponential())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doubling is the thing a job author reasons about when they write
    /// `retries = 5`, so the whole ladder is pinned rather than the first step.
    #[test]
    fn exponential_doubles_and_then_stops_at_the_ceiling() {
        let policy = Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600));
        assert_eq!(policy.delay(1), Duration::from_secs(30));
        assert_eq!(policy.delay(2), Duration::from_secs(60));
        assert_eq!(policy.delay(3), Duration::from_secs(120));
        assert_eq!(policy.delay(8), Duration::from_secs(3600), "clamped");
        assert_eq!(policy.delay(64), Duration::from_secs(3600), "still clamped");
    }

    /// The default budget is a promise in `DEFAULT_RETRIES`' documentation, and
    /// a silently different total would change every job that overrode nothing.
    #[test]
    fn the_default_budget_spans_about_eighteen_hours() {
        let policy = RetryPolicy::default();
        let total: Duration = (1..policy.max_attempts())
            .map(|attempt| policy.backoff().delay(attempt))
            .sum();
        let hours = total.as_secs_f64() / 3_600.0;
        assert!(
            (16.0..20.0).contains(&hours),
            "the default ladder spans {hours:.1} hours; `DEFAULT_RETRIES` says about eighteen"
        );
    }

    /// Linear grows by a step and clamps the same way.
    #[test]
    fn linear_adds_a_step_per_attempt() {
        let policy = Backoff::Linear {
            base: Duration::from_secs(10),
            max: Duration::from_secs(45),
        };
        assert_eq!(policy.delay(1), Duration::from_secs(10));
        assert_eq!(policy.delay(4), Duration::from_secs(40));
        assert_eq!(policy.delay(5), Duration::from_secs(45), "clamped");
    }

    /// Fixed and immediate have no ladder at all, which is the point of them.
    #[test]
    fn fixed_and_immediate_ignore_the_attempt() {
        let fixed = Backoff::Fixed {
            delay: Duration::from_secs(7),
        };
        assert_eq!(fixed.delay(1), fixed.delay(20));
        assert_eq!(Backoff::Immediate.delay(9), Duration::ZERO);
    }

    /// A zero attempt is a caller's off-by-one; a worker loop must not panic on
    /// it, and treating it as the first attempt loses nothing.
    #[test]
    fn a_zero_attempt_is_treated_as_the_first() {
        let policy = Backoff::default_exponential();
        assert_eq!(policy.delay(0), policy.delay(1));
    }

    /// Full jitter has to stay inside `[0, delay]` — a sample above the nominal
    /// delay would make a `retry_after` header a lie.
    #[test]
    fn jitter_never_leaves_the_interval() {
        let policy = Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600));
        for attempt in 1..8 {
            let ceiling = policy.delay(attempt);
            for _ in 0..200 {
                assert!(policy.delay_jittered(attempt) <= ceiling);
            }
        }
    }

    /// Acceptance criterion 6: "retry backoff matches the declared policy
    /// within jitter bounds". With full jitter the mean of a large sample lands
    /// near half the nominal delay, which is the check that catches a policy
    /// wired to the wrong variant.
    #[test]
    fn the_jittered_mean_lands_near_half_the_nominal_delay() {
        let policy = Backoff::Fixed {
            delay: Duration::from_secs(100),
        };
        let samples = 4_000;
        let total: u64 = (0..samples)
            .map(|_| policy.delay_jittered(1).as_millis() as u64)
            .sum();
        let mean = total / samples;
        assert!(
            (45_000..55_000).contains(&mean),
            "the mean of full jitter over 100s was {mean}ms, expected about 50000ms"
        );
    }

    /// The four written forms of `#[job(backoff = "…")]`, since the macro
    /// hands this string straight through.
    #[test]
    fn the_written_forms_parse_to_what_they_say() {
        assert_eq!(Backoff::parse("immediate").unwrap(), Backoff::Immediate);
        assert_eq!(
            Backoff::parse("fixed(30s)").unwrap(),
            Backoff::Fixed {
                delay: Duration::from_secs(30)
            }
        );
        assert_eq!(
            Backoff::parse("linear(30s, max = 1h)").unwrap(),
            Backoff::Linear {
                base: Duration::from_secs(30),
                max: Duration::from_secs(3600)
            }
        );
        assert_eq!(
            Backoff::parse("exponential(30s, max = 1h)").unwrap(),
            Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600))
        );
        // Whitespace is a style choice, not a syntax error.
        assert_eq!(
            Backoff::parse("  exponential( 30s ,  max=1h )  ").unwrap(),
            Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600))
        );
    }

    /// The macro turns these into compile errors, so the message has to name
    /// what is wrong and what to write instead.
    #[test]
    fn a_malformed_policy_says_what_to_write_instead() {
        let error = Backoff::parse("exponential 30s").unwrap_err().to_string();
        assert!(error.contains("is not a backoff policy"), "{error}");
        assert!(error.contains("exponential(30s, max = 1h)"), "{error}");

        let error = Backoff::parse("quadratic(30s)").unwrap_err().to_string();
        assert!(
            error.contains("`quadratic` is not a backoff policy"),
            "{error}"
        );

        let error = Backoff::parse("exponential(banana)")
            .unwrap_err()
            .to_string();
        assert!(error.contains("is not a duration"), "{error}");

        let error = Backoff::parse("fixed(30s, max = 1h)")
            .unwrap_err()
            .to_string();
        assert!(error.contains("has no ceiling"), "{error}");

        let error = Backoff::parse("linear(1s, 1h)").unwrap_err().to_string();
        assert!(error.contains("must be written `max = 1h`"), "{error}");
    }

    /// A row written by a newer deploy with a priority this build does not know
    /// must still run, at the nearest priority it does know.
    #[test]
    fn an_unknown_priority_rounds_to_the_nearest_known_one() {
        assert_eq!(Priority::from_i16(-10), Priority::Low);
        assert_eq!(Priority::from_i16(0), Priority::Normal);
        assert_eq!(Priority::from_i16(10), Priority::High);
        assert_eq!(Priority::from_i16(20), Priority::Critical);
        assert_eq!(Priority::from_i16(4), Priority::Normal);
        assert_eq!(Priority::from_i16(6), Priority::High);
        assert_eq!(Priority::from_i16(i16::MIN), Priority::Low);
        assert_eq!(Priority::from_i16(i16::MAX), Priority::Critical);
    }

    /// The budget is spent when the attempt that just failed reaches it, and a
    /// budget of one means "try once".
    #[test]
    fn the_budget_runs_out_where_it_says_it_does() {
        let policy = RetryPolicy::new(3, Backoff::Immediate);
        assert!(policy.next_delay(1).is_some());
        assert!(policy.next_delay(2).is_some());
        assert!(policy.next_delay(3).is_none());
        assert!(policy.next_delay(99).is_none());

        assert!(
            RetryPolicy::new(1, Backoff::Immediate)
                .next_delay(1)
                .is_none()
        );
        assert!(
            RetryPolicy::new(0, Backoff::Immediate)
                .next_delay(1)
                .is_none()
        );
    }

    /// The retry policy travels on the queue row, so it has to survive JSON.
    #[test]
    fn a_policy_round_trips_through_a_queue_row() {
        let policy = RetryPolicy::new(
            5,
            Backoff::exponential(Duration::from_secs(30), Duration::from_secs(3600)),
        );
        let json = serde_json::to_string(&policy).expect("serialises");
        let back: RetryPolicy = serde_json::from_str(&json).expect("deserialises");
        assert_eq!(back, policy);
    }

    /// `Overlap::Skip` is the default because a nightly cleanup that takes 25
    /// hours must not accumulate. Changing it silently would be an outage.
    #[test]
    fn skipping_is_the_default_overlap() {
        assert_eq!(Overlap::default(), Overlap::Skip);
        let json = serde_json::to_string(&Overlap::Queue).expect("serialises");
        assert_eq!(json, "\"queue\"");
    }
}
