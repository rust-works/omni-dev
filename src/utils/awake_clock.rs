//! A monotonic clock that stands still while the machine sleeps.
//!
//! The liveness TTLs of the daemon's in-memory registries
//! ([`crate::sessions`], [`crate::worktrees`]) are measured on it, so each
//! registry owns one [`AwakeClock`] and stamps its entries with a reading.

use std::time::{Duration, Instant};

/// A monotonic clock that stands still while the machine sleeps, so a TTL
/// measured on it counts only the time the daemon could actually have heard
/// from a client (#2108, #2126).
///
/// The wall clock is the wrong ruler for a liveness TTL: while the system sleeps
/// nothing refreshes an entry — no hooks, no heartbeats, no pid watcher — so the
/// first read after a wake-up used to find every entry "stale" and reap the lot.
/// [`Instant`] does not advance across suspend on the platforms the daemon runs
/// on (`CLOCK_UPTIME_RAW` on macOS, `CLOCK_MONOTONIC` on Linux), so entries
/// stamped with it survive a sleep of any length and are reaped only after a
/// TTL's worth of *awake* silence.
///
/// Stamps are a [`Duration`] since the clock was created rather than a raw
/// [`Instant`], so tests can move the clock forward without subtracting from an
/// `Instant` (which panics on a host with little uptime). Readings from
/// different clocks are not comparable: each registry compares only its own.
#[derive(Debug)]
pub(crate) struct AwakeClock {
    /// The clock's creation, the zero of every stamp.
    base: Instant,
    /// Test-only extra elapsed time, in milliseconds.
    #[cfg(test)]
    skew_ms: std::sync::atomic::AtomicU64,
}

impl AwakeClock {
    pub(crate) fn new() -> Self {
        Self {
            base: Instant::now(),
            #[cfg(test)]
            skew_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Awake time elapsed since the clock was created.
    pub(crate) fn now(&self) -> Duration {
        let elapsed = self.base.elapsed();
        #[cfg(test)]
        let elapsed = elapsed
            + Duration::from_millis(self.skew_ms.load(std::sync::atomic::Ordering::Relaxed));
        elapsed
    }

    /// Moves the clock forward, as if the machine had been awake that long.
    #[cfg(test)]
    pub(crate) fn advance(&self, by: Duration) {
        self.skew_ms.fetch_add(
            u64::try_from(by.as_millis()).unwrap_or(u64::MAX),
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_new_clock_starts_near_zero() {
        let clock = AwakeClock::new();
        assert!(clock.now() < Duration::from_secs(60));
    }

    #[test]
    fn now_never_runs_backwards() {
        let clock = AwakeClock::new();
        let first = clock.now();
        let second = clock.now();
        assert!(second >= first);
    }

    #[test]
    fn advance_moves_now_forward_by_at_least_that_much() {
        let clock = AwakeClock::new();
        let before = clock.now();
        clock.advance(Duration::from_secs(120));
        assert!(clock.now() >= before + Duration::from_secs(120));
    }

    #[test]
    fn advances_accumulate() {
        let clock = AwakeClock::new();
        clock.advance(Duration::from_secs(10));
        clock.advance(Duration::from_secs(20));
        assert!(clock.now() >= Duration::from_secs(30));
    }

    #[test]
    fn an_absurd_advance_saturates_instead_of_wrapping() {
        let clock = AwakeClock::new();
        clock.advance(Duration::MAX);
        // `u64::MAX` ms is ~584 million years; the point is only that the
        // conversion saturates rather than panicking or wrapping to a small value.
        assert!(clock.now() >= Duration::from_secs(1_000_000));
    }
}
