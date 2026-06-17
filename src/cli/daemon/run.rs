//! `omni-dev daemon run` — become the daemon (foreground).

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use crate::daemon;
use crate::daemon::server::{self, DaemonOptions};

/// Runs the daemon in the foreground.
///
/// Binds the control socket (which doubles as the single-instance lock), starts
/// every registered service, and blocks until `SIGTERM`/`SIGINT` or a
/// `daemon stop`. This is the process a launchd LaunchAgent (or `daemon start`)
/// execs.
#[derive(Parser)]
pub struct RunCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl RunCommand {
    /// Executes the run command.
    pub async fn execute(self) -> Result<()> {
        let socket_path = server::resolve_socket(self.socket)?;
        let registry = daemon::build_default_registry().await?;
        server::run(registry, DaemonOptions { socket_path }).await
    }
}
