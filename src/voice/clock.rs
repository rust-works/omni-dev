//! Pluggable wall clock for deterministic timestamps in tests.
//!
//! Mirrors the [`UlidRng`](super::det::UlidRng) pattern: production code
//! uses [`SystemClock`], tests inject [`FixedClock`] so snapshot output is
//! byte-stable.

use chrono::{DateTime, Utc};

/// Source of wall-clock time.
pub trait Clock: Send + Sync {
    /// Returns the current UTC time.
    fn now(&self) -> DateTime<Utc>;
}

/// Production clock: defers to `Utc::now()`.
#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Test clock: always returns the same instant.
#[derive(Debug, Clone)]
pub struct FixedClock(pub DateTime<Utc>);

impl FixedClock {
    /// Parses an RFC3339 timestamp into a fixed clock. Panics if `s` is
    /// not a valid RFC3339 string — intended for test-only use, where a
    /// hard-coded constant string is the input.
    #[must_use]
    #[allow(clippy::expect_used, clippy::missing_panics_doc)]
    pub fn from_rfc3339(s: &str) -> Self {
        let dt = DateTime::parse_from_rfc3339(s)
            .expect("FixedClock::from_rfc3339: invalid RFC3339 timestamp")
            .with_timezone(&Utc);
        Self(dt)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_clock_returns_same_instant_repeatedly() {
        let clock = FixedClock::from_rfc3339("2026-01-01T00:00:00Z");
        let a = clock.now();
        let b = clock.now();
        assert_eq!(a, b);
    }

    #[test]
    fn fixed_clock_from_rfc3339_parses_utc() {
        let clock = FixedClock::from_rfc3339("2026-01-01T00:00:00Z");
        assert_eq!(clock.0.to_rfc3339(), "2026-01-01T00:00:00+00:00");
    }

    #[test]
    fn system_clock_returns_recent_time() {
        let clock = SystemClock;
        let now = clock.now();
        let real_now = Utc::now();
        let delta = (real_now - now).num_seconds().abs();
        assert!(delta < 2, "SystemClock should be close to Utc::now()");
    }
}
