//! `omni-dev daemon stop` — stop the running daemon.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::daemon::client::DaemonClient;
use crate::daemon::server;

/// Stops the running daemon, gracefully draining its services.
///
/// On macOS it also boots out the launchd agent — removing the demand socket — so
/// the daemon stays stopped rather than being re-activated on the next client
/// connect or at the next login. Re-run `daemon start` to re-enable it.
#[derive(Parser)]
pub struct StopCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl StopCommand {
    /// Executes the stop command.
    pub async fn execute(self) -> Result<()> {
        let socket_path = server::resolve_socket(self.socket)?;
        let stopped = DaemonClient::new(&socket_path).shutdown().await.is_ok();
        #[cfg(target_os = "macos")]
        {
            // Disable launchd auto-start if this daemon was started via `daemon start`.
            let _ = crate::daemon::launchd::unload();
        }
        if stopped {
            println!("daemon stopping");
        } else {
            println!("daemon not running");
        }
        Ok(())
    }
}
