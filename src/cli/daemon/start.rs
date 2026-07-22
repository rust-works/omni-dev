//! `omni-dev daemon start` — launch the daemon in the background.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use super::control;
use crate::daemon::client::DaemonClient;
use crate::daemon::{server, DaemonServiceKind, ServiceSelection};

/// Starts the daemon in the background.
///
/// On macOS this installs and loads a per-user launchd LaunchAgent, and on Linux a
/// per-user systemd socket unit (when a user manager is available); either **owns**
/// the control socket and demand-spawns the daemon on the first client connect (so
/// it also activates at login), and `start` warms it with one readiness ping to
/// trigger that first spawn. Without a service manager (other Unix, or the systemd
/// fallback) it spawns a detached `daemon run` — its own session, stdout/stderr
/// appended to a `daemon.log` beside the socket — with no auto-start at login.
/// Returns once the control socket accepts connections.
#[derive(Parser)]
pub struct StartCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,

    /// Host only this comma-separated subset of services (default: all), baked
    /// into the generated launchd plist / systemd unit so it survives the
    /// service-manager exec. Overrides `OMNI_DEV_DAEMON_SERVICES`. Values:
    /// browser-bridge, snowflake, worktrees, sessions.
    #[arg(long, value_name = "SVC", value_delimiter = ',')]
    pub services: Vec<DaemonServiceKind>,
}

impl StartCommand {
    /// Executes the start command.
    pub async fn execute(self) -> Result<()> {
        let socket_path = server::resolve_socket(self.socket)?;
        if DaemonClient::new(&socket_path).ping().await.is_ok() {
            println!("daemon already running (socket {})", socket_path.display());
            return Ok(());
        }
        let services = ServiceSelection::from_flag_or_env(&self.services);
        control::launch(&socket_path, &services)?;
        control::wait_until_ready(&socket_path).await?;
        println!("daemon started (socket {})", socket_path.display());
        Ok(())
    }
}
