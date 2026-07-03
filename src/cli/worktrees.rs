//! `omni-dev worktrees` — a thin client for the daemon's cross-window worktree
//! registry.
//!
//! Lifecycle stays on `omni-dev daemon` (`start`/`stop`/`status`/`restart`);
//! this command only sends the `worktrees` service's read op (`list`) over the
//! daemon's Unix control socket. The companion VS Code extension is what *feeds*
//! the registry (`register`/`heartbeat`/`unregister`), talking to the same
//! socket directly from each window.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::Value;

use crate::daemon::client::DaemonClient;
use crate::daemon::protocol::{DaemonEnvelope, DaemonReply};
use crate::daemon::server;

/// The `worktrees` service routing key on the daemon control socket.
const SERVICE: &str = "worktrees";

/// Worktrees: see the repos/worktrees open across every VS Code window, kept
/// live by the daemon.
#[derive(Parser)]
pub struct WorktreesCommand {
    /// The worktrees subcommand to execute.
    #[command(subcommand)]
    pub command: WorktreesSubcommands,
}

/// Worktrees subcommands.
#[derive(Subcommand)]
pub enum WorktreesSubcommands {
    /// List the repos/worktrees currently open across all windows.
    List(ListCommand),
}

impl WorktreesCommand {
    /// Executes the worktrees command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WorktreesSubcommands::List(cmd) => cmd.execute().await,
        }
    }
}

/// Lists the live cross-window set of open worktrees/repos.
#[derive(Parser)]
pub struct ListCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Emit machine-readable JSON instead of a table.
    #[arg(long)]
    pub json: bool,
}

impl ListCommand {
    /// Executes the list command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "list", Value::Null).await?;
        if self.json {
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(());
        }
        println!("{}", render_windows(&result));
        Ok(())
    }
}

/// Sends one `worktrees` service op over the control socket, returning its
/// payload or turning an `ok: false` reply into an error.
async fn call(socket: &Path, op: &str, payload: Value) -> Result<Value> {
    let reply = DaemonClient::new(socket)
        .request(DaemonEnvelope::service(SERVICE, op, payload))
        .await?;
    reply_payload(reply)
}

/// Unwraps a daemon reply into its payload, turning an `ok: false` reply into an
/// error. Pure (no socket), so both mappings are unit-testable.
fn reply_payload(reply: DaemonReply) -> Result<Value> {
    if reply.ok {
        Ok(reply.payload)
    } else {
        bail!(
            "daemon returned an error: {}",
            reply.error.as_deref().unwrap_or("unknown error")
        )
    }
}

/// Renders a `list` reply as a human-readable table: a header and one row per
/// open window (repo, title, primary folder, and how long ago it was last
/// seen). Returns a placeholder line when nothing is open.
fn render_windows(result: &Value) -> String {
    let windows = result
        .get("windows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if windows.is_empty() {
        return "No open windows.".to_string();
    }
    let mut out = format!(
        "{:<24} {:<32} {:<40} {:>5}",
        "REPO", "TITLE", "FOLDER", "AGE"
    );
    for window in windows {
        let repo = sanitize(window.get("repo").and_then(Value::as_str).unwrap_or("-"));
        let title = sanitize(window.get("title").and_then(Value::as_str).unwrap_or(""));
        let folder_disp = folder_summary(window);
        let age = age_secs(window.get("last_seen").and_then(Value::as_str));
        out.push_str(&format!(
            "\n{repo:<24} {title:<32} {folder_disp:<40} {age:>4}s"
        ));
    }
    out
}

/// The primary folder of a window, with a `(+N)` suffix when it has more than
/// one workspace folder.
fn folder_summary(window: &Value) -> String {
    let folders = window
        .get("folders")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let first = sanitize(folders.first().and_then(Value::as_str).unwrap_or(""));
    let extra = folders.len().saturating_sub(1);
    if extra > 0 {
        format!("{first} (+{extra})")
    } else {
        first
    }
}

/// Strips control characters (C0, DEL, C1) from an untrusted registry string so
/// a malicious `register` payload cannot inject terminal escape sequences into
/// the rendered table (#1137). The `--json` path stays verbatim.
fn sanitize(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// Seconds elapsed since an RFC 3339 timestamp (0 if absent/unparseable).
fn age_secs(ts: Option<&str>) -> i64 {
    ts.and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or(0, |t| {
            (Utc::now() - t.with_timezone(&Utc)).num_seconds().max(0)
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Mirrors the `omni-dev worktrees` argv surface for parse tests.
    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: WorktreesSubcommands,
    }

    fn parse(args: &[&str]) -> WorktreesSubcommands {
        let mut full = vec!["omni-dev"];
        full.extend_from_slice(args);
        Wrapper::try_parse_from(full).unwrap().cmd
    }

    #[test]
    fn list_parses_flags_and_defaults() {
        let WorktreesSubcommands::List(cmd) = parse(&["list"]);
        assert!(!cmd.json);
        assert!(cmd.socket.is_none());

        let WorktreesSubcommands::List(cmd) = parse(&["list", "--json", "--socket", "/tmp/d.sock"]);
        assert!(cmd.json);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));
    }

    #[test]
    fn render_windows_handles_empty_replies() {
        assert_eq!(
            render_windows(&json!({ "windows": [] })),
            "No open windows."
        );
        assert_eq!(render_windows(&json!({})), "No open windows.");
    }

    #[test]
    fn render_windows_renders_rows() {
        let result = json!({ "windows": [{
            "key": "w1",
            "repo": "omni-dev",
            "title": "issue-1011 — worktrees",
            "folders": ["/home/me/omni-dev", "/home/me/docs"],
            "last_seen": "2000-01-01T00:00:00Z",
        }]});
        let table = render_windows(&result);
        assert!(table.contains("omni-dev"), "{table}");
        assert!(table.contains("issue-1011"), "{table}");
        // Primary folder plus a (+1) for the second workspace folder.
        assert!(table.contains("/home/me/omni-dev (+1)"), "{table}");
        // A header line plus exactly one data row.
        assert_eq!(table.lines().count(), 2, "{table}");
    }

    #[test]
    fn render_windows_strips_control_bytes() {
        // C0 (ESC, CR, BEL), DEL, and C1 (CSI) bytes in every untrusted field
        // must not reach the terminal (#1137).
        let result = json!({ "windows": [{
            "key": "w1",
            "repo": "evil\x1b[31mrepo",
            "title": "ti\rtle\x07\u{9b}2J",
            "folders": ["/tmp/a\x1b]0;owned\x07\u{7f}", "/tmp/b"],
            "last_seen": "2000-01-01T00:00:00Z",
        }]});
        let table = render_windows(&result);
        assert!(
            !table.contains(|c: char| c.is_control() && c != '\n'),
            "{table:?}"
        );
        // Visible text survives with only the control bytes removed.
        assert!(table.contains("evil[31mrepo"), "{table:?}");
        assert!(table.contains("title2J"), "{table:?}");
        assert!(table.contains("/tmp/a]0;owned (+1)"), "{table:?}");
        // Embedded CR/LF cannot forge extra rows: header plus one data row.
        assert_eq!(table.lines().count(), 2, "{table:?}");
    }

    #[test]
    fn folder_summary_strips_control_bytes() {
        assert_eq!(
            folder_summary(&json!({ "folders": ["/a\x1b[2J/b"] })),
            "/a[2J/b"
        );
    }

    #[test]
    fn folder_summary_counts_extra_folders() {
        assert_eq!(folder_summary(&json!({ "folders": [] })), "");
        assert_eq!(folder_summary(&json!({ "folders": ["/a"] })), "/a");
        assert_eq!(
            folder_summary(&json!({ "folders": ["/a", "/b", "/c"] })),
            "/a (+2)"
        );
    }

    #[test]
    fn age_secs_handles_absent_and_unparseable_and_past() {
        assert_eq!(age_secs(None), 0);
        assert_eq!(age_secs(Some("not-a-timestamp")), 0);
        assert!(age_secs(Some("2000-01-01T00:00:00Z")) > 0);
    }

    #[test]
    fn reply_payload_unwraps_ok_and_maps_errors() {
        // ok → payload.
        assert_eq!(
            reply_payload(DaemonReply::ok(json!({ "a": 1 }))).unwrap(),
            json!({ "a": 1 })
        );
        // ok: false with a message → that message.
        let err = reply_payload(DaemonReply::err("boom")).unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        // ok: false with no message → the "unknown error" fallback.
        let err = reply_payload(DaemonReply {
            ok: false,
            payload: Value::Null,
            error: None,
        })
        .unwrap_err();
        assert!(err.to_string().contains("unknown error"), "{err}");
    }
}
