//! `omni-dev daemon start` — launch the daemon in the background.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use super::control;
use crate::daemon::client::DaemonClient;
use crate::daemon::server;

/// Starts the daemon in the background.
///
/// On macOS this installs and loads a per-user launchd LaunchAgent (so the
/// daemon also starts at login); elsewhere it spawns a detached `daemon run`.
/// Returns once the control socket accepts connections.
#[derive(Parser)]
pub struct StartCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl StartCommand {
    /// Executes the start command.
    pub async fn execute(self) -> Result<()> {
        let socket_path = server::resolve_socket(self.socket)?;
        if DaemonClient::new(&socket_path).ping().await.is_ok() {
            println!("daemon already running (socket {})", socket_path.display());
            return Ok(());
        }
        control::launch(&socket_path)?;
        control::wait_until_ready(&socket_path).await?;
        println!("daemon started (socket {})", socket_path.display());
        Ok(())
    }
}
