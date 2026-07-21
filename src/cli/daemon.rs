//! `omni-dev daemon` — supervise the long-lived daemon and its services.

pub(crate) mod bridge;
pub(crate) mod control;
pub(crate) mod logs;
pub(crate) mod restart;
pub(crate) mod run;
pub(crate) mod service;
pub(crate) mod start;
pub(crate) mod status;
pub(crate) mod stop;
pub(crate) mod webhook;

use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::daemon::protocol::StatusReport;

/// Daemon: host long-lived services (e.g. the browser bridge) under one
/// supervised, menu-bar-controllable process.
#[derive(Parser)]
pub struct DaemonCommand {
    /// The daemon subcommand to execute.
    #[command(subcommand)]
    pub command: DaemonSubcommands,
}

/// Daemon subcommands.
#[derive(Subcommand)]
pub enum DaemonSubcommands {
    /// Runs the daemon in the foreground (the process launchd execs).
    Run(run::RunCommand),
    /// Starts the daemon in the background.
    Start(start::StartCommand),
    /// Stops the running daemon.
    Stop(stop::StopCommand),
    /// Restarts the daemon.
    Restart(restart::RestartCommand),
    /// Reports daemon and per-service status.
    Status(status::StatusCommand),
    /// Reads (and optionally follows) the daemon's log file.
    Logs(logs::LogsCommand),
    /// Controls the daemon-hosted browser bridge (restart, disconnect a tab, …).
    Bridge(bridge::BridgeCommand),
    /// Manages the non-polling PR-status webhook (register/list/remove/config).
    Webhook(webhook::WebhookCommand),
    /// Sends an arbitrary operation to any daemon service (low-level escape hatch).
    Service(service::ServiceCommand),
}

/// Prints a non-fatal warning when the resident daemon differs from this CLI
/// binary — the client is driving a stale daemon after a binary upgrade (#1113).
///
/// Comparison prefers the git commit SHA when both sides know theirs, so a
/// rebuilt-but-same-crate-version daemon the CLI has outrun is still flagged
/// (#1374); it falls back to crate-version string equality otherwise. A daemon
/// that advertises neither is treated as unknown and never warns.
pub(crate) fn warn_version_mismatch(report: &StatusReport) {
    if is_daemon_stale(
        report.version.as_deref(),
        report.provenance.commit_long.as_deref(),
        crate::VERSION,
        crate::build_info::GIT_SHA,
    ) {
        let cli = describe_build(crate::VERSION, crate::build_info::GIT_SHA_SHORT);
        let daemon = describe_build(
            report.version.as_deref().unwrap_or("unknown"),
            report.provenance.commit.as_deref(),
        );
        eprintln!(
            "warning: omni-dev CLI {cli} is talking to daemon {daemon}; \
             run `omni-dev daemon restart` to upgrade the resident daemon"
        );
    }
}

/// Whether a resident daemon is stale relative to this CLI. When both sides
/// report a commit SHA the comparison is commit-level (catching a same-version
/// rebuild); otherwise it falls back to crate-version string equality. A daemon
/// that advertises no version and no commit is never stale.
fn is_daemon_stale(
    daemon_version: Option<&str>,
    daemon_commit: Option<&str>,
    cli_version: &str,
    cli_commit: Option<&str>,
) -> bool {
    match (cli_commit, daemon_commit) {
        // Both know their commit: a mismatch means different code even at the
        // same crate version (#1374).
        (Some(cli), Some(daemon)) => cli != daemon,
        // Otherwise fall back to crate-version equality (a pre-#1374 daemon, or
        // a build with no git metadata on either side).
        _ => matches!(daemon_version, Some(v) if v != cli_version),
    }
}

/// Formats a `v<version> (<short-sha>)` build descriptor, omitting the commit
/// when unknown.
fn describe_build(version: &str, short_commit: Option<&str>) -> String {
    match short_commit {
        Some(commit) => format!("v{version} ({commit})"),
        None => format!("v{version}"),
    }
}

/// Sends one operation to a named daemon service over the control socket and
/// returns its payload, turning an `ok: false` reply into an error. Stamps the
/// caller's request-log `invocation_id` so any HTTP the daemon issues while
/// serving the op correlates back to this invocation (#1198). Shared by the
/// typed `daemon bridge` command and the generic `daemon service` passthrough.
pub(crate) async fn call_service(
    socket: &std::path::Path,
    service: &str,
    op: &str,
    payload: serde_json::Value,
) -> Result<serde_json::Value> {
    use crate::daemon::client::DaemonClient;
    use crate::daemon::protocol::DaemonEnvelope;

    let origin = crate::request_log::current_context().invocation_id;
    let reply = DaemonClient::new(socket)
        .request(DaemonEnvelope::service(service, op, payload).with_origin(origin))
        .await?;
    if reply.ok {
        Ok(reply.payload)
    } else {
        anyhow::bail!(
            "daemon returned an error: {}",
            reply.error.as_deref().unwrap_or("unknown error")
        )
    }
}

impl DaemonCommand {
    /// Executes the daemon command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            DaemonSubcommands::Run(cmd) => cmd.execute().await,
            DaemonSubcommands::Start(cmd) => cmd.execute().await,
            DaemonSubcommands::Stop(cmd) => cmd.execute().await,
            DaemonSubcommands::Restart(cmd) => cmd.execute().await,
            DaemonSubcommands::Status(cmd) => cmd.execute().await,
            DaemonSubcommands::Logs(cmd) => cmd.execute().await,
            DaemonSubcommands::Bridge(cmd) => cmd.execute().await,
            DaemonSubcommands::Webhook(cmd) => cmd.execute().await,
            DaemonSubcommands::Service(cmd) => cmd.execute().await,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Mirrors the `omni-dev daemon` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: DaemonSubcommands,
    }

    fn parse(args: &[&str]) -> DaemonSubcommands {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().cmd
    }

    #[test]
    fn parses_all_subcommands() {
        assert!(matches!(parse(&["run"]), DaemonSubcommands::Run(_)));
        assert!(matches!(parse(&["start"]), DaemonSubcommands::Start(_)));
        assert!(matches!(parse(&["stop"]), DaemonSubcommands::Stop(_)));
        assert!(matches!(parse(&["restart"]), DaemonSubcommands::Restart(_)));
        assert!(matches!(parse(&["status"]), DaemonSubcommands::Status(_)));
        assert!(matches!(parse(&["logs"]), DaemonSubcommands::Logs(_)));
        assert!(matches!(
            parse(&["bridge", "status"]),
            DaemonSubcommands::Bridge(_)
        ));
        assert!(matches!(
            parse(&["service", "browser-bridge", "status"]),
            DaemonSubcommands::Service(_)
        ));
    }

    #[test]
    fn logs_flags_and_defaults_parse() {
        let DaemonSubcommands::Logs(cmd) = parse(&["logs"]) else {
            panic!("expected logs");
        };
        assert_eq!(cmd.lines, 200);
        assert!(!cmd.follow);
        assert!(cmd.socket.is_none());

        let DaemonSubcommands::Logs(cmd) =
            parse(&["logs", "-f", "-n", "50", "--socket", "/tmp/d.sock"])
        else {
            panic!("expected logs");
        };
        assert!(cmd.follow);
        assert_eq!(cmd.lines, 50);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));
    }

    #[test]
    fn is_daemon_stale_falls_back_to_version_when_a_commit_is_unknown() {
        // No commits known on either side → crate-version string comparison.
        assert!(!is_daemon_stale(Some("1.2.3"), None, "1.2.3", None));
        assert!(is_daemon_stale(Some("1.2.2"), None, "1.2.3", None));
        assert!(is_daemon_stale(Some("1.3.0"), None, "1.2.3", None));
        // A daemon that advertises neither version nor commit is never stale.
        assert!(!is_daemon_stale(None, None, "1.2.3", None));
        // One side missing a commit still falls back to the version compare.
        assert!(is_daemon_stale(Some("1.2.2"), None, "1.2.3", Some("aaaa")));
        assert!(is_daemon_stale(Some("1.2.2"), Some("bbbb"), "1.2.3", None));
    }

    #[test]
    fn is_daemon_stale_compares_on_commit_when_both_are_known() {
        // Same crate version, different commit → stale (the #1374 blind-spot fix).
        assert!(is_daemon_stale(
            Some("1.2.3"),
            Some("bbbbbbbb"),
            "1.2.3",
            Some("aaaaaaaa")
        ));
        // Same commit → not stale, regardless of any version noise.
        assert!(!is_daemon_stale(
            Some("1.2.3"),
            Some("aaaaaaaa"),
            "1.2.3",
            Some("aaaaaaaa")
        ));
    }

    #[test]
    fn warn_version_mismatch_covers_every_branch_without_panicking() {
        // Exercises the print branch (a commit mismatch) and the no-op branches
        // (matching, and a daemon advertising nothing). It writes to stderr, so
        // there is nothing to assert beyond it not panicking.
        let mismatch = StatusReport {
            version: Some("0.0.0-mismatch".to_string()),
            provenance: crate::build_info::Provenance {
                commit: Some("deadbeef".to_string()),
                commit_long: Some("deadbeefdeadbeef".to_string()),
                ..crate::build_info::Provenance::default()
            },
            ..StatusReport::default()
        };
        warn_version_mismatch(&mismatch);
        warn_version_mismatch(&StatusReport::current(vec![]));
        warn_version_mismatch(&StatusReport::default());
    }

    #[tokio::test]
    async fn call_service_returns_the_payload_of_an_ok_reply() {
        let (_dir, sock, server) = crate::daemon::testutil::fake_daemon_reply(
            serde_json::json!({ "ok": true, "payload": { "restarted": true } }),
        );
        let payload = call_service(&sock, "browser-bridge", "restart", serde_json::Value::Null)
            .await
            .unwrap();
        assert_eq!(payload, serde_json::json!({ "restarted": true }));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn daemon_execute_routes_logs_without_a_daemon() {
        // The `Logs` dispatch arm reads the log file directly (no socket), so a
        // missing `daemon.log` beside an absent socket is a clean no-op.
        let dir = tempfile::tempdir().unwrap();
        let cmd = DaemonCommand {
            command: DaemonSubcommands::Logs(logs::LogsCommand {
                socket: Some(dir.path().join("daemon.sock")),
                lines: 10,
                follow: false,
            }),
        };
        cmd.execute().await.unwrap();
    }

    #[tokio::test]
    async fn daemon_execute_routes_bridge_and_service_over_the_socket() {
        // The `Bridge` and `Service` dispatch arms both reach a service over the
        // control socket; a fake daemon acknowledges each.
        let (_bdir, bsock, bserver) = crate::daemon::testutil::fake_daemon_reply(
            serde_json::json!({ "ok": true, "payload": { "restarted": true } }),
        );
        DaemonCommand {
            command: DaemonSubcommands::Bridge(bridge::BridgeCommand {
                command: bridge::BridgeSubcommands::Restart(bridge::SocketArg {
                    socket: Some(bsock),
                }),
            }),
        }
        .execute()
        .await
        .unwrap();
        bserver.await.unwrap();

        let (_sdir, ssock, sserver) = crate::daemon::testutil::fake_daemon_reply(
            serde_json::json!({ "ok": true, "payload": { "connected": false } }),
        );
        DaemonCommand {
            command: DaemonSubcommands::Service(service::ServiceCommand {
                service: "browser-bridge".to_string(),
                op: "status".to_string(),
                payload: None,
                socket: Some(ssock),
            }),
        }
        .execute()
        .await
        .unwrap();
        sserver.await.unwrap();
    }

    #[tokio::test]
    async fn call_service_maps_an_error_reply_to_an_err() {
        let (_dir, sock, server) = crate::daemon::testutil::fake_daemon_reply(
            serde_json::json!({ "ok": false, "error": "no connected tab with id 9" }),
        );
        let err = call_service(
            &sock,
            "browser-bridge",
            "disconnect-tab",
            serde_json::Value::Null,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("no connected tab with id 9"),
            "{err}"
        );
        server.await.unwrap();
    }

    #[test]
    fn socket_override_parses() {
        let DaemonSubcommands::Run(cmd) = parse(&["run", "--socket", "/tmp/x.sock"]) else {
            panic!("expected run");
        };
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/x.sock")));
    }

    #[test]
    fn status_json_flag_parses() {
        let DaemonSubcommands::Status(cmd) = parse(&["status", "--json"]) else {
            panic!("expected status");
        };
        assert!(cmd.json);
    }

    #[test]
    fn socket_defaults_to_none() {
        let DaemonSubcommands::Status(cmd) = parse(&["status"]) else {
            panic!("expected status");
        };
        assert!(cmd.socket.is_none());
        assert!(!cmd.json);
    }

    /// `daemon status` against a socket with no daemon dispatches through to the
    /// "not running" path (table and `--json`) without erroring or side effects.
    #[tokio::test]
    async fn status_dispatch_reports_not_running() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("absent.sock");
        for json in [false, true] {
            let cmd = DaemonCommand {
                command: DaemonSubcommands::Status(status::StatusCommand {
                    socket: Some(socket.clone()),
                    output: crate::cli::format::TableOrJson::Table,
                    json,
                }),
            };
            cmd.execute().await.unwrap();
        }
    }
}
