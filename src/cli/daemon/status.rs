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
    println!("daemon: running ({})", describe_status(report));
    // A top-level GitHub API budget line (#1375), when the monitor has a reading —
    // the daemon's `gh` usage spends this budget machine-wide, so it belongs above
    // the per-service rows, not nested under one. `⚠` flags a resource ≥ ~80% used.
    if let Some(rate_limit) = &report.github_rate_limit {
        let line = rate_limit.summary_line();
        if !line.is_empty() {
            println!("  github rate limit: {line}");
        }
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
    super::warn_version_mismatch(report);
    Ok(())
}

/// Formats the `daemon status` header detail: the daemon's crate version plus
/// whatever git provenance it advertised — commit, build time, and a `dirty`
/// marker (#1374). Each provenance piece is appended only when present, so a
/// daemon built without git metadata renders exactly `v<version>`.
fn describe_status(report: &StatusReport) -> String {
    let mut parts = match &report.version {
        Some(version) => vec![format!("v{version}")],
        None => vec!["version unknown".to_string()],
    };
    if let Some(commit) = &report.provenance.commit {
        parts.push(commit.clone());
    }
    if let Some(built) = &report.provenance.build_timestamp {
        parts.push(format!("built {built}"));
    }
    if report.provenance.dirty == Some(true) {
        parts.push("dirty".to_string());
    }
    parts.join(", ")
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
            ..StatusReport::default()
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
                ..StatusReport::default()
            },
        )
        .unwrap();
    }

    #[test]
    fn print_running_renders_the_github_rate_limit_line() {
        use crate::github_rate_limit::{RateLimitResource, RateLimitSnapshot};
        let res = |used: u64| RateLimitResource {
            used,
            limit: 5000,
            remaining: 5000 - used,
            percent: used as f64 / 5000.0 * 100.0,
            reset: 1_700_000_000,
        };
        // Below threshold, at/over threshold, and JSON — each renders without error
        // (the field serializes into the JSON payload automatically).
        let mut rep = report(Some("1.2.3"));
        rep.github_rate_limit = Some(RateLimitSnapshot {
            graphql: Some(res(100)),
            core: Some(res(27)),
            search: None,
        });
        print_running(false, &rep).unwrap();
        print_running(true, &rep).unwrap();

        rep.github_rate_limit = Some(RateLimitSnapshot {
            graphql: Some(res(4500)), // 90% ⇒ warn branch
            core: Some(res(27)),
            search: None,
        });
        print_running(false, &rep).unwrap();
    }

    #[test]
    fn describe_status_appends_provenance_pieces_when_present() {
        use crate::build_info::Provenance;

        // Bare version only when no provenance is advertised.
        assert_eq!(describe_status(&report(Some("0.36.0"))), "v0.36.0");
        // Unknown version renders its placeholder.
        assert_eq!(describe_status(&report(None)), "version unknown");

        // Commit, build time, and the dirty marker are appended in order; a
        // clean tree omits the `dirty` marker.
        let dirty = StatusReport {
            version: Some("0.36.0".to_string()),
            provenance: Provenance {
                commit: Some("a6d304fd".to_string()),
                build_timestamp: Some("2026-07-20T05:33:17+00:00".to_string()),
                dirty: Some(true),
                ..Provenance::default()
            },
            ..StatusReport::default()
        };
        assert_eq!(
            describe_status(&dirty),
            "v0.36.0, a6d304fd, built 2026-07-20T05:33:17+00:00, dirty"
        );

        let clean = StatusReport {
            version: Some("0.36.0".to_string()),
            provenance: Provenance {
                commit: Some("a6d304fd".to_string()),
                dirty: Some(false),
                ..Provenance::default()
            },
            ..StatusReport::default()
        };
        assert_eq!(describe_status(&clean), "v0.36.0, a6d304fd");
    }

    #[test]
    fn print_not_running_renders_table_and_json() {
        print_not_running(false).unwrap();
        print_not_running(true).unwrap();
    }
}
