//! `omni-dev daemon status` — report daemon and per-service status.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use serde_json::json;

use crate::cli::format::TableOrJson;
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
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
    /// Deprecated: use `-o`/`--output json` instead.
    #[arg(long, hide = true)]
    pub json: bool,
}

impl StatusCommand {
    /// Executes the status command.
    pub async fn execute(mut self) -> Result<()> {
        if self.json {
            eprintln!("warning: --json is deprecated; use -o/--output json instead");
            self.output = TableOrJson::Json;
        }
        let json_out = self.output == TableOrJson::Json;
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
    match &report.version {
        Some(version) => println!("daemon: running (v{version})"),
        None => println!("daemon: running (version unknown)"),
    }
    if report.services.is_empty() {
        println!("  (no services registered)");
    }
    for svc in &report.services {
        let health = if svc.healthy { "ok" } else { "unhealthy" };
        println!("  {:<16} {:<10} {}", svc.name, health, svc.summary);
    }
    // Non-fatal: flag a stale resident daemon (older/newer than this CLI) so an
    // operator knows a `daemon restart` is needed to pick up the new binary.
    super::warn_version_mismatch(report.version.as_deref());
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::daemon::service::ServiceStatus;

    fn report(version: Option<&str>) -> StatusReport {
        StatusReport {
            services: vec![ServiceStatus {
                name: "browser-bridge".to_string(),
                healthy: true,
                summary: "no tab connected".to_string(),
                detail: serde_json::Value::Null,
            }],
            version: version.map(str::to_string),
        }
    }

    #[test]
    fn print_running_renders_table_and_json_with_or_without_a_version() {
        // Table: a known version, an unknown (pre-#1113) version, and JSON — each
        // exercises a distinct branch of the header line without erroring.
        print_running(false, &report(Some("1.2.3"))).unwrap();
        print_running(false, &report(None)).unwrap();
        print_running(true, &report(Some("1.2.3"))).unwrap();
        // A running daemon with no services still renders.
        print_running(
            false,
            &StatusReport {
                services: vec![],
                version: Some("1.2.3".to_string()),
            },
        )
        .unwrap();
    }

    #[test]
    fn print_not_running_renders_table_and_json() {
        print_not_running(false).unwrap();
        print_not_running(true).unwrap();
    }
}
