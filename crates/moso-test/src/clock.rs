//! A clock a test can move.
//!
//! # Why this is here and not in `moso-core`
//!
//! `43-testing.md` specifies `app.advance_time(Duration)` as affecting "TTLs,
//! schedules, delayed jobs, `now()`". That only works if every one of those
//! reads the time through one indirection the harness can replace — and
//! `moso-core` has no such indirection today: `moso-core` reads
//! `std::time::Instant::now()` and `SystemTime::now()` directly at each site.
//!
//! So this module supplies the clock and the harness registers it as a provider,
//! which makes it work for **application** code (`Inject<TestClock>`), and
//! reports the gap for framework code. When `moso::time::now()` lands,
//! [`TestApp::advance_time`](crate::TestApp::advance_time) should drive it and
//! this type becomes its test implementation rather than a parallel one.
//!
//! # Relationship with Tokio's clock
//!
//! Tokio has its own pausable clock, which is what actually makes
//! `tokio::time::sleep` return early. It can only be advanced from a
//! current-thread runtime with time paused, so the harness will not touch it
//! unless the test says it may — see
//! [`TestAppBuilder::paused_time`](crate::TestAppBuilder::paused_time).

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, SystemTime};

/// A wall clock whose offset from the real one a test controls.
///
/// Cheap to clone: every clone reads and writes the same offset, so advancing
/// the handle a test holds advances the one the application injected.
///
/// ```
/// use moso_test::TestClock;
/// use std::time::Duration;
///
/// let clock = TestClock::new();
/// let start = clock.now();
///
/// // Advancing the handle a test holds advances the one the application injected.
/// let injected = clock.clone();
/// clock.advance(Duration::from_secs(60));
///
/// assert_eq!(injected.offset(), Duration::from_secs(60));
/// assert!(injected.now() > start);
///
/// clock.rewind(Duration::from_secs(30));
/// assert_eq!(clock.offset(), Duration::from_secs(30));
/// ```
///
/// Register it with `App::new(cfg).provide(clock.clone())` so a handler taking
/// `Inject<TestClock>` reads the time the test controls. Expiry, rate limits and
/// scheduled work then become assertions instead of sleeps.
#[derive(Clone, Debug)]
pub struct TestClock {
    inner: Arc<ClockInner>,
}

#[derive(Debug)]
struct ClockInner {
    base: SystemTime,
    /// Signed offset in milliseconds, so a test can also rewind.
    offset_ms: AtomicI64,
}

impl TestClock {
    /// A clock reading the current wall time.
    #[must_use]
    pub fn new() -> Self {
        Self::at(SystemTime::now())
    }

    /// A clock reading `base` until it is moved.
    ///
    /// Pin it to a fixed instant when the test asserts on a rendered timestamp:
    /// a test that formats `now()` and compares strings is otherwise a test that
    /// fails once a year at midnight.
    #[must_use]
    pub fn at(base: SystemTime) -> Self {
        Self {
            inner: Arc::new(ClockInner {
                base,
                offset_ms: AtomicI64::new(0),
            }),
        }
    }

    /// The instant this clock currently reads.
    #[must_use]
    pub fn now(&self) -> SystemTime {
        let offset = self.inner.offset_ms.load(Ordering::Relaxed);
        let magnitude = Duration::from_millis(offset.unsigned_abs());
        if offset >= 0 {
            self.inner.base + magnitude
        } else {
            self.inner
                .base
                .checked_sub(magnitude)
                .unwrap_or(SystemTime::UNIX_EPOCH)
        }
    }

    /// The instant the clock started from, whatever it has been moved to since.
    #[must_use]
    pub fn base(&self) -> SystemTime {
        self.inner.base
    }

    /// Move the clock forward.
    pub fn advance(&self, by: Duration) {
        self.shift(millis(by));
    }

    /// Move the clock backward.
    ///
    /// Present because "what happens when a client's clock is behind ours" is a
    /// real test, not because time travel is a good idea.
    pub fn rewind(&self, by: Duration) {
        self.shift(-millis(by));
    }

    /// Jump to an absolute instant.
    pub fn set(&self, at: SystemTime) {
        let offset = match at.duration_since(self.inner.base) {
            Ok(ahead) => millis(ahead),
            Err(behind) => -millis(behind.duration()),
        };
        self.inner.offset_ms.store(offset, Ordering::Relaxed);
    }

    /// How far the clock has been moved, and in which direction.
    #[must_use]
    pub fn offset(&self) -> Duration {
        Duration::from_millis(self.inner.offset_ms.load(Ordering::Relaxed).unsigned_abs())
    }

    /// Whether [`offset`](Self::offset) is in the past.
    #[must_use]
    pub fn is_rewound(&self) -> bool {
        self.inner.offset_ms.load(Ordering::Relaxed) < 0
    }

    fn shift(&self, by: i64) {
        // Saturating rather than wrapping: a test that adds a century twice
        // should read "a very long time from now", not "1970".
        let _ =
            self.inner
                .offset_ms
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                    Some(current.saturating_add(by))
                });
    }
}

impl Default for TestClock {
    fn default() -> Self {
        Self::new()
    }
}

/// A `Duration` as signed milliseconds, saturating rather than overflowing.
fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epoch_clock() -> TestClock {
        TestClock::at(SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000))
    }

    #[test]
    fn a_fresh_clock_reads_its_base() {
        let clock = epoch_clock();
        assert_eq!(clock.now(), clock.base());
        assert_eq!(clock.offset(), Duration::ZERO);
    }

    #[test]
    fn advancing_moves_every_clone() {
        let clock = epoch_clock();
        let injected = clock.clone();
        clock.advance(Duration::from_secs(3600));
        assert_eq!(injected.now(), clock.base() + Duration::from_secs(3600));
    }

    #[test]
    fn advancing_twice_accumulates() {
        let clock = epoch_clock();
        clock.advance(Duration::from_secs(10));
        clock.advance(Duration::from_secs(5));
        assert_eq!(clock.offset(), Duration::from_secs(15));
    }

    #[test]
    fn rewinding_reads_before_the_base() {
        let clock = epoch_clock();
        clock.rewind(Duration::from_secs(60));
        assert!(clock.is_rewound());
        assert_eq!(clock.now(), clock.base() - Duration::from_secs(60));
    }

    #[test]
    fn setting_an_absolute_instant_ahead_of_the_base() {
        let clock = epoch_clock();
        let target = clock.base() + Duration::from_secs(90);
        clock.set(target);
        assert_eq!(clock.now(), target);
    }

    #[test]
    fn setting_an_absolute_instant_behind_the_base() {
        let clock = epoch_clock();
        let target = clock.base() - Duration::from_secs(90);
        clock.set(target);
        assert_eq!(clock.now(), target);
        assert!(clock.is_rewound());
    }

    #[test]
    fn an_absurd_advance_saturates_instead_of_overflowing() {
        let clock = epoch_clock();
        clock.advance(Duration::from_secs(u64::MAX / 2));
        clock.advance(Duration::from_secs(u64::MAX / 2));
        // The point is that neither call panicked and the clock still reads
        // forwards.
        assert!(!clock.is_rewound());
        assert!(clock.now() > clock.base());
    }
}
