//! Accept-error handling for the control-socket loop (#2111).
//!
//! When the daemon runs out of descriptors (`EMFILE`/`ENFILE`) `accept` fails
//! *without* consuming the pending connection, so a loop that retries at once
//! spins on the same failure — burning a core, flooding the log, and leaving
//! every client queued in the backlog. [`AcceptBackoff`] is the pure state
//! machine that paces the retries and decides what to log; the loop in
//! [`server`](super::server) owns the sleeping, so this module needs no clock of
//! its own and its tests need no real delays.

use std::time::{Duration, Instant};

use tokio::net::{UnixListener, UnixStream};

/// The first retry delay after an `accept` error.
const INITIAL_DELAY: Duration = Duration::from_millis(5);

/// The longest the loop waits between retries. Descriptors free up as in-flight
/// requests finish, so a short ceiling keeps recovery prompt while still cutting
/// the retry rate from unbounded to one per second.
const MAX_DELAY: Duration = Duration::from_secs(1);

/// The minimum gap between two warnings in one error burst. The first error of a
/// burst is always logged; the rest are throttled so a sustained outage reads as
/// a handful of lines instead of one per retry.
const LOG_INTERVAL: Duration = Duration::from_secs(30);

/// Where the server loop gets its connections from.
///
/// A real [`UnixListener`] in production. It is a trait so tests can script the
/// error sequence — an `EMFILE` cannot be provoked on demand without lowering
/// the test process's own descriptor limit.
#[async_trait::async_trait]
pub(crate) trait ConnectionSource: Send + Sync {
    /// Waits for the next connection, or for an I/O error.
    async fn accept(&self) -> std::io::Result<UnixStream>;
}

#[async_trait::async_trait]
impl ConnectionSource for UnixListener {
    async fn accept(&self) -> std::io::Result<UnixStream> {
        // Inherent method, not this trait method: `Type::method` path syntax
        // prefers the inherent one, so this does not recurse.
        Self::accept(self).await.map(|(stream, _addr)| stream)
    }
}

/// What the loop should do after a failed `accept`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FailureStep {
    /// How long to wait before the next `accept`.
    pub delay: Duration,
    /// Whether this failure should be logged.
    pub log: bool,
    /// How long the daemon has been unable to accept since the last time this was
    /// reported (zero on the burst's first failure). The loop credits it to the
    /// registries, so a window is not aged out for heartbeats the daemon could
    /// not hear.
    pub outage: Duration,
}

/// The end of an error burst, reported by the first `accept` that works again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Recovery {
    /// How many consecutive `accept` calls failed.
    pub failures: u32,
    /// How long the burst lasted, first failure to this success.
    pub duration: Duration,
    /// The stretch since the last [`FailureStep::outage`] report.
    pub outage: Duration,
}

/// Paces `accept` retries and tracks one error burst at a time.
#[derive(Debug, Default)]
pub(crate) struct AcceptBackoff {
    burst: Option<Burst>,
}

/// State for the burst in progress.
#[derive(Debug)]
struct Burst {
    started: Instant,
    failures: u32,
    next_delay: Duration,
    last_logged: Option<Instant>,
    /// The instant up to which the outage has already been reported.
    reported_to: Instant,
}

impl AcceptBackoff {
    /// A backoff with no burst in progress.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Records a failed `accept` at `now`.
    pub(crate) fn on_failure(&mut self, now: Instant) -> FailureStep {
        let burst = self.burst.get_or_insert(Burst {
            started: now,
            failures: 0,
            next_delay: INITIAL_DELAY,
            last_logged: None,
            reported_to: now,
        });
        burst.failures += 1;
        let delay = burst.next_delay;
        burst.next_delay = (delay * 2).min(MAX_DELAY);
        let log = burst
            .last_logged
            .is_none_or(|at| now.saturating_duration_since(at) >= LOG_INTERVAL);
        if log {
            burst.last_logged = Some(now);
        }
        let outage = now.saturating_duration_since(burst.reported_to);
        burst.reported_to = now;
        FailureStep { delay, log, outage }
    }

    /// Records a successful `accept` at `now`. Returns the finished burst when
    /// one was in progress, and resets so the next failure starts a fresh one.
    pub(crate) fn on_success(&mut self, now: Instant) -> Option<Recovery> {
        let burst = self.burst.take()?;
        Some(Recovery {
            failures: burst.failures,
            duration: now.saturating_duration_since(burst.started),
            outage: now.saturating_duration_since(burst.reported_to),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The trait impl must delegate to the listener's own `accept`, not call itself.
    #[tokio::test]
    async fn a_unix_listener_yields_the_connection_a_client_makes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let _client = UnixStream::connect(&path).await.unwrap();

        let accepted =
            tokio::time::timeout(Duration::from_secs(2), ConnectionSource::accept(&listener))
                .await
                .expect("accept should not hang or recurse");
        assert!(accepted.is_ok());
    }

    fn at(base: Instant, millis: u64) -> Instant {
        base + Duration::from_millis(millis)
    }

    #[test]
    fn delay_doubles_from_the_initial_value_up_to_the_cap() {
        let base = Instant::now();
        let mut backoff = AcceptBackoff::new();
        let delays: Vec<Duration> = (0..12)
            .map(|i| backoff.on_failure(at(base, i)).delay)
            .collect();
        assert_eq!(delays[0], INITIAL_DELAY);
        assert_eq!(delays[1], INITIAL_DELAY * 2);
        assert_eq!(delays[2], INITIAL_DELAY * 4);
        // 5ms * 2^8 = 1.28s, so the cap bites from the ninth retry on and stays.
        assert_eq!(delays[8], MAX_DELAY);
        assert_eq!(delays[11], MAX_DELAY);
        assert!(delays.windows(2).all(|pair| pair[0] <= pair[1]));
    }

    #[test]
    fn a_success_ends_the_burst_and_the_next_failure_starts_from_the_initial_delay() {
        let base = Instant::now();
        let mut backoff = AcceptBackoff::new();
        for i in 0..5 {
            backoff.on_failure(at(base, i));
        }
        assert!(backoff.on_success(at(base, 100)).is_some());
        let step = backoff.on_failure(at(base, 200));
        assert_eq!(step.delay, INITIAL_DELAY);
        assert!(step.log, "a new burst logs its first failure again");
    }

    #[test]
    fn a_success_with_no_burst_in_progress_reports_nothing() {
        assert_eq!(AcceptBackoff::new().on_success(Instant::now()), None);
    }

    #[test]
    fn only_the_first_failure_in_a_log_interval_is_logged() {
        let base = Instant::now();
        let mut backoff = AcceptBackoff::new();
        assert!(backoff.on_failure(base).log);
        assert!(!backoff.on_failure(at(base, 1_000)).log);
        assert!(!backoff.on_failure(at(base, 29_999)).log);
        // A full interval after the last logged failure, one reminder is due.
        assert!(backoff.on_failure(at(base, 30_000)).log);
        assert!(!backoff.on_failure(at(base, 30_001)).log);
    }

    #[test]
    fn outage_is_reported_incrementally_and_sums_to_the_whole_burst() {
        let base = Instant::now();
        let mut backoff = AcceptBackoff::new();
        let first = backoff.on_failure(at(base, 1_000));
        assert_eq!(first.outage, Duration::ZERO, "nothing has elapsed yet");
        let second = backoff.on_failure(at(base, 1_005));
        let third = backoff.on_failure(at(base, 1_015));
        let recovery = backoff.on_success(at(base, 1_045)).unwrap();
        assert_eq!(second.outage, Duration::from_millis(5));
        assert_eq!(third.outage, Duration::from_millis(10));
        assert_eq!(recovery.outage, Duration::from_millis(30));
        assert_eq!(
            first.outage + second.outage + third.outage + recovery.outage,
            Duration::from_millis(45),
            "the credits cover the burst exactly once"
        );
    }

    #[test]
    fn recovery_reports_the_failure_count_and_burst_duration() {
        let base = Instant::now();
        let mut backoff = AcceptBackoff::new();
        for i in 0..4 {
            backoff.on_failure(at(base, i * 10));
        }
        let recovery = backoff.on_success(at(base, 100)).unwrap();
        assert_eq!(recovery.failures, 4);
        assert_eq!(recovery.duration, Duration::from_millis(100));
    }

    #[test]
    fn a_clock_that_runs_backwards_never_panics_or_goes_negative() {
        let base = Instant::now();
        let mut backoff = AcceptBackoff::new();
        backoff.on_failure(at(base, 100));
        let step = backoff.on_failure(base);
        assert_eq!(step.outage, Duration::ZERO);
        assert_eq!(backoff.on_success(base).unwrap().outage, Duration::ZERO);
    }
}
