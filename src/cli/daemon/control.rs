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
/// macOS installs and loads a socket-activated launchd LaunchAgent; Linux installs
/// a socket-activated systemd **user** unit when a user manager is available
/// (falling back to a detached spawn otherwise); any other Unix spawns a detached
/// `omni-dev daemon run` directly. Both service managers auto-start the daemon at
/// login; the detached-spawn fallback (`spawn_detached`) does not (#1174).
#[cfg(target_os = "macos")]
pub(super) fn launch(socket: &Path) -> Result<()> {
    crate::daemon::launchd::install_and_load(socket)
}

/// Linux `launch`: install a systemd user unit for auto-start at login when a user
/// manager is available, else fall back to the detached spawn. See the macOS
/// `launch` for the cross-platform contract.
#[cfg(target_os = "linux")]
pub(super) fn launch(socket: &Path) -> Result<()> {
    use crate::daemon::systemd;

    if systemd::is_available() {
        match systemd::install_and_load(socket) {
            Ok(()) => return Ok(()),
            Err(e) => tracing::warn!(
                "systemd auto-start unavailable ({e}); falling back to a detached spawn"
            ),
        }
    }
    spawn_detached(socket)
}

/// Non-macOS, non-Linux Unix `launch`: there is no login auto-start integration,
/// so always use the detached spawn.
#[cfg(all(not(target_os = "macos"), not(target_os = "linux")))]
pub(super) fn launch(socket: &Path) -> Result<()> {
    spawn_detached(socket)
}

/// Spawns a detached `omni-dev daemon run`: `setsid` puts it in its own session
/// (so it survives the launching terminal and its SIGHUP), stdin comes from
/// `/dev/null`, and stdout/stderr are appended to a `0600` `daemon.log` beside the
/// socket. The off-macOS fallback when no service-manager auto-start is installed;
/// it does not restart the daemon at login (#1174).
#[cfg(not(target_os = "macos"))]
fn spawn_detached(socket: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    use anyhow::Context;

    use crate::daemon::paths;

    let exe = std::env::current_exe().context("could not resolve the current executable")?;
    if let Some(dir) = socket.parent() {
        paths::ensure_dir_0700(dir)?;
    }
    let log_path = paths::log_path_for_socket(socket);
    let log = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&log_path)
        .with_context(|| format!("failed to open daemon log {}", log_path.display()))?;
    paths::ensure_handle_0600(&log)
        .with_context(|| format!("failed to set 0600 on {}", log_path.display()))?;
    let log_err = log
        .try_clone()
        .with_context(|| format!("failed to clone handle for {}", log_path.display()))?;

    let mut command = std::process::Command::new(exe);
    command
        .arg("daemon")
        .arg("run")
        .arg("--socket")
        .arg(socket)
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(log_err);
    // `pre_exec` runs between fork and exec in the child, where only
    // async-signal-safe calls are allowed — which is exactly why it is
    // `unsafe` and there is no safe route to `setsid` on `Command`.
    // SAFETY: `setsid(2)` is async-signal-safe, allocates nothing, and cannot
    // fail here with EPERM because a freshly forked child is never a
    // process-group leader.
    #[allow(unsafe_code)]
    unsafe {
        command.pre_exec(|| {
            if nix::libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
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
///
/// Not compiled on Linux: `restart` there may face a systemd socket-activated
/// daemon, where pinging the still-armed socket would re-activate it. See
/// [`restart`](super::restart) (#1174).
#[cfg(not(target_os = "linux"))]
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

    #[cfg(not(target_os = "linux"))]
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
