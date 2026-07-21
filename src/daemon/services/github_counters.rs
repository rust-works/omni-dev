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
//! Every emission is best-effort and bounded (a small local log read, no
//! network), so it never delays shutdown.

use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use anyhow::{bail, Result};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::daemon::service::{DaemonService, MenuItem, MenuSnapshot, ServiceStatus};
use crate::github_metrics::{self, GhCounts};
use crate::request_log;

/// Delay before the first ("startup") summary, so the pollers have come up.
const STARTUP_DELAY: Duration = Duration::from_secs(5);
/// Interval between periodic summaries after the first one.
const PERIODIC_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// The periodic-summary task and the token that stops it.
struct LoggerTask {
    /// Cancelled by [`shutdown`](DaemonService::shutdown) to end the loop.
    token: CancellationToken,
    /// The spawned loop, awaited on shutdown so it fully unwinds.
    handle: JoinHandle<()>,
}

/// Periodically logs, and reports on demand, the GitHub API-call counters.
pub struct GithubCountersService {
    /// When the daemon started, so every summary reports calls **since boot** —
    /// a clean before/after marker that resets across restarts.
    started_at: DateTime<Utc>,
    /// The periodic-summary task (idempotent start; `None` until started).
    logger: Mutex<Option<LoggerTask>>,
}

impl Default for GithubCountersService {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubCountersService {
    /// Cheap construction (like the worktrees/sessions services): captures the
    /// start time and persists nothing. Call [`Self::start_counter_logger`] to
    /// spawn the periodic task once inside the tokio runtime.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started_at: Utc::now(),
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
        let token = CancellationToken::new();
        let loop_token = token.clone();
        let started_at = self.started_at;
        let handle = tokio::spawn(async move {
            // Wait for the baseline delay, but exit immediately if the daemon is
            // already shutting down (shutdown() emits the final summary itself).
            tokio::select! {
                () = loop_token.cancelled() => return,
                () = tokio::time::sleep(startup_delay) => {}
            }
            emit(started_at, "startup").await;
            loop {
                tokio::select! {
                    () = loop_token.cancelled() => break,
                    () = tokio::time::sleep(interval) => emit(started_at, "periodic").await,
                }
            }
        });
        *guard = Some(LoggerTask { token, handle });
    }
}

/// Reads and tallies the `gh` records logged since `started_at` (all sources).
/// Best-effort: a missing/unreadable log yields zero counts. Offloaded to a
/// blocking thread by callers, since it reads a file.
fn counts_since(started_at: DateTime<Utc>) -> GhCounts {
    match request_log::log_file_path() {
        Some(path) => github_metrics::aggregate(&path, Some(started_at), None, None),
        None => GhCounts::default(),
    }
}

/// Emits one `tracing::info` summary line, aggregating on a blocking thread so
/// the log read never stalls the async executor.
async fn emit(started_at: DateTime<Utc>, phase: &str) {
    let counts = tokio::task::spawn_blocking(move || counts_since(started_at))
        .await
        .unwrap_or_default();
    // Bind before the macro so the summary is computed whenever this runs, not
    // only when an info-level subscriber is installed (the poller idiom).
    let summary = counts.summary_line();
    tracing::info!("github api calls ({phase}): {summary}");
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
                let started_at = self.started_at;
                let counts = tokio::task::spawn_blocking(move || counts_since(started_at)).await?;
                let mut value = counts.to_json();
                if let Value::Object(map) = &mut value {
                    map.insert("since".to_string(), json!(started_at.to_rfc3339()));
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
        let started_at = self.started_at;
        let counts = tokio::task::spawn_blocking(move || counts_since(started_at))
            .await
            .unwrap_or_default();
        ServiceStatus {
            name: self.name().to_string(),
            healthy: true,
            summary: format!("{} GitHub API call(s) since start", counts.api_total()),
            detail: counts.to_json(),
        }
    }

    async fn shutdown(&self) {
        // Stop the periodic task (take it from under the lock before awaiting, so
        // the std::Mutex is never held across the `.await`), then emit the final
        // summary. This is the deterministic, awaited flush for every termination
        // path (SIGTERM/SIGINT/SIGHUP + the `shutdown` op).
        let task = self
            .logger
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(task) = task {
            task.token.cancel();
            let _ = task.handle.await;
        }
        emit(self.started_at, "shutdown").await;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn service_reports_status_and_handles_summary() {
        let svc = GithubCountersService::new();
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
        assert!(svc.handle("bogus", Value::Null).await.is_err());

        // menu()/menu_action()/shutdown() are inert but must not panic; shutdown()
        // emits the final summary even though no logger task was started.
        assert_eq!(svc.menu().title, "GitHub API");
        svc.menu_action("x").await.unwrap();
        svc.shutdown().await;
    }
}
