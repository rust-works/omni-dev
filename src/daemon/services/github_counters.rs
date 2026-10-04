//! A [`DaemonService`] that periodically logs a summary of omni-dev's GitHub
//! API-call counters (#1387), and surfaces them in `daemon status`.
//!
//! The counters themselves are recorded by `crate::github_metrics::run_gh` at
//! every `gh` call site (across the daemon *and* one-shot CLI processes) as
//! `kind: "gh"` request-log records. This service just reads them back and emits
//! a `tracing` summary at three points, so there is a periodic footprint in the
//! log and a clean before/after marker across restarts without anyone running a
//! command:
//!
//! - **~5s after startup** — a baseline once the pollers have come up.
//! - **every 10 minutes** thereafter — a background task on a private
//!   [`CancellationToken`], mirroring the worktrees poller shape.
//! - **once on shutdown** — from [`shutdown`](DaemonService::shutdown), the
//!   deterministic, awaited flush that `registry.shutdown_all()` drives after the
//!   accept loop drains. That single hook covers SIGTERM / SIGINT / SIGHUP and
//!   the built-in `shutdown` op, since all of them funnel through the one shared
//!   token that ends the accept loop.
//!
//! Every emission is best-effort and cheap, however large the request log has
//! grown (it is unbounded unless rotation or `prune` is opted into). The service
//! keeps a running [`IncrementalCounts`] from the moment it starts, so each
//! summary reads only the records appended since the previous one, never the
//! log's history (#2132). A size-capped rotation is followed through the
//! numbered files, so the tally keeps its counts (#2162); a log that is pruned,
//! truncated or rewritten underneath it, or whose file was rotated out of
//! retention, is rescanned once, from its first byte.
//!
//! Shutdown never waits on a scan beyond `SHUTDOWN_SCAN_BUDGET`. A blocking
//! thread cannot be cancelled from outside, and the runtime waits for one before
//! the process may exit, so the scan itself polls a stop condition before every
//! line: shutdown cancels it for every scan already in flight, and gives the
//! final summary a deadline. A summary cut short says so rather than reporting a
//! partial count as complete.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::service::{DaemonService, MenuItem, MenuSnapshot, ServiceStatus};
use crate::github_metrics::{GhCounts, IncrementalCounts};
use crate::request_log;

/// Delay before the first ("startup") summary, so the pollers have come up.
const STARTUP_DELAY: Duration = Duration::from_secs(5);
/// Interval between periodic summaries after the first one.
const PERIODIC_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// The longest the final summary may spend reading the log. Normally it reads a
/// few minutes of records and finishes in milliseconds; this only bites when the
/// log was replaced and has to be rescanned, or rotated and the rotated files have
/// to be read.
const SHUTDOWN_SCAN_BUDGET: Duration = Duration::from_secs(2);

/// The running tally, shared by the logger task, `status`, `summary` and
/// `shutdown`. Locked only on a blocking thread, never across an `.await`.
///
/// A caller that arrives while another is reading waits for it, which is cheap
/// unless the log was just replaced: that read rebuilds the tally from the new
/// file and so can take a while. Shutdown is not held up by it, since cancelling
/// `stopping` makes the reader in flight stop at its next line.
type SharedTally = Arc<Mutex<IncrementalCounts>>;

/// Periodically logs, and reports on demand, the GitHub API-call counters.
pub struct GithubCountersService {
    /// When the daemon started, so every summary reports calls **since boot** —
    /// a clean before/after marker that resets across restarts.
    started_at: DateTime<Utc>,
    /// Counts of the `gh` records logged since `started_at`; `None` when no log
    /// path resolves, so there is nothing to count.
    tally: Option<SharedTally>,
    /// Cancelled when shutdown begins: ends the logger and abandons every scan in
    /// flight, so none of them delays the final summary.
    stopping: CancellationToken,
    /// The periodic-summary task (idempotent start; `None` until started).
    logger: Mutex<Option<JoinHandle<()>>>,
}

impl Default for GithubCountersService {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubCountersService {
    /// Cheap construction (like the worktrees/sessions services): captures the
    /// start time and the request log's current extent, reads none of its
    /// records, and persists nothing. Call [`Self::start_counter_logger`] to
    /// spawn the periodic task once inside the tokio runtime.
    #[must_use]
    pub fn new() -> Self {
        Self::with_log_path(request_log::log_file_path())
    }

    /// [`new`](Self::new) over an explicit request log, so tests do not read the
    /// developer's real one.
    fn with_log_path(path: Option<PathBuf>) -> Self {
        let (started_at, tally) = match path {
            Some(path) => {
                let tally = IncrementalCounts::start(path);
                (tally.since(), Some(Arc::new(Mutex::new(tally))))
            }
            None => (Utc::now(), None),
        };
        Self {
            started_at,
            tally,
            stopping: CancellationToken::new(),
            logger: Mutex::new(None),
        }
    }

    /// Spawns the ~5s-then-every-10-min summary task. Idempotent and a no-op
    /// outside a tokio runtime (unit tests), mirroring the worktrees pollers.
    pub fn start_counter_logger(&self) {
        self.start_counter_logger_with(STARTUP_DELAY, PERIODIC_INTERVAL);
    }

    /// [`start_counter_logger`](Self::start_counter_logger) with explicit
    /// cadences, so tests can drive it at millisecond speed.
    fn start_counter_logger_with(&self, startup_delay: Duration, interval: Duration) {
        if tokio::runtime::Handle::try_current().is_err() {
            tracing::debug!("no tokio runtime; github counter logger not started");
            return;
        }
        let mut guard = self.logger.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_some() {
            return;
        }
        let stopping = self.stopping.clone();
        let tally = self.tally.clone();
        *guard = Some(tokio::spawn(async move {
            // Wait for the baseline delay, but exit immediately if the daemon is
            // already shutting down (shutdown() emits the final summary itself).
            tokio::select! {
                () = stopping.cancelled() => return,
                () = tokio::time::sleep(startup_delay) => {}
            }
            emit_background(&tally, "startup", &stopping).await;
            loop {
                tokio::select! {
                    () = stopping.cancelled() => break,
                    () = tokio::time::sleep(interval) => {
                        emit_background(&tally, "periodic", &stopping).await;
                    }
                }
            }
        }));
    }
}

/// A [`refresh`](IncrementalCounts::refresh) stop condition that fires once
/// `token` is cancelled.
fn cancelled_by(token: &CancellationToken) -> impl FnMut() -> bool + Send + 'static {
    let token = token.clone();
    move || token.is_cancelled()
}

/// Brings the tally up to date and returns the counts since boot, and whether
/// they are current — `false` when `stop` cut the read short or the log could
/// not be read. Best-effort: with no tally (no log path) the counts are zero.
///
/// The read runs on a blocking thread so it never stalls the async executor.
async fn counts_since_boot(
    tally: &Option<SharedTally>,
    stop: impl FnMut() -> bool + Send + 'static,
) -> (GhCounts, bool) {
    let Some(tally) = tally else {
        return (GhCounts::default(), true);
    };
    let reader = Arc::clone(tally);
    let read = tokio::task::spawn_blocking(move || {
        let mut tally = reader.lock().unwrap_or_else(PoisonError::into_inner);
        let current = tally.refresh(stop);
        (tally.counts().clone(), current)
    })
    .await;
    match read {
        Ok(counts) => counts,
        Err(e) => {
            tracing::warn!("github counters: the log read did not complete: {e}");
            (last_known(tally), false)
        }
    }
}

/// What the tally last held, for when a read of it failed outright. The tally
/// outlives the failed task, so zeros would be a plausible-looking wrong answer;
/// should the lock still be held, there is nothing better than zeros.
fn last_known(tally: &SharedTally) -> GhCounts {
    match tally.try_lock() {
        Ok(tally) => tally.counts().clone(),
        Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner().counts().clone(),
        Err(TryLockError::WouldBlock) => GhCounts::default(),
    }
}

/// Emits one `tracing::info` summary line for `phase`.
fn log_summary(phase: &str, counts: &GhCounts, current: bool) {
    // Bind before the macro so the summary is computed whenever this runs, not
    // only when an info-level subscriber is installed (the poller idiom).
    let mut summary = counts.summary_line();
    if !current {
        summary.push_str(" (incomplete: the log read was cut short)");
    }
    tracing::info!("github api calls ({phase}): {summary}");
}

/// The logger task's emission: a summary of the counters, dropped if shutdown
/// began while it was reading, since shutdown emits the final summary itself.
async fn emit_background(tally: &Option<SharedTally>, phase: &str, stopping: &CancellationToken) {
    let (counts, current) = counts_since_boot(tally, cancelled_by(stopping)).await;
    if !stopping.is_cancelled() {
        log_summary(phase, &counts, current);
    }
}

#[async_trait]
impl DaemonService for GithubCountersService {
    fn name(&self) -> &'static str {
        "github"
    }

    async fn handle(&self, op: &str, _payload: Value) -> Result<Value> {
        match op {
            // A live socket query of the current counters (since daemon start).
            "summary" => {
                let (counts, current) =
                    counts_since_boot(&self.tally, cancelled_by(&self.stopping)).await;
                let mut value = counts.to_json();
                if let Value::Object(map) = &mut value {
                    map.insert("since".to_string(), json!(self.started_at.to_rfc3339()));
                    if !current {
                        map.insert("incomplete".to_string(), json!(true));
                    }
                }
                Ok(value)
            }
            other => bail!("unknown github op: {other}"),
        }
    }

    fn menu(&self) -> MenuSnapshot {
        // Kept cheap and non-blocking (polled ~1 Hz): no file read here. The live
        // numbers live in `daemon status` and the daemon log.
        MenuSnapshot {
            title: "GitHub API".to_string(),
            items: vec![MenuItem::Label(
                "call counters logged to daemon.log".to_string(),
            )],
        }
    }

    async fn menu_action(&self, _action_id: &str) -> Result<()> {
        Ok(())
    }

    async fn status(&self) -> ServiceStatus {
        let (counts, current) = counts_since_boot(&self.tally, cancelled_by(&self.stopping)).await;
        let mut summary = format!("{} GitHub API call(s) since start", counts.api_total());
        if !current {
            summary.push_str(" (incomplete)");
        }
        ServiceStatus {
            name: self.name().to_string(),
            healthy: true,
            summary,
            detail: counts.to_json(),
        }
    }

    async fn shutdown(&self) {
        // Stop first: this ends the logger loop and abandons any scan in flight
        // (periodic, `status`, `summary`), so nothing ahead of the final summary
        // holds up the exit. Take the logger from under the lock before awaiting,
        // so the std::Mutex is never held across the `.await`.
        self.stopping.cancel();
        let logger = self
            .logger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(logger) = logger {
            let _ = logger.await;
        }
        // The deterministic, awaited flush for every termination path
        // (SIGTERM/SIGINT/SIGHUP + the `shutdown` op), under a deadline of its
        // own: `stopping` is already cancelled and must not cut it short.
        let deadline = Instant::now() + SHUTDOWN_SCAN_BUDGET;
        let (counts, current) =
            counts_since_boot(&self.tally, move || Instant::now() >= deadline).await;
        log_summary("shutdown", &counts, current);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::github_metrics::aggregate;
    use chrono::{Duration as ChronoDuration, SecondsFormat};
    use std::io::Write;
    use std::path::Path;

    /// A `gh` record line stamped `secs` seconds after `since`.
    fn gh_line(since: DateTime<Utc>, secs: i64, command: &[&str]) -> String {
        format!(
            "{{\"kind\":\"gh\",\"timestamp\":\"{}\",\"command\":{},\"source\":\"daemon\"}}\n",
            (since + ChronoDuration::seconds(secs)).to_rfc3339_opts(SecondsFormat::Millis, true),
            serde_json::to_string(command).unwrap(),
        )
    }

    fn append(path: &Path, text: &str) {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
    }

    /// The tally's total, read straight from the shared state.
    fn tallied(svc: &GithubCountersService) -> u64 {
        svc.tally.as_ref().unwrap().lock().unwrap().counts().total()
    }

    #[tokio::test]
    async fn service_reports_status_and_handles_summary() {
        let dir = tempfile::tempdir().unwrap();
        let svc = GithubCountersService::with_log_path(Some(dir.path().join("log.jsonl")));
        assert_eq!(svc.name(), "github");

        // status() reads the log read-only and must be healthy with a well-formed
        // detail object (counts may be empty/absent — we assert shape, not values).
        let status = svc.status().await;
        assert_eq!(status.name, "github");
        assert!(status.healthy);
        assert!(status.detail.get("api_total").is_some());

        // `summary` returns the counts plus a `since` marker; unknown ops error.
        let summary = svc.handle("summary", Value::Null).await.unwrap();
        assert!(summary.get("since").is_some());
        assert!(summary.get("by_source").is_some());
        assert!(summary.get("incomplete").is_none());
        assert!(svc.handle("bogus", Value::Null).await.is_err());

        // menu()/menu_action()/shutdown() are inert but must not panic; shutdown()
        // emits the final summary even though no logger task was started.
        assert_eq!(svc.menu().title, "GitHub API");
        svc.menu_action("x").await.unwrap();
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn without_a_log_path_there_is_nothing_to_count() {
        let svc = GithubCountersService::with_log_path(None);
        let status = svc.status().await;
        assert!(status.healthy);
        assert_eq!(status.summary, "0 GitHub API call(s) since start");
        svc.shutdown().await;
    }

    #[tokio::test]
    async fn counts_only_the_records_appended_after_start() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        // Records already in the log at boot, stamped *inside* the window (the
        // future): a full scan counts every one, so the service reporting none of
        // them means it never read them.
        let future = Utc::now() + ChronoDuration::hours(1);
        let pre: String = (0..50)
            .map(|_| gh_line(future, 0, &["pr", "list"]))
            .collect();
        append(&log, &pre);

        let svc = GithubCountersService::with_log_path(Some(log.clone()));
        assert_eq!(
            aggregate(&log, Some(svc.started_at), None, None).total(),
            50
        );
        assert_eq!(
            svc.status().await.summary,
            "0 GitHub API call(s) since start"
        );

        append(&log, &gh_line(svc.started_at, 1, &["api", "graphql"]));
        append(&log, &gh_line(svc.started_at, 2, &["pr", "view"]));
        let status = svc.status().await;
        assert_eq!(status.summary, "2 GitHub API call(s) since start");

        let summary = svc.handle("summary", Value::Null).await.unwrap();
        assert_eq!(summary["api_total"], 2);
        assert_eq!(summary["by_subcommand"]["pr view"], 1);
        assert_eq!(summary["since"], svc.started_at.to_rfc3339());
    }

    #[tokio::test]
    async fn a_scan_is_abandoned_once_shutdown_begins() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let svc = GithubCountersService::with_log_path(Some(log.clone()));
        for n in 1..=3 {
            append(&log, &gh_line(svc.started_at, n, &["pr", "list"]));
        }

        svc.stopping.cancel();
        let status = svc.status().await;
        assert_eq!(
            status.summary,
            "0 GitHub API call(s) since start (incomplete)"
        );
        let summary = svc.handle("summary", Value::Null).await.unwrap();
        assert_eq!(summary["incomplete"], true);

        // The abandoned scan lost nothing: the final summary, under its own
        // deadline, reads what was left and the tally is whole.
        assert_eq!(tallied(&svc), 0);
        svc.shutdown().await;
        assert_eq!(tallied(&svc), 3);
    }

    #[tokio::test]
    async fn a_read_that_fails_outright_reports_what_the_tally_last_held() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let svc = GithubCountersService::with_log_path(Some(log.clone()));
        append(&log, &gh_line(svc.started_at, 1, &["pr", "list"]));
        append(&log, &gh_line(svc.started_at, 2, &["pr", "view"]));
        assert_eq!(
            svc.status().await.summary,
            "2 GitHub API call(s) since start"
        );

        // A stop condition that panics takes its blocking task down, and poisons
        // the lock, mid-read. The tally is intact, so the answer is its last
        // contents, marked incomplete, not zeros that read as complete.
        let (counts, current) = counts_since_boot(&svc.tally, || panic!("induced")).await;
        assert!(!current);
        assert_eq!(counts.total(), 2);

        // And the service goes on working: the poisoned lock is recovered.
        append(&log, &gh_line(svc.started_at, 3, &["pr", "list"]));
        assert_eq!(
            svc.status().await.summary,
            "3 GitHub API call(s) since start"
        );
    }

    #[tokio::test]
    async fn the_logger_keeps_the_tally_current_and_stops_on_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.jsonl");
        let svc = GithubCountersService::with_log_path(Some(log.clone()));
        svc.start_counter_logger_with(Duration::from_millis(1), Duration::from_millis(5));
        // A second start is a no-op.
        svc.start_counter_logger_with(Duration::from_millis(1), Duration::from_millis(5));

        append(&log, &gh_line(svc.started_at, 1, &["pr", "list"]));
        let deadline = Instant::now() + Duration::from_secs(10);
        while tallied(&svc) == 0 {
            assert!(
                Instant::now() < deadline,
                "the logger never read the record"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        svc.shutdown().await;
        assert!(svc.logger.lock().unwrap().is_none());
        assert!(svc.stopping.is_cancelled());
    }
}
