//! `omni-dev daemon restart` — stop then start the daemon.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use super::control;
use crate::daemon::client::DaemonClient;
use crate::daemon::server;

/// Restarts the daemon: stop it (if running), then start it again.
#[derive(Parser)]
pub struct RestartCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl RestartCommand {
    /// Executes the restart command.
    pub async fn execute(self) -> Result<()> {
        let socket_path = server::resolve_socket(self.socket)?;
        let client = DaemonClient::new(&socket_path);
        if client.ping().await.is_ok() {
            client.shutdown().await.ok();
            // On the socket-activated Linux path, pinging the still-armed systemd
            // socket would re-activate the daemon; the relaunch + readiness ping
            // below drive a clean handoff instead (systemd serializes at most one
            // service instance, so the old drains and a fresh one comes up). Safe
            // on the detached-spawn fallback too, where `bind_or_reclaim` handles
            // the brief socket contention. (#1174)
            #[cfg(not(target_os = "linux"))]
            control::wait_until_down(&socket_path).await?;
        }
        // On macOS `launch` re-bootstraps via `install_and_load`, which already
        // boots out any prior agent before bootstrapping. Do *not* boot out
        // separately first: that would unregister auto-start in a window where a
        // failed/aborted re-bootstrap leaves the daemon both stopped and
        // unregistered — strictly worse than before `restart` ran. See #994.
        control::launch(&socket_path)?;
        control::wait_until_ready(&socket_path).await?;
        println!("daemon restarted (socket {})", socket_path.display());
        Ok(())
    }
}
