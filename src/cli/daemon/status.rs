//! `omni-dev daemon status` — report daemon and per-service status.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use serde_json::json;

use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::StatusReport;
use crate::daemon::server;

/// Reports whether the daemon is running and the status of each hosted service.
///
/// Under launchd socket activation, "running" means a daemon process is currently
/// spawned. The agent demand-spawns the daemon on the next client connect, so
/// "not running" means "no process is resident right now", not "unavailable" —
/// unless the agent was booted out via `daemon stop`.
#[derive(Parser)]
pub struct StatusCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl StatusCommand {
    /// Executes the status command.
    pub async fn execute(self) -> Result<()> {
        let json_out = self.json;
        let socket_path = server::resolve_socket(self.socket)?;
        match DaemonClient::new(&socket_path).status().await {
            Ok(report) => print_running(json_out, &report),
            Err(_) => print_not_running(json_out),
        }
    }
}

/// Prints the running daemon's aggregated service status.
fn print_running(json_out: bool, report: &StatusReport) -> Result<()> {
    if json_out {
        println!("{}", serde_json::to_string_pretty(report)?);
        return Ok(());
    }
    println!("daemon: running");
    if report.services.is_empty() {
        println!("  (no services registered)");
    }
    for svc in &report.services {
        let health = if svc.healthy { "ok" } else { "unhealthy" };
        println!("  {:<16} {:<10} {}", svc.name, health, svc.summary);
    }
    Ok(())
}

/// Prints the "not running" status (or its JSON equivalent).
fn print_not_running(json_out: bool) -> Result<()> {
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "running": false }))?
        );
    } else {
        println!("daemon: not running");
    }
    Ok(())
}
