//! A small, fast, non-cryptographic random source for jitter.
//!
//! Jitter is not optional — a thousand jobs that failed together and retry
//! together are a thundering herd against whatever failed — but it is also not
//! a security primitive, so this crate does not take a dependency on `rand` for
//! it. `SplitMix64` is ten lines, seeded once per process from
//! [`uuid::Uuid::new_v4`], which is the operating system's entropy pool by way
//! of a dependency this crate already has.
//!
//! Everything here is deliberately private: a public random-number generator on
//! a jobs crate is an attractive nuisance.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The generator's state, advanced with a relaxed fetch-update.
///
/// One shared counter and not a thread-local: the contention is a single
/// uncontended atomic per retry, and a thread-local would give two workers on
/// two threads correlated jitter after a fork.
static STATE: AtomicU64 = AtomicU64::new(0);

/// Seeds the state on first use, from the operating system.
fn seed() -> u64 {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut seed = [0_u8; 8];
    seed.copy_from_slice(&bytes[..8]);
    // A zero seed would make SplitMix64 emit a fixed stream, and a v4 UUID's
    // first eight bytes are all random, so this only fires on a broken source.
    u64::from_le_bytes(seed) | 1
}

/// SplitMix64's golden-ratio increment.
const GAMMA: u64 = 0x9E37_79B9_7F4A_7C15;

/// The next value in the stream.
fn next_u64() -> u64 {
    // Seeded lazily rather than in a `OnceLock`: the first caller wins the
    // compare-exchange and everybody else just adds, so the steady state is one
    // relaxed `fetch_add` with no branch that can ever be taken twice.
    if STATE.load(Ordering::Relaxed) == 0 {
        let _ = STATE.compare_exchange(0, seed(), Ordering::Relaxed, Ordering::Relaxed);
    }
    let mut z = STATE
        .fetch_add(GAMMA, Ordering::Relaxed)
        .wrapping_add(GAMMA);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A uniform sample in `0..bound`, or zero when `bound` is zero.
///
/// Lemire's multiply-shift rather than a modulo: the bias of `% bound` is
/// invisible for small bounds and grows with them, and a backoff bound is a
/// number of milliseconds that can reach into the millions.
pub(crate) fn below(bound: u64) -> u64 {
    if bound == 0 {
        return 0;
    }
    let product = u128::from(next_u64()) * u128::from(bound);
    (product >> 64) as u64
}

/// A uniform sample in `[0, span]`, saturating rather than overflowing.
///
/// Full jitter, as `Backoff::delay_jittered` documents: uniform across the
/// whole interval rather than a narrow band around the nominal delay, because a
/// narrow band does not break up a herd.
pub(crate) fn duration_below(span: Duration) -> Duration {
    let millis = u64::try_from(span.as_millis()).unwrap_or(u64::MAX);
    Duration::from_millis(below(millis.saturating_add(1)))
}

/// A short lowercase-hex suffix, for a worker identifier.
pub(crate) fn hex_suffix() -> String {
    format!("{:08x}", next_u64() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A generator that returns the same number twice in a row is not jittering
    /// anything, and a herd is exactly what it was meant to break up.
    #[test]
    fn the_stream_does_not_repeat_itself() {
        let sample: Vec<u64> = (0..64).map(|_| next_u64()).collect();
        let unique: std::collections::BTreeSet<u64> = sample.iter().copied().collect();
        assert_eq!(unique.len(), sample.len(), "64 draws collided");
    }

    /// The bound is exclusive, and a zero bound must not divide by zero.
    #[test]
    fn samples_stay_inside_their_bound() {
        for _ in 0..2_000 {
            assert!(below(10) < 10);
        }
        assert_eq!(below(0), 0);
        assert_eq!(below(1), 0);
    }

    /// Full jitter means the *whole* interval is reachable, not a band around
    /// the middle of it. Two thousand draws over ten buckets makes an empty
    /// bucket a real signal rather than bad luck.
    #[test]
    fn full_jitter_covers_the_whole_interval() {
        let mut buckets = [0_u32; 10];
        for _ in 0..2_000 {
            let sample = duration_below(Duration::from_millis(999));
            let index = usize::try_from(sample.as_millis() / 100)
                .unwrap_or(9)
                .min(9);
            buckets[index] += 1;
        }
        assert!(
            buckets.iter().all(|count| *count > 0),
            "an empty decile means the jitter is not uniform: {buckets:?}"
        );
    }

    /// A suffix that collides makes two pods indistinguishable in the
    /// dashboard, which is the one thing the suffix exists to prevent.
    #[test]
    fn worker_suffixes_are_eight_hex_digits_and_differ() {
        let first = hex_suffix();
        let second = hex_suffix();
        assert_eq!(first.len(), 8);
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(first, second);
    }
}
