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

use crate::cli::format::TableOrJson;
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
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
    /// Deprecated: use `-o`/`--output json` instead.
    #[arg(long, hide = true)]
    pub json: bool,
}

impl ListCommand {
    /// Executes the list command.
    pub async fn execute(mut self) -> Result<()> {
        if self.json {
            eprintln!("warning: --json is deprecated; use -o/--output json instead");
            self.output = TableOrJson::Json;
        }
        let socket = server::resolve_socket(self.socket)?;
        let result = call(&socket, "list", Value::Null).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            TableOrJson::Table => println!("{}", render_windows(&result)),
        }
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
/// open window (repo, the daemon-computed branch and its ahead/behind sync
/// state, the primary folder, and how long ago it was last seen). Returns a
/// placeholder line when nothing is open.
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
        "{:<22} {:<24} {:<9} {:<40} {:>5}",
        "REPO", "BRANCH", "SYNC", "FOLDER", "AGE"
    );
    for window in windows {
        let repo = sanitize(repo_name(window));
        let branch = sanitize(window.get("branch").and_then(Value::as_str).unwrap_or("-"));
        let sync = sync_summary(window);
        let folder_disp = folder_summary(window);
        let age = age_secs(window.get("last_seen").and_then(Value::as_str));
        out.push_str(&format!(
            "\n{repo:<22} {branch:<24} {sync:<9} {folder_disp:<40} {age:>4}s"
        ));
    }
    out
}

/// The repo name to show for a window: the daemon-computed `main_repo` (which
/// names the *parent* repository of a linked worktree, not its worktree-folder
/// basename) when present, else the companion-reported `repo`, else `-`.
fn repo_name(window: &Value) -> &str {
    window
        .get("main_repo")
        .and_then(Value::as_str)
        .or_else(|| window.get("repo").and_then(Value::as_str))
        .unwrap_or("-")
}

/// A compact `+ahead -behind` divergence indicator for a window, or `-` when
/// the branch tracks no upstream (or there is no branch at all). The counts are
/// daemon-computed integers, so no sanitizing is needed.
fn sync_summary(window: &Value) -> String {
    let ahead = window.get("ahead").and_then(Value::as_u64);
    let behind = window.get("behind").and_then(Value::as_u64);
    match (ahead, behind) {
        (Some(ahead), Some(behind)) => format!("+{ahead} -{behind}"),
        _ => "-".to_string(),
    }
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
        assert_eq!(cmd.output, TableOrJson::Table);
        assert!(!cmd.json);
        assert!(cmd.socket.is_none());

        let WorktreesSubcommands::List(cmd) =
            parse(&["list", "-o", "json", "--socket", "/tmp/d.sock"]);
        assert_eq!(cmd.output, TableOrJson::Json);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));
    }

    #[test]
    fn list_deprecated_json_flag_still_parses() {
        // `--json` is captured separately; `execute` folds it into `output`.
        let WorktreesSubcommands::List(cmd) = parse(&["list", "--json"]);
        assert!(cmd.json);
        assert_eq!(cmd.output, TableOrJson::Table);
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
            "branch": "issue-1011",
            "ahead": 2,
            "behind": 1,
            "folders": ["/home/me/omni-dev", "/home/me/docs"],
            "last_seen": "2000-01-01T00:00:00Z",
        }]});
        let table = render_windows(&result);
        assert!(table.contains("omni-dev"), "{table}");
        // The computed branch and its sync state both render.
        assert!(table.contains("issue-1011"), "{table}");
        assert!(table.contains("+2 -1"), "{table}");
        // Primary folder plus a (+1) for the second workspace folder.
        assert!(table.contains("/home/me/omni-dev (+1)"), "{table}");
        // A header line plus exactly one data row.
        assert_eq!(table.lines().count(), 2, "{table}");
    }

    #[test]
    fn render_windows_prefers_main_repo_over_companion_repo() {
        // A linked worktree: the companion reports the worktree-folder basename,
        // but the daemon-computed `main_repo` names the parent repo, and that is
        // what the REPO column shows.
        let result = json!({ "windows": [{
            "key": "w1",
            "repo": "issue-1250",
            "main_repo": "omni-dev",
            "branch": "issue-1250",
            "folders": ["/home/me/worktrees/issue-1250"],
            "last_seen": "2000-01-01T00:00:00Z",
        }]});
        let table = render_windows(&result);
        assert!(table.contains("omni-dev"), "{table}");
        // The misleading worktree-folder basename does not appear in REPO (it is
        // still visible in the FOLDER column path).
        let data_row = table.lines().nth(1).unwrap();
        assert!(data_row.starts_with("omni-dev"), "{data_row}");
    }

    #[test]
    fn repo_name_falls_back_to_companion_repo_then_dash() {
        assert_eq!(
            repo_name(&json!({ "main_repo": "omni-dev", "repo": "wt" })),
            "omni-dev"
        );
        assert_eq!(repo_name(&json!({ "repo": "wt" })), "wt");
        assert_eq!(repo_name(&json!({})), "-");
    }

    #[test]
    fn render_windows_strips_control_bytes() {
        // C0 (ESC, CR, BEL), DEL, and C1 (CSI) bytes in every string-valued
        // field must not reach the terminal (#1137).
        let result = json!({ "windows": [{
            "key": "w1",
            "repo": "evil\x1b[31mrepo",
            "branch": "br\ranch\x07\u{9b}2J",
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
        assert!(table.contains("branch2J"), "{table:?}");
        assert!(table.contains("/tmp/a]0;owned (+1)"), "{table:?}");
        // Embedded CR/LF cannot forge extra rows: header plus one data row.
        assert_eq!(table.lines().count(), 2, "{table:?}");
    }

    #[test]
    fn sync_summary_formats_or_dashes() {
        assert_eq!(sync_summary(&json!({ "ahead": 2, "behind": 1 })), "+2 -1");
        assert_eq!(sync_summary(&json!({ "ahead": 0, "behind": 0 })), "+0 -0");
        // Branch present but no upstream, or nothing at all → a dash.
        assert_eq!(sync_summary(&json!({ "branch": "main" })), "-");
        assert_eq!(sync_summary(&json!({})), "-");
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
