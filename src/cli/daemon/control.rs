//! Shared launch/poll helpers for the daemon launcher subcommands
//! (`start` / `restart`).

use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::daemon::client::DaemonClient;

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

/// Polls until the daemon answers `ping`, or times out (~5s).
pub(super) async fn wait_until_ready(socket: &Path) -> Result<()> {
    let client = DaemonClient::new(socket);
    for _ in 0..50 {
        if client.ping().await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "daemon did not become ready within 5s (socket {})",
        socket.display()
    )
}

/// Polls until the daemon stops answering `ping`, or times out (~5s).
pub(super) async fn wait_until_down(socket: &Path) -> Result<()> {
    let client = DaemonClient::new(socket);
    for _ in 0..50 {
        if client.ping().await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!(
        "daemon did not stop within 5s (socket {})",
        socket.display()
    )
}
