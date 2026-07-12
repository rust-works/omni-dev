//! `omni-dev worktrees` — a thin client for the daemon's cross-window worktree
//! registry.
//!
//! Lifecycle stays on `omni-dev daemon` (`start`/`stop`/`status`/`restart`);
//! this command only sends the `worktrees` service's read ops (`list` for the
//! open windows, `tree` for the repos-and-all-their-worktrees view) over the
//! daemon's Unix control socket. The companion VS Code extension is what *feeds*
//! the registry (`register`/`heartbeat`/`unregister`), talking to the same
//! socket directly from each window.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use chrono::Utc;
use clap::{Parser, Subcommand};
use serde_json::{json, Value};

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
    /// Show every repository and all its worktrees, grouped by repository.
    Tree(TreeCommand),
}

impl WorktreesCommand {
    /// Executes the worktrees command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WorktreesSubcommands::List(cmd) => cmd.execute().await,
            WorktreesSubcommands::Tree(cmd) => cmd.execute().await,
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

/// Shows every repository and all of its worktrees (open or not), grouped by
/// repository — the daemon's `tree` op, which derives the repos from the open
/// windows and enumerates each repo's worktrees.
#[derive(Parser)]
pub struct TreeCommand {
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
}

impl TreeCommand {
    /// Executes the tree command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let mut result = call(&socket, "tree", Value::Null).await?;
        // Ahead/behind is no longer part of the (cheap) streamed `tree` snapshot
        // (#1306); fetch it on demand for the worktrees we are about to render and
        // fold it back in, so `worktrees tree` shows the same `+ahead -behind` sync
        // state as before. Best-effort: an older daemon without the `ahead-behind`
        // op just renders `-`.
        enrich_ahead_behind(&socket, &mut result).await;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&result)?),
            TableOrJson::Table => println!("{}", render_tree(&result)),
        }
        Ok(())
    }
}

/// Fetches ahead/behind on demand for every worktree in a `tree` reply and folds
/// the counts back into each worktree object, so `worktrees tree` renders the same
/// `+ahead -behind` sync state the cheap snapshot no longer carries (#1306). A
/// best-effort enrichment: if there are no worktrees, the daemon lacks the
/// `ahead-behind` op (older daemon), or the call fails, `result` is left as-is and
/// the tree still renders — just with `-` for sync.
async fn enrich_ahead_behind(socket: &Path, result: &mut Value) {
    let paths = worktree_paths(result);
    if paths.is_empty() {
        return;
    }
    let Ok(reply) = call(socket, "ahead-behind", json!({ "paths": paths })).await else {
        return;
    };
    if let Some(results) = reply.get("results").and_then(Value::as_object) {
        merge_ahead_behind(result, results);
    }
}

/// Every worktree path in a `tree` reply, in render order — the batch the
/// on-demand `ahead-behind` op is asked about.
fn worktree_paths(result: &Value) -> Vec<String> {
    let mut paths = Vec::new();
    for repo in result
        .get("repos")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        for worktree in repo
            .get("worktrees")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            if let Some(path) = worktree.get("path").and_then(Value::as_str) {
                paths.push(path.to_string());
            }
        }
    }
    paths
}

/// Folds `{ ahead, behind }` counts (keyed by worktree path) from an `ahead-behind`
/// reply back into a `tree` reply's worktree objects. A worktree whose path is
/// absent from `results` (no upstream) is left untouched. Pure, so the merge is
/// unit-testable without a socket.
fn merge_ahead_behind(result: &mut Value, results: &serde_json::Map<String, Value>) {
    for repo in result
        .get_mut("repos")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for worktree in repo
            .get_mut("worktrees")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            // Take the worktree object up front so the insert reuses this handle
            // rather than a second, always-succeeding `as_object_mut` (a non-object
            // element in the array is skipped here).
            let Some(obj) = worktree.as_object_mut() else {
                continue;
            };
            let Some(path) = obj.get("path").and_then(Value::as_str).map(str::to_string) else {
                continue;
            };
            let Some(counts) = results.get(&path) else {
                continue;
            };
            // Fold both counts in together, or neither — a malformed entry missing
            // a side is left as no-sync rather than half-applied.
            if let (Some(ahead), Some(behind)) =
                (counts.get("ahead").cloned(), counts.get("behind").cloned())
            {
                obj.insert("ahead".to_string(), ahead);
                obj.insert("behind".to_string(), behind);
            }
        }
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

/// Renders a `tree` reply as a repo-grouped view: a header line per repository
/// (its name, GitHub `owner/name` when present, and root path), then one indented
/// row per worktree — a `*` marks the main working tree, followed by the branch,
/// its `+ahead -behind` sync state, an `open` flag when a live window has it open,
/// and the worktree path. Returns a placeholder when no repository is open.
fn render_tree(result: &Value) -> String {
    let repos = result
        .get("repos")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if repos.is_empty() {
        return "No repositories open.".to_string();
    }
    let mut out = String::new();
    for (i, repo) in repos.iter().enumerate() {
        // A blank line separates repositories (but not before the first): the
        // previous worktree row has no trailing newline, so two are needed.
        if i > 0 {
            out.push_str("\n\n");
        }
        out.push_str(&repo_header(repo));
        for worktree in repo
            .get("worktrees")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            out.push('\n');
            out.push_str(&worktree_row(worktree));
        }
    }
    out
}

/// The header line for one repo in the tree view: `<name>  (github: owner/name)
/// <root>`, with the GitHub clause omitted for a non-GitHub repo.
fn repo_header(repo: &Value) -> String {
    let name = sanitize(repo.get("main_repo").and_then(Value::as_str).unwrap_or("-"));
    let root = sanitize(repo.get("root").and_then(Value::as_str).unwrap_or(""));
    match github_summary(repo) {
        Some(github) => format!("{name}  ({github})  {root}"),
        None => format!("{name}  {root}"),
    }
}

/// A `github: owner/name` summary for a repo, or `None` when it has no GitHub
/// identity (a non-GitHub or remote-less repo).
fn github_summary(repo: &Value) -> Option<String> {
    let owner = repo.pointer("/github/owner").and_then(Value::as_str)?;
    let name = repo.pointer("/github/name").and_then(Value::as_str)?;
    Some(format!("github: {}/{}", sanitize(owner), sanitize(name)))
}

/// One indented worktree row: a `*` for the main working tree, the branch, the
/// `+ahead -behind` sync state, an `open` flag when a window has it open, and the
/// worktree path.
fn worktree_row(worktree: &Value) -> String {
    let marker = if worktree.get("is_main").and_then(Value::as_bool) == Some(true) {
        '*'
    } else {
        ' '
    };
    let branch = sanitize(
        worktree
            .get("branch")
            .and_then(Value::as_str)
            .unwrap_or("-"),
    );
    let sync = sync_summary(worktree);
    let open = if worktree.get("open").and_then(Value::as_bool) == Some(true) {
        "open"
    } else {
        ""
    };
    let path = sanitize(worktree.get("path").and_then(Value::as_str).unwrap_or(""));
    format!("  {marker} {branch:<24} {sync:<9} {open:<5} {path}")
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
        // Routing: `worktrees list` maps to the List variant.
        assert!(matches!(parse(&["list"]), WorktreesSubcommands::List(_)));
        // Flags, via the leaf parser (clap treats argv[0] as the command name).
        let cmd = ListCommand::try_parse_from(["list"]).unwrap();
        assert_eq!(cmd.output, TableOrJson::Table);
        assert!(!cmd.json);
        assert!(cmd.socket.is_none());

        let cmd =
            ListCommand::try_parse_from(["list", "-o", "json", "--socket", "/tmp/d.sock"]).unwrap();
        assert_eq!(cmd.output, TableOrJson::Json);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));
    }

    #[test]
    fn list_deprecated_json_flag_still_parses() {
        // `--json` is captured separately; `execute` folds it into `output`.
        let cmd = ListCommand::try_parse_from(["list", "--json"]).unwrap();
        assert!(cmd.json);
        assert_eq!(cmd.output, TableOrJson::Table);
    }

    #[test]
    fn tree_parses_flags_and_defaults() {
        // Routing: `worktrees tree` maps to the Tree variant.
        assert!(matches!(parse(&["tree"]), WorktreesSubcommands::Tree(_)));
        let cmd = TreeCommand::try_parse_from(["tree"]).unwrap();
        assert_eq!(cmd.output, TableOrJson::Table);
        assert!(cmd.socket.is_none());

        let cmd =
            TreeCommand::try_parse_from(["tree", "-o", "json", "--socket", "/tmp/d.sock"]).unwrap();
        assert_eq!(cmd.output, TableOrJson::Json);
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
    fn render_tree_handles_empty_replies() {
        assert_eq!(
            render_tree(&json!({ "repos": [] })),
            "No repositories open."
        );
        assert_eq!(render_tree(&json!({})), "No repositories open.");
    }

    #[test]
    fn worktree_paths_collects_every_worktree_in_render_order() {
        let result = json!({ "repos": [
            // The middle worktree has no `path` and is skipped, not collected.
            { "worktrees": [ { "path": "/a" }, { "branch": "detached" }, { "path": "/b" } ] },
            { "worktrees": [ { "path": "/c" } ] },
        ]});
        assert_eq!(worktree_paths(&result), vec!["/a", "/b", "/c"]);
        // No repos / no worktrees → an empty batch (nothing to fetch).
        assert!(worktree_paths(&json!({})).is_empty());
        assert!(worktree_paths(&json!({ "repos": [{ "worktrees": [] }] })).is_empty());
    }

    #[test]
    fn merge_ahead_behind_folds_counts_by_path_and_leaves_others() {
        // The on-demand `ahead-behind` op reports one worktree diverging and omits
        // the other (no upstream). The merge folds the counts onto the matching
        // path and leaves the untracked worktree without sync fields.
        let mut result = json!({ "repos": [{ "worktrees": [
            { "path": "/a", "branch": "main" },
            { "path": "/b", "branch": "feature" },
        ]}]});
        let results = json!({ "/a": { "ahead": 2, "behind": 1 } });
        merge_ahead_behind(&mut result, results.as_object().unwrap());

        let worktrees = result.pointer("/repos/0/worktrees").unwrap();
        let a = &worktrees[0];
        assert_eq!(a.get("ahead").and_then(Value::as_u64), Some(2));
        assert_eq!(a.get("behind").and_then(Value::as_u64), Some(1));
        // And it renders exactly as an eager snapshot would have.
        assert_eq!(sync_summary(a), "+2 -1");
        let b = &worktrees[1];
        assert!(b.get("ahead").is_none(), "{b:?}");
        assert!(b.get("behind").is_none(), "{b:?}");
        assert_eq!(sync_summary(b), "-");
    }

    #[test]
    fn merge_ahead_behind_skips_malformed_worktrees_and_counts() {
        // Every defensive guard, on malformed input that never comes from a real
        // daemon: a non-object array element, a worktree with no `path`, and a
        // results entry missing a side. None panics; none is half-applied.
        let mut result = json!({ "repos": [{ "worktrees": [
            "not-an-object",                       // non-object element → skipped
            { "branch": "detached" },              // object, but no path → skipped
            { "path": "/a", "branch": "main" },    // matched, but counts malformed
        ]}]});
        let results = json!({ "/a": { "ahead": 2 } }); // missing `behind`
        merge_ahead_behind(&mut result, results.as_object().unwrap());

        let worktrees = result.pointer("/repos/0/worktrees").unwrap();
        // Non-object element is untouched.
        assert_eq!(worktrees[0], json!("not-an-object"));
        // Pathless worktree: no sync fields inserted.
        assert!(worktrees[1].get("ahead").is_none(), "{:?}", worktrees[1]);
        // Malformed counts: neither side folded in (both-or-nothing).
        assert!(worktrees[2].get("ahead").is_none(), "{:?}", worktrees[2]);
        assert!(worktrees[2].get("behind").is_none(), "{:?}", worktrees[2]);
    }

    #[tokio::test]
    async fn enrich_ahead_behind_is_a_noop_when_there_are_no_worktrees() {
        // No worktrees → no batch to fetch → early return before any socket call,
        // so even a nonexistent socket leaves the tree untouched.
        let mut result = json!({ "repos": [] });
        let before = result.clone();
        enrich_ahead_behind(Path::new("/nonexistent/omni-dev-ab.sock"), &mut result).await;
        assert_eq!(result, before);
    }

    #[tokio::test]
    async fn enrich_ahead_behind_leaves_the_tree_when_the_daemon_is_unreachable() {
        // A real worktree but no daemon at the socket → the call fails and the tree
        // is returned as-is (rendered with `-` for sync), never erroring.
        let mut result =
            json!({ "repos": [{ "worktrees": [{ "path": "/x", "branch": "main" }] }] });
        enrich_ahead_behind(Path::new("/nonexistent/omni-dev-ab.sock"), &mut result).await;
        let wt = result.pointer("/repos/0/worktrees/0").unwrap();
        assert!(wt.get("ahead").is_none(), "{wt:?}");
        assert!(wt.get("behind").is_none(), "{wt:?}");
    }

    /// Spawns a minimal fake daemon on a short-path Unix socket that answers the
    /// one `ahead-behind` request with `reply` (the daemon's NDJSON reply shape).
    /// Returns the temp dir (kept alive for the socket's lifetime), the socket
    /// path, and the server task.
    fn fake_daemon_reply(
        reply: Value,
    ) -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<()>) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        // A short base path keeps the socket under the 104-byte `sockaddr_un` limit.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, LinesCodec::new());
            let _req = framed.next().await.unwrap().unwrap();
            framed
                .send(serde_json::to_string(&reply).unwrap())
                .await
                .unwrap();
        });
        (dir, sock, server)
    }

    #[tokio::test]
    async fn enrich_ahead_behind_folds_counts_from_a_live_socket() {
        let (_dir, sock, server) = fake_daemon_reply(
            json!({ "ok": true, "payload": { "results": { "/x": { "ahead": 3, "behind": 4 } } } }),
        );
        let mut result =
            json!({ "repos": [{ "worktrees": [{ "path": "/x", "branch": "main" }] }] });
        enrich_ahead_behind(&sock, &mut result).await;
        server.await.unwrap();

        let wt = result.pointer("/repos/0/worktrees/0").unwrap();
        assert_eq!(wt.get("ahead").and_then(Value::as_u64), Some(3));
        assert_eq!(wt.get("behind").and_then(Value::as_u64), Some(4));
    }

    #[tokio::test]
    async fn enrich_ahead_behind_ignores_a_reply_without_results() {
        // An `ok` reply carrying no `results` object (an older/oddly-shaped daemon)
        // leaves the tree unchanged rather than erroring.
        let (_dir, sock, server) = fake_daemon_reply(json!({ "ok": true, "payload": {} }));
        let mut result =
            json!({ "repos": [{ "worktrees": [{ "path": "/x", "branch": "main" }] }] });
        enrich_ahead_behind(&sock, &mut result).await;
        server.await.unwrap();

        let wt = result.pointer("/repos/0/worktrees/0").unwrap();
        assert!(wt.get("ahead").is_none(), "{wt:?}");
        assert!(wt.get("behind").is_none(), "{wt:?}");
    }

    #[test]
    fn render_tree_groups_repos_and_worktrees() {
        let result = json!({ "repos": [{
            "main_repo": "omni-dev",
            "github": { "owner": "rust-works", "name": "omni-dev" },
            "root": "/home/me/omni-dev",
            "worktrees": [
                { "path": "/home/me/omni-dev", "branch": "main", "ahead": 2, "behind": 0,
                  "is_main": true, "open": true, "window_key": "w1" },
                { "path": "/home/me/wt/issue-1300", "branch": "issue-1300", "ahead": 1, "behind": 3,
                  "is_main": false, "open": false },
            ],
        }]});
        let out = render_tree(&result);
        // Repo header carries the GitHub identity and root.
        let header = out.lines().next().unwrap();
        assert!(header.contains("omni-dev"), "{out}");
        assert!(header.contains("github: rust-works/omni-dev"), "{out}");
        assert!(header.contains("/home/me/omni-dev"), "{out}");
        // The main working tree is marked with `*`, its sync, and `open`.
        assert!(
            out.lines()
                .any(|l| l.contains("* main") && l.contains("+2 -0") && l.contains("open")),
            "{out}"
        );
        // The linked worktree is unmarked and not flagged open.
        let linked = out
            .lines()
            .find(|l| l.contains("issue-1300"))
            .unwrap_or_default();
        assert!(!linked.contains('*'), "{linked}");
        assert!(!linked.contains("open"), "{linked}");
        assert!(linked.contains("+1 -3"), "{linked}");
        // Header + two worktree rows.
        assert_eq!(out.lines().count(), 3, "{out}");
    }

    #[test]
    fn render_tree_separates_multiple_repos_with_blank_line() {
        let result = json!({ "repos": [
            {
                "main_repo": "alpha",
                "root": "/r/alpha",
                "worktrees": [
                    { "path": "/r/alpha", "branch": "main", "is_main": true, "open": false },
                ],
            },
            {
                "main_repo": "beta",
                "root": "/r/beta",
                "worktrees": [
                    { "path": "/r/beta", "branch": "main", "is_main": true, "open": false },
                ],
            },
        ]});
        let out = render_tree(&result);
        // Two headers, two worktree rows, and one blank separator between repos.
        assert!(
            out.contains("\n\nbeta"),
            "repos not blank-separated: {out:?}"
        );
        let alpha = out.find("alpha").unwrap();
        let beta = out.find("beta").unwrap();
        assert!(alpha < beta, "repo order not preserved: {out}");
        assert_eq!(out.lines().count(), 5, "{out:?}");
    }

    #[test]
    fn render_tree_omits_github_for_non_github_repo() {
        let result = json!({ "repos": [{
            "main_repo": "internal",
            "root": "/srv/internal",
            "worktrees": [
                { "path": "/srv/internal", "branch": "main", "is_main": true, "open": false },
            ],
        }]});
        let out = render_tree(&result);
        assert!(!out.contains("github:"), "{out}");
        assert!(out.lines().next().unwrap().contains("internal"), "{out}");
    }

    #[test]
    fn render_tree_strips_control_bytes() {
        // Control bytes in the repo name, github identity, branch, and path must
        // not reach the terminal (#1137), matching the `list` renderer.
        let result = json!({ "repos": [{
            "main_repo": "evil\x1b[31mrepo",
            "github": { "owner": "ow\x07ner", "name": "na\u{9b}2Jme" },
            "root": "/tmp/r\x1b]0;x\x07oot",
            "worktrees": [
                { "path": "/tmp/w\rt", "branch": "br\x1b[2Janch", "is_main": true, "open": true },
            ],
        }]});
        let out = render_tree(&result);
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
        // Embedded CR/LF cannot forge extra lines: header plus one worktree row.
        assert_eq!(out.lines().count(), 2, "{out:?}");
    }

    #[test]
    fn github_summary_needs_both_owner_and_name() {
        assert_eq!(
            github_summary(&json!({ "github": { "owner": "o", "name": "n" } })).as_deref(),
            Some("github: o/n")
        );
        assert_eq!(github_summary(&json!({ "github": { "owner": "o" } })), None);
        assert_eq!(github_summary(&json!({})), None);
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
