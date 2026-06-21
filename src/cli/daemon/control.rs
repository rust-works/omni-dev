//! Shared launch/poll helpers for the daemon launcher subcommands
//! (`start` / `restart`).

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::Result;
use tokio::time::timeout;

use crate::daemon::client::DaemonClient;

/// Per-attempt cap on a single `ping`, so one hung probe can't blow the overall
/// budget — without it a `connect` that succeeds but is never accepted (e.g.
/// against a daemon mid-drain) would block forever.
const PING_TIMEOUT: Duration = Duration::from_millis(250);

/// Pause between poll attempts.
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Total wall-clock budget for a readiness/teardown poll.
const POLL_BUDGET: Duration = Duration::from_secs(5);

/// Launches a background daemon bound to `socket`.
///
/// macOS installs and loads a launchd LaunchAgent (auto-start at login);
/// elsewhere a detached `omni-dev daemon run` is spawned.
#[cfg(target_os = "macos")]
pub(super) fn launch(socket: &Path) -> Result<()> {
    crate::daemon::launchd::install_and_load(socket)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn launch(socket: &Path) -> Result<()> {
    use anyhow::Context;
    let exe = std::env::current_exe().context("could not resolve the current executable")?;
    std::process::Command::new(exe)
        .arg("daemon")
        .arg("run")
        .arg("--socket")
        .arg(socket)
        .spawn()
        .context("failed to spawn the daemon process")?;
    Ok(())
}

/// One bounded `ping`: `true` only on a healthy pong within [`PING_TIMEOUT`];
/// an error reply or a timeout both count as "not alive".
async fn pings_alive(client: &DaemonClient) -> bool {
    matches!(timeout(PING_TIMEOUT, client.ping()).await, Ok(Ok(())))
}

/// Polls `ping` until it reaches `want_alive`, or `budget` elapses. Each attempt
/// is individually bounded by [`PING_TIMEOUT`] so the deadline is real and a
/// single hung probe cannot stall the loop. Returns whether the target state was
/// reached in time.
async fn poll_until(socket: &Path, want_alive: bool, budget: Duration) -> bool {
    let client = DaemonClient::new(socket);
    let deadline = Instant::now() + budget;
    loop {
        if pings_alive(&client).await == want_alive {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Polls until the daemon answers `ping`, or the ~5s budget elapses.
pub(super) async fn wait_until_ready(socket: &Path) -> Result<()> {
    if poll_until(socket, true, POLL_BUDGET).await {
        Ok(())
    } else {
        anyhow::bail!(
            "daemon did not become ready within 5s (socket {})",
            socket.display()
        )
    }
}

/// Polls until the daemon stops answering `ping`, or the ~5s budget elapses.
pub(super) async fn wait_until_down(socket: &Path) -> Result<()> {
    if poll_until(socket, false, POLL_BUDGET).await {
        Ok(())
    } else {
        anyhow::bail!(
            "daemon did not stop within 5s (socket {})",
            socket.display()
        )
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn wait_until_down_returns_when_no_daemon() {
        // Nothing is listening on this socket, so `ping` fails on the first try
        // and the helper returns quickly. (No daemon is launched.)
        let dir = tempfile::tempdir().unwrap();
        wait_until_down(&dir.path().join("absent.sock"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn poll_until_alive_honours_its_budget() {
        // No daemon will ever come up, so waiting for `alive` must give up at the
        // deadline rather than hang or burn the full 5s default. A small budget
        // keeps the test fast; we also assert it returns close to that budget.
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("never.sock");
        let budget = Duration::from_millis(150);

        let start = Instant::now();
        let reached = poll_until(&socket, true, budget).await;
        let elapsed = start.elapsed();

        assert!(!reached, "no daemon should ever be reported alive");
        assert!(
            elapsed >= budget && elapsed < Duration::from_secs(2),
            "poll should end near the {budget:?} budget, took {elapsed:?}"
        );
    }
}
