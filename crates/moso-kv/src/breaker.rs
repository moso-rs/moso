//! The circuit breaker, and the jitter it opens with.
//!
//! A struggling Redis does not need every instance of every service retrying
//! it on every request. After `failure_threshold` consecutive transient
//! failures the breaker opens: calls fail immediately with
//! [`Error::CircuitOpen`](crate::Error::CircuitOpen) — which, for a `Degrade`
//! namespace, is a cache miss and nothing more — and the store gets the
//! quiet it needs.
//!
//! Three details are the whole design:
//!
//! 1. **Only transient failures count.** A `WRONGTYPE`, an unsupported
//!    operation or a decode failure is a bug in the program; retrying it
//!    forever is correct, and opening a circuit over it would hide it.
//! 2. **The cooldown is jittered.** Ten instances that all opened at the same
//!    moment must not all probe at the same moment, or the recovering store
//!    gets a thundering herd exactly when it is weakest.
//! 3. **Exactly one probe.** When the cooldown ends, the first caller through
//!    becomes the probe and everybody else is still refused. If the probe
//!    succeeds the breaker closes; if it fails the cooldown doubles, up to a
//!    ceiling.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How a breaker opens and recovers.
///
/// ```
/// use moso_kv::breaker::BreakerConfig;
/// use std::time::Duration;
///
/// let config = BreakerConfig::default()
///     .failure_threshold(3)
///     .cooldown(Duration::from_millis(250));
///
/// assert_eq!(config.failure_threshold, 3);
/// assert_eq!(config.cooldown, Duration::from_millis(250));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct BreakerConfig {
    /// Consecutive transient failures before the breaker opens.
    pub failure_threshold: u32,
    /// How long it stays open the first time.
    pub cooldown: Duration,
    /// The longest the cooldown grows to after repeated failed probes.
    pub max_cooldown: Duration,
    /// How much of the cooldown is randomised, in percent of the cooldown.
    ///
    /// `25` means a cooldown between 100% and 125% of the nominal one.
    pub jitter_percent: u32,
}

impl Default for BreakerConfig {
    /// Five failures, a second of cooldown growing to thirty, 25% jitter.
    ///
    /// Five and not one because a single timeout is normal on a busy store;
    /// a second and not ten because a cache that stays off for ten seconds
    /// after a blip costs more than the retries would have.
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cooldown: Duration::from_secs(1),
            max_cooldown: Duration::from_secs(30),
            jitter_percent: 25,
        }
    }
}

impl BreakerConfig {
    /// A breaker that never opens.
    ///
    /// For a test that wants to see every failure, and for a store whose
    /// failures are local (the memory backend has none).
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig};
    ///
    /// let breaker = Breaker::new(BreakerConfig::never());
    /// for _ in 0..1_000 {
    ///     breaker.record_failure();
    /// }
    /// assert!(breaker.allow().is_ok());
    /// ```
    #[must_use]
    pub const fn never() -> Self {
        Self {
            failure_threshold: u32::MAX,
            cooldown: Duration::ZERO,
            max_cooldown: Duration::ZERO,
            jitter_percent: 0,
        }
    }

    /// Set [`failure_threshold`](Self::failure_threshold).
    ///
    /// ```
    /// use moso_kv::breaker::BreakerConfig;
    ///
    /// assert_eq!(BreakerConfig::default().failure_threshold(2).failure_threshold, 2);
    /// ```
    #[must_use]
    pub const fn failure_threshold(mut self, threshold: u32) -> Self {
        self.failure_threshold = threshold;
        self
    }

    /// Set [`cooldown`](Self::cooldown).
    ///
    /// ```
    /// use moso_kv::breaker::BreakerConfig;
    /// use std::time::Duration;
    ///
    /// assert_eq!(
    ///     BreakerConfig::default().cooldown(Duration::from_secs(2)).cooldown,
    ///     Duration::from_secs(2),
    /// );
    /// ```
    #[must_use]
    pub const fn cooldown(mut self, cooldown: Duration) -> Self {
        self.cooldown = cooldown;
        self
    }

    /// Set [`max_cooldown`](Self::max_cooldown).
    ///
    /// ```
    /// use moso_kv::breaker::BreakerConfig;
    /// use std::time::Duration;
    ///
    /// let config = BreakerConfig::default().max_cooldown(Duration::from_secs(5));
    /// assert_eq!(config.max_cooldown, Duration::from_secs(5));
    /// ```
    #[must_use]
    pub const fn max_cooldown(mut self, max: Duration) -> Self {
        self.max_cooldown = max;
        self
    }

    /// Set [`jitter_percent`](Self::jitter_percent).
    ///
    /// ```
    /// use moso_kv::breaker::BreakerConfig;
    ///
    /// assert_eq!(BreakerConfig::default().jitter_percent(0).jitter_percent, 0);
    /// ```
    #[must_use]
    pub const fn jitter_percent(mut self, percent: u32) -> Self {
        self.jitter_percent = percent;
        self
    }
}

/// What a breaker is doing right now.
///
/// ```
/// use moso_kv::breaker::BreakerState;
///
/// assert_eq!(BreakerState::Closed.as_str(), "closed");
/// assert!(BreakerState::Closed.is_passing());
/// assert!(!BreakerState::Open.is_passing());
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BreakerState {
    /// Everything goes through.
    Closed,
    /// Nothing goes through; the cooldown is running.
    Open,
    /// One probe is in flight and everything else is still refused.
    HalfOpen,
}

impl BreakerState {
    /// Whether calls are allowed.
    ///
    /// ```
    /// use moso_kv::breaker::BreakerState;
    ///
    /// assert!(BreakerState::Closed.is_passing());
    /// ```
    #[must_use]
    pub const fn is_passing(self) -> bool {
        matches!(self, BreakerState::Closed)
    }

    /// The name in a log field or a metric label.
    ///
    /// ```
    /// use moso_kv::breaker::BreakerState;
    ///
    /// assert_eq!(BreakerState::HalfOpen.as_str(), "half_open");
    /// ```
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BreakerState::Closed => "closed",
            BreakerState::Open => "open",
            BreakerState::HalfOpen => "half_open",
        }
    }

    /// The `u8` the atomic holds.
    const fn code(self) -> u8 {
        match self {
            BreakerState::Closed => 0,
            BreakerState::Open => 1,
            BreakerState::HalfOpen => 2,
        }
    }

    /// The state a `u8` means. Anything unexpected reads as `Closed`, which is
    /// the safe direction: a confused breaker lets traffic through rather than
    /// blocking it forever.
    const fn from_code(code: u8) -> Self {
        match code {
            1 => BreakerState::Open,
            2 => BreakerState::HalfOpen,
            _ => BreakerState::Closed,
        }
    }
}

/// A lock-free circuit breaker.
///
/// ```
/// use moso_kv::breaker::{Breaker, BreakerConfig, BreakerState};
/// use std::time::Duration;
///
/// let breaker = Breaker::new(
///     BreakerConfig::default()
///         .failure_threshold(2)
///         .cooldown(Duration::from_millis(50))
///         .jitter_percent(0),
/// );
///
/// assert!(breaker.allow().is_ok());
///
/// breaker.record_failure();
/// assert!(breaker.allow().is_ok(), "one failure is not a pattern");
///
/// breaker.record_failure();
/// assert_eq!(breaker.state(), BreakerState::Open);
/// assert!(breaker.allow().is_err());
///
/// // A success while closed resets the count.
/// breaker.record_success();
/// assert_eq!(breaker.state(), BreakerState::Closed);
/// ```
#[derive(Debug)]
pub struct Breaker {
    config: BreakerConfig,
    /// A [`BreakerState`] code.
    state: AtomicU32,
    /// Consecutive transient failures while closed.
    failures: AtomicU32,
    /// Milliseconds since [`Self::base`] at which the cooldown ends.
    open_until_ms: AtomicU64,
    /// The current cooldown in milliseconds, doubling after a failed probe.
    cooldown_ms: AtomicU64,
    /// The monotonic origin the two `_ms` fields are measured from.
    base: Instant,
    /// The jitter generator's state.
    seed: AtomicU64,
}

impl Breaker {
    /// A closed breaker.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig, BreakerState};
    ///
    /// assert_eq!(Breaker::new(BreakerConfig::default()).state(), BreakerState::Closed);
    /// ```
    #[must_use]
    pub fn new(config: BreakerConfig) -> Self {
        let cooldown_ms = u64::try_from(config.cooldown.as_millis()).unwrap_or(u64::MAX);
        Self {
            config,
            state: AtomicU32::new(u32::from(BreakerState::Closed.code())),
            failures: AtomicU32::new(0),
            open_until_ms: AtomicU64::new(0),
            cooldown_ms: AtomicU64::new(cooldown_ms),
            base: Instant::now(),
            seed: AtomicU64::new(seed_from_clock()),
        }
    }

    /// The configuration this breaker was built with.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig};
    ///
    /// let breaker = Breaker::new(BreakerConfig::default().failure_threshold(9));
    /// assert_eq!(breaker.config().failure_threshold, 9);
    /// ```
    #[must_use]
    pub fn config(&self) -> BreakerConfig {
        self.config
    }

    /// What the breaker is doing.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig, BreakerState};
    ///
    /// assert_eq!(Breaker::new(BreakerConfig::never()).state(), BreakerState::Closed);
    /// ```
    #[must_use]
    pub fn state(&self) -> BreakerState {
        let code = u8::try_from(self.state.load(Ordering::Acquire)).unwrap_or(0);
        BreakerState::from_code(code)
    }

    /// May a call go through?
    ///
    /// `Err(remaining)` when it may not, carrying how long until the next
    /// probe — which becomes the `Retry-After` on the 503.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig};
    /// use std::time::Duration;
    ///
    /// let breaker = Breaker::new(
    ///     BreakerConfig::default().failure_threshold(1).cooldown(Duration::from_secs(5)),
    /// );
    /// breaker.record_failure();
    ///
    /// let remaining = breaker.allow().expect_err("open");
    /// assert!(remaining <= Duration::from_secs(7));
    /// ```
    pub fn allow(&self) -> Result<(), Duration> {
        match self.state() {
            BreakerState::Closed => Ok(()),
            BreakerState::HalfOpen => Err(self.remaining()),
            BreakerState::Open => {
                let remaining = self.remaining();
                if remaining > Duration::ZERO {
                    return Err(remaining);
                }
                // The cooldown is up. Exactly one caller becomes the probe.
                match self.state.compare_exchange(
                    u32::from(BreakerState::Open.code()),
                    u32::from(BreakerState::HalfOpen.code()),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => Ok(()),
                    Err(_) => Err(Duration::ZERO),
                }
            }
        }
    }

    /// Record that a call worked.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig, BreakerState};
    ///
    /// let breaker = Breaker::new(BreakerConfig::default().failure_threshold(1));
    /// breaker.record_failure();
    /// assert_eq!(breaker.state(), BreakerState::Open);
    ///
    /// breaker.record_success();
    /// assert_eq!(breaker.state(), BreakerState::Closed);
    /// ```
    pub fn record_success(&self) {
        self.failures.store(0, Ordering::Release);
        self.cooldown_ms.store(
            u64::try_from(self.config.cooldown.as_millis()).unwrap_or(u64::MAX),
            Ordering::Release,
        );
        self.state
            .store(u32::from(BreakerState::Closed.code()), Ordering::Release);
    }

    /// Record that a call failed transiently.
    ///
    /// Only [`Error::retryable`](crate::Error::retryable) failures should reach
    /// here; a programmer error must not open a circuit.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig, BreakerState};
    ///
    /// let breaker = Breaker::new(BreakerConfig::default().failure_threshold(2));
    /// breaker.record_failure();
    /// assert_eq!(breaker.state(), BreakerState::Closed);
    /// breaker.record_failure();
    /// assert_eq!(breaker.state(), BreakerState::Open);
    /// ```
    pub fn record_failure(&self) {
        if self.config.failure_threshold == u32::MAX {
            return;
        }

        if self.state() == BreakerState::HalfOpen {
            // The probe failed: back off further before the next one.
            let doubled = self.cooldown_ms.load(Ordering::Acquire).saturating_mul(2);
            let ceiling = u64::try_from(self.config.max_cooldown.as_millis()).unwrap_or(u64::MAX);
            self.cooldown_ms
                .store(doubled.min(ceiling.max(1)), Ordering::Release);
            self.open();
            return;
        }

        let failures = self.failures.fetch_add(1, Ordering::AcqRel) + 1;
        if failures >= self.config.failure_threshold {
            self.open();
        }
    }

    /// How long until the breaker will consider a probe.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig};
    /// use std::time::Duration;
    ///
    /// assert_eq!(Breaker::new(BreakerConfig::default()).remaining(), Duration::ZERO);
    /// ```
    #[must_use]
    pub fn remaining(&self) -> Duration {
        let until = self.open_until_ms.load(Ordering::Acquire);
        let now = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        Duration::from_millis(until.saturating_sub(now))
    }

    /// Force the breaker closed — what a test, or an operator endpoint, does.
    ///
    /// ```
    /// use moso_kv::breaker::{Breaker, BreakerConfig, BreakerState};
    ///
    /// let breaker = Breaker::new(BreakerConfig::default().failure_threshold(1));
    /// breaker.record_failure();
    /// breaker.reset();
    /// assert_eq!(breaker.state(), BreakerState::Closed);
    /// ```
    pub fn reset(&self) {
        self.record_success();
        self.open_until_ms.store(0, Ordering::Release);
    }

    /// Open the breaker for a jittered cooldown.
    fn open(&self) {
        let cooldown = self.cooldown_ms.load(Ordering::Acquire);
        let jittered = cooldown.saturating_add(self.jitter(cooldown));
        let now = u64::try_from(self.base.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.open_until_ms
            .store(now.saturating_add(jittered), Ordering::Release);
        self.state
            .store(u32::from(BreakerState::Open.code()), Ordering::Release);
    }

    /// A pseudo-random 0..=`jitter_percent`% of `cooldown`, in milliseconds.
    ///
    /// An xorshift rather than a `rand` dependency: this is decorrelation, not
    /// cryptography, and one more crate in the tree for eight lines of shift
    /// is a bad trade (`docs/00-foundations/03-crate-layout.md`, rule 6).
    fn jitter(&self, cooldown: u64) -> u64 {
        if self.config.jitter_percent == 0 || cooldown == 0 {
            return 0;
        }
        let mut state = self.seed.load(Ordering::Relaxed);
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        self.seed.store(state, Ordering::Relaxed);

        let span = cooldown.saturating_mul(u64::from(self.config.jitter_percent)) / 100;
        if span == 0 { 0 } else { state % (span + 1) }
    }
}

/// A non-zero starting state for the xorshift, from the wall clock.
fn seed_from_clock() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0x9E37_79B9_7F4A_7C15_u64, |since| {
            u64::from(since.subsec_nanos())
        });
    // xorshift is stuck at zero, so never start there.
    nanos.wrapping_mul(0x2545_F491_4F6C_DD1D) | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn breaker(threshold: u32, cooldown_ms: u64) -> Breaker {
        Breaker::new(
            BreakerConfig::default()
                .failure_threshold(threshold)
                .cooldown(Duration::from_millis(cooldown_ms))
                .max_cooldown(Duration::from_millis(cooldown_ms * 8))
                .jitter_percent(0),
        )
    }

    #[test]
    fn it_opens_only_after_the_threshold() {
        let breaker = breaker(3, 50);
        for _ in 0..2 {
            breaker.record_failure();
            assert!(breaker.allow().is_ok());
        }
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Open);
        assert!(breaker.allow().is_err());
    }

    #[test]
    fn a_success_resets_the_run() {
        let breaker = breaker(3, 50);
        breaker.record_failure();
        breaker.record_failure();
        breaker.record_success();
        breaker.record_failure();
        breaker.record_failure();
        assert!(breaker.allow().is_ok(), "the run restarted");
    }

    #[test]
    fn exactly_one_caller_becomes_the_probe() {
        let breaker = breaker(1, 0);
        breaker.record_failure();
        assert_eq!(breaker.state(), BreakerState::Open);

        // The cooldown is zero, so the first caller is let through and the
        // rest are not.
        assert!(breaker.allow().is_ok());
        assert_eq!(breaker.state(), BreakerState::HalfOpen);
        for _ in 0..10 {
            assert!(breaker.allow().is_err());
        }
    }

    #[test]
    fn a_successful_probe_closes_the_breaker() {
        let breaker = breaker(1, 0);
        breaker.record_failure();
        breaker.allow().expect("the probe goes through");
        breaker.record_success();
        assert_eq!(breaker.state(), BreakerState::Closed);
        assert!(breaker.allow().is_ok());
    }

    #[test]
    fn a_failed_probe_doubles_the_cooldown_up_to_the_ceiling() {
        let breaker = breaker(1, 10);
        breaker.record_failure();
        assert_eq!(breaker.cooldown_ms.load(Ordering::Acquire), 10);

        for expected in [20, 40, 80, 80, 80] {
            std::thread::sleep(Duration::from_millis(1));
            // Wind the clock forward by forcing the cooldown to have elapsed.
            breaker.open_until_ms.store(0, Ordering::Release);
            breaker.allow().expect("probe");
            breaker.record_failure();
            assert_eq!(breaker.cooldown_ms.load(Ordering::Acquire), expected);
        }
    }

    #[test]
    fn a_breaker_that_never_opens_never_opens() {
        let breaker = Breaker::new(BreakerConfig::never());
        for _ in 0..10_000 {
            breaker.record_failure();
        }
        assert!(breaker.allow().is_ok());
        assert_eq!(breaker.state(), BreakerState::Closed);
    }

    #[test]
    fn reset_forces_it_closed() {
        let breaker = breaker(1, 10_000);
        breaker.record_failure();
        assert!(breaker.allow().is_err());
        breaker.reset();
        assert!(breaker.allow().is_ok());
        assert_eq!(breaker.remaining(), Duration::ZERO);
    }

    #[test]
    fn the_jitter_stays_inside_its_percentage_and_moves() {
        let breaker = Breaker::new(
            BreakerConfig::default()
                .cooldown(Duration::from_millis(1_000))
                .jitter_percent(25),
        );

        let mut seen = std::collections::HashSet::new();
        for _ in 0..200 {
            let value = breaker.jitter(1_000);
            assert!(value <= 250, "{value} is over 25% of 1000");
            seen.insert(value);
        }
        assert!(seen.len() > 1, "the jitter never moved");
    }

    #[test]
    fn zero_jitter_is_zero() {
        let breaker = breaker(1, 1_000);
        assert_eq!(breaker.jitter(1_000), 0);
    }

    #[test]
    fn a_state_names_itself_and_round_trips() {
        for state in [
            BreakerState::Closed,
            BreakerState::Open,
            BreakerState::HalfOpen,
        ] {
            assert_eq!(BreakerState::from_code(state.code()), state);
            assert!(!state.as_str().is_empty());
        }
        assert_eq!(BreakerState::from_code(99), BreakerState::Closed);
    }

    #[test]
    fn the_seed_is_never_zero() {
        for _ in 0..100 {
            assert_ne!(seed_from_clock(), 0);
        }
    }
}
