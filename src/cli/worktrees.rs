//! `omni-dev worktrees` — a thin client for the daemon's cross-window worktree
//! registry.
//!
//! Lifecycle stays on `omni-dev daemon` (`start`/`stop`/`status`/`restart`);
//! this command sends the `worktrees` service's ops over the daemon's Unix
//! control socket: the read views (`list`, `tree`, `tree --follow`), the actions
//! (`focus`, `close`, `show-closed`), and — for typed parity with the companion
//! (#1361) — the window feed ops (`register`/`heartbeat`/`unregister`) that let a
//! scripted/headless reporter or an integration test drive the registry the way
//! the VS Code extension does from each window.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
    /// Focus (raise) the VS Code window for a worktree folder.
    Focus(FocusCommand),
    /// Close a worktree's window and, for a linked worktree, delete it.
    Close(CloseCommand),
    /// Show or set whether closed worktrees are shown across all windows.
    ShowClosed(ShowClosedCommand),
    /// Register a window's open worktree folders (companion feed op).
    Register(RegisterCommand),
    /// Refresh a window's liveness and read any pending close directive.
    Heartbeat(HeartbeatCommand),
    /// Remove a window's registration (companion feed op).
    Unregister(UnregisterCommand),
}

impl WorktreesCommand {
    /// Executes the worktrees command.
    pub async fn execute(self) -> Result<()> {
        match self.command {
            WorktreesSubcommands::List(cmd) => cmd.execute().await,
            WorktreesSubcommands::Tree(cmd) => cmd.execute().await,
            WorktreesSubcommands::Focus(cmd) => cmd.execute().await,
            WorktreesSubcommands::Close(cmd) => cmd.execute().await,
            WorktreesSubcommands::ShowClosed(cmd) => cmd.execute().await,
            WorktreesSubcommands::Register(cmd) => cmd.execute().await,
            WorktreesSubcommands::Heartbeat(cmd) => cmd.execute().await,
            WorktreesSubcommands::Unregister(cmd) => cmd.execute().await,
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
    /// Stream live snapshots: re-render on every change until interrupted
    /// (Ctrl-C). Uses the daemon's `subscribe` push op.
    #[arg(short = 'f', long)]
    pub follow: bool,
}

impl TreeCommand {
    /// Executes the tree command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        if self.follow {
            return follow_tree_stream(&socket, self.output).await;
        }
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

/// Follows the daemon's `subscribe` push stream, re-rendering the tree on each
/// snapshot until the daemon closes the stream or the user interrupts (Ctrl-C).
///
/// Each frame is enriched with on-demand ahead/behind, exactly like the one-shot
/// path, so a followed view — table **or** JSON — carries the same shape as a
/// plain `tree` (the JSON stream stays one compact NDJSON frame per snapshot).
async fn follow_tree_stream(socket: &Path, output: TableOrJson) -> Result<()> {
    let mut sub = DaemonClient::new(socket)
        .subscribe(DaemonEnvelope::service(SERVICE, "subscribe", Value::Null))
        .await?;
    loop {
        tokio::select! {
            frame = sub.next() => {
                // `None` = the daemon closed the stream (shutdown); we are done.
                let Some(frame) = frame else { break };
                let mut payload = reply_payload(frame?)?;
                // Enrich before either renderer so `tree --follow` matches the
                // one-shot `tree` byte-for-byte in JSON and column-for-column in
                // the table (the one-shot enriches ahead of both branches too).
                enrich_ahead_behind(socket, &mut payload).await;
                match output {
                    // A compact one-line frame per snapshot (an NDJSON stream).
                    TableOrJson::Json => println!("{}", serde_json::to_string(&payload)?),
                    TableOrJson::Table => println!("{}", render_tree(&payload)),
                }
            }
            // Ctrl-C ends the follow; dropping `sub` closes the connection,
            // which the daemon reads as the stream's teardown.
            _ = tokio::signal::ctrl_c() => break,
        }
    }
    Ok(())
}

/// Focuses (raises) the VS Code window for a worktree folder.
///
/// Reuses the daemon's `open` op — the same launcher path the macOS tray's
/// per-window "focus" action drives (`OMNI_DEV_VSCODE_BIN` → well-known paths →
/// `code`), which VS Code uses to reuse an already-open window. This makes that
/// tray-only capability reachable from the CLI on Linux/headless too (#1113).
#[derive(Parser)]
pub struct FocusCommand {
    /// Worktree folder whose window to focus. Shown by `worktrees tree`/`list`.
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl FocusCommand {
    /// Executes the focus command.
    pub async fn execute(self) -> Result<()> {
        // Resolve to an absolute path client-side: the daemon runs in a different
        // cwd and guards the `open` path as absolute-and-existing, so a relative
        // path would be meaningless there. A clear error here beats the daemon's.
        let path = std::fs::canonicalize(&self.path)
            .with_context(|| format!("cannot resolve worktree path: {}", self.path.display()))?;
        let socket = server::resolve_socket(self.socket)?;
        call(&socket, "open", json!({ "path": path.to_string_lossy() })).await?;
        println!("Focused {}", path.display());
        Ok(())
    }
}

/// Closes a worktree's window and, for a linked worktree, deletes it — the
/// daemon's two-phase `close` op driven from the CLI.
///
/// A CLI process is never a VS Code window, so it omits `requester_key`: the
/// daemon then treats the close as cross-window, signalling every owning window
/// to close and waiting (bounded ~20s) for them to unregister before it prunes.
/// All destructive/git logic (the `git2` prune, the main-tree refusal) stays in
/// the daemon (ADR-0049); the CLI adds no new authority.
#[derive(Parser)]
pub struct CloseCommand {
    /// Worktree folder to close. A linked worktree is deleted; the main working
    /// tree only has its window closed (never deleted).
    #[arg(value_name = "PATH")]
    pub path: PathBuf,
    /// Only close the worktree's window(s); never delete the worktree.
    #[arg(long)]
    pub window_only: bool,
    /// Run the safety check and print the report, but do not close or delete.
    #[arg(long)]
    pub dry_run: bool,
    /// Skip the interactive confirmation before deleting.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl CloseCommand {
    /// Executes the close command, confirming a delete interactively via stdin.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(confirm_removal).await
    }

    /// The close core, with the destructive-confirm decision injected as
    /// `confirm(has_risks) -> bool`. Splitting it this way keeps the abort and
    /// confirmed-execute branches unit-testable without driving real stdin (which
    /// would block a test on a TTY); production wires in [`confirm_removal`].
    async fn execute_with<F, Fut>(self, confirm: F) -> Result<()>
    where
        F: FnOnce(bool) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        // Resolve to an absolute path client-side (like `focus`): the daemon runs
        // in a different cwd and matches the target by canonical path.
        let path = std::fs::canonicalize(&self.path)
            .with_context(|| format!("cannot resolve worktree path: {}", self.path.display()))?;
        let path_str = path.to_string_lossy().to_string();
        let socket = server::resolve_socket(self.socket)?;

        // "Close Window": non-destructive, no safety check — the daemon closes the
        // owning window(s) and never inspects git. `--dry-run` is honoured here
        // too, so the combination never has a side effect.
        if self.window_only {
            if self.dry_run {
                println!(
                    "Would close the window for {} (dry run; nothing closed)",
                    path.display()
                );
                return Ok(());
            }
            call(
                &socket,
                "close",
                json!({ "path": path_str, "remove": false }),
            )
            .await?;
            println!("Closed the window for {}", path.display());
            return Ok(());
        }

        // Phase 1: the side-effect-free safety check (remove:true, unconfirmed).
        let report = call(
            &socket,
            "close",
            json!({ "path": path_str, "remove": true }),
        )
        .await?;
        println!("{}", render_safety_report(&path, &report));

        if self.dry_run {
            return Ok(());
        }
        // The daemon refuses to remove the main working tree; fail fast rather than
        // send a phase-2 execute it would reject.
        if report.get("removable").and_then(Value::as_bool) != Some(true) {
            bail!(
                "{} is not a removable worktree (nothing deleted); \
                 use --window-only to just close its window",
                path.display()
            );
        }
        let has_risks = report
            .get("risks")
            .and_then(Value::as_array)
            .is_some_and(|r| !r.is_empty());
        if !self.yes && !confirm(has_risks).await {
            println!("Aborted; nothing was deleted.");
            return Ok(());
        }

        // Phase 2: execute the delete.
        call(
            &socket,
            "close",
            json!({ "path": path_str, "remove": true, "confirmed": true }),
        )
        .await?;
        println!("Deleted worktree {}", path.display());
        Ok(())
    }
}

/// Shows or sets the cross-window "show closed worktrees" toggle.
///
/// With a boolean argument it sets the daemon-backed value (`set-show-closed`),
/// which every subscribed window re-reads; with no argument it reads the current
/// value from the top-level `show_closed` of a `tree` snapshot.
#[derive(Parser)]
pub struct ShowClosedCommand {
    /// New value (`true`/`false`). Omit to read the current value.
    #[arg(value_name = "BOOL", value_parser = clap::builder::BoolishValueParser::new())]
    pub value: Option<bool>,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl ShowClosedCommand {
    /// Executes the show-closed command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        if let Some(show_closed) = self.value {
            call(
                &socket,
                "set-show-closed",
                json!({ "show_closed": show_closed }),
            )
            .await?;
            println!("show-closed: {show_closed}");
        } else {
            // The value is not a dedicated op — it rides the `tree` snapshot.
            let tree = call(&socket, "tree", Value::Null).await?;
            let current = tree
                .get("show_closed")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            println!("show-closed: {current}");
        }
        Ok(())
    }
}

/// Registers a window's open worktree folders (a companion feed op).
///
/// Exposed as a typed command so scripted/headless reporters and integration
/// tests can drive the registry the way the VS Code companion does. Mirrors
/// `RegisterRequest`.
#[derive(Parser)]
pub struct RegisterCommand {
    /// Stable per-window identity (the companion generates a per-activate UUID).
    #[arg(long, value_name = "KEY")]
    pub key: String,
    /// A workspace-folder path (repeatable).
    #[arg(long = "folder", value_name = "PATH")]
    pub folders: Vec<PathBuf>,
    /// Repository root or name, when the window has one.
    #[arg(long, value_name = "REPO")]
    pub repo: Option<String>,
    /// Window title, for display.
    #[arg(long, value_name = "TITLE")]
    pub title: Option<String>,
    /// Reporting process id.
    #[arg(long, value_name = "PID")]
    pub pid: Option<u32>,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl RegisterCommand {
    /// Executes the register command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let payload = json!({
            "key": self.key,
            "folders": self.folders,
            "repo": self.repo,
            "title": self.title,
            "pid": self.pid,
        });
        call(&socket, "register", payload).await?;
        println!("Registered {}", self.key);
        Ok(())
    }
}

/// Refreshes a window's liveness and reports the daemon's reply.
///
/// A companion feed op made typed: the reply carries `known` (false asks the
/// window to re-register after a daemon restart) and, when present, `close` (a
/// cross-window close directive).
#[derive(Parser)]
pub struct HeartbeatCommand {
    /// The window key to heartbeat.
    #[arg(long, value_name = "KEY")]
    pub key: String,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl HeartbeatCommand {
    /// Executes the heartbeat command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let reply = call(&socket, "heartbeat", json!({ "key": self.key })).await?;
        let known = reply.get("known").and_then(Value::as_bool).unwrap_or(false);
        // `close` is omitted from the reply when false; treat absent as false.
        let close = reply.get("close").and_then(Value::as_bool).unwrap_or(false);
        println!("known: {known}");
        println!("close: {close}");
        Ok(())
    }
}

/// Removes a window's registration — a companion feed op made typed. Prints
/// whether an entry was actually removed.
#[derive(Parser)]
pub struct UnregisterCommand {
    /// The window key to unregister.
    #[arg(long, value_name = "KEY")]
    pub key: String,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl UnregisterCommand {
    /// Executes the unregister command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let reply = call(&socket, "unregister", json!({ "key": self.key })).await?;
        let removed = reply
            .get("removed")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!("removed: {removed}");
        Ok(())
    }
}

/// Renders a phase-1 `close` `SafetyReport` as a human-readable block: whether
/// the target is removable, whether it is the main tree, whether a window has it
/// open (and which), and any `risks`/`info` notes. Every daemon-supplied string is
/// `sanitize`d (#1137); the booleans/counts are daemon-computed and safe.
fn render_safety_report(path: &Path, report: &Value) -> String {
    let removable = report
        .get("removable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let is_main = report
        .get("is_main")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let open = report.get("open").and_then(Value::as_bool).unwrap_or(false);
    let mut out = format!("Worktree: {}", path.display());
    out.push_str(&format!("\n  removable:        {removable}"));
    out.push_str(&format!("\n  main working tree: {is_main}"));
    if open {
        let key = sanitize(
            report
                .get("window_key")
                .and_then(Value::as_str)
                .unwrap_or("-"),
        );
        let count = report
            .get("window_folder_count")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        out.push_str(&format!(
            "\n  open in a window:  yes (key {key}, {count} folder(s))"
        ));
    } else {
        out.push_str("\n  open in a window:  no");
    }
    out.push_str(&render_notes("risks", report.get("risks")));
    out.push_str(&render_notes("info", report.get("info")));
    out
}

/// Renders a labelled list of `close` safety notes (`risks` or `info`), each a
/// `- [kind] detail` line with both fields `sanitize`d. Empty when there are none.
fn render_notes(label: &str, notes: Option<&Value>) -> String {
    let notes = notes
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if notes.is_empty() {
        return String::new();
    }
    let mut out = format!("\n  {label}:");
    for note in notes {
        let kind = sanitize(note.get("kind").and_then(Value::as_str).unwrap_or("-"));
        let detail = sanitize(note.get("detail").and_then(Value::as_str).unwrap_or(""));
        out.push_str(&format!("\n    - [{kind}] {detail}"));
    }
    out
}

/// Prompts on stderr for confirmation before a destructive delete and returns
/// whether the user assented, reading the answer from real stdin.
///
/// A thin wrapper over [`confirm_removal_with`] that supplies the live stdin
/// reader; the prompt-and-decide logic is factored out so it stays testable
/// without driving real stdin.
async fn confirm_removal(has_risks: bool) -> bool {
    confirm_removal_with(has_risks, read_stdin_line()).await
}

/// Prints the confirmation prompt and resolves the (already-injected) read of the
/// user's answer into a yes/no decision. Any read error, a closed stdin (EOF), or
/// a join failure surfaces as `None` and is treated as "no", so a delete never
/// proceeds unattended.
async fn confirm_removal_with(
    has_risks: bool,
    read: impl std::future::Future<Output = Option<String>>,
) -> bool {
    use std::io::Write;
    eprint!("{}", confirm_prompt(has_risks));
    let _ = std::io::stderr().flush();
    read.await.as_deref().is_some_and(answer_is_yes)
}

/// Reads one line from stdin on a dedicated thread (`spawn_blocking`) so it never
/// stalls an async worker while it waits for input. Returns `None` on any read
/// error, EOF, or join failure.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer).ok().map(|_| answer)
    })
    .await
    .ok()
    .flatten()
}

/// The confirmation prompt shown before a delete — it names the risks when the
/// safety report flagged any. Pure, so the wording is unit-testable.
fn confirm_prompt(has_risks: bool) -> &'static str {
    if has_risks {
        "Delete this worktree despite the risks above? [y/N] "
    } else {
        "Delete this worktree? [y/N] "
    }
}

/// Whether a confirmation answer is an affirmative (`y`/`yes`, case-insensitive).
/// Split out so the yes/no decision is unit-testable without real stdin.
fn answer_is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_lowercase().as_str(), "y" | "yes")
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
    fn focus_parses_path_and_socket() {
        // Routing: `worktrees focus` maps to the Focus variant.
        assert!(matches!(
            parse(&["focus", "/home/me/wt"]),
            WorktreesSubcommands::Focus(_)
        ));
        // The path is a required positional; `--socket` is optional.
        let cmd = FocusCommand::try_parse_from(["focus", "/home/me/wt"]).unwrap();
        assert_eq!(cmd.path, Path::new("/home/me/wt"));
        assert!(cmd.socket.is_none());

        let cmd = FocusCommand::try_parse_from(["focus", "/home/me/wt", "--socket", "/tmp/d.sock"])
            .unwrap();
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));

        // The path is required.
        assert!(FocusCommand::try_parse_from(["focus"]).is_err());
    }

    #[tokio::test]
    async fn focus_errors_on_a_nonexistent_path_before_any_socket_call() {
        // Canonicalisation fails for a path that does not exist, so `focus`
        // reports a clear error without needing a daemon.
        let cmd = FocusCommand {
            path: PathBuf::from("/nonexistent/omni-dev-focus-xyz"),
            socket: Some(PathBuf::from("/nonexistent/omni-dev-focus.sock")),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(
            err.to_string().contains("cannot resolve worktree path"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn focus_sends_the_open_op_for_an_existing_folder() {
        // A real (temp) folder canonicalises, so `focus` sends the `open` op to
        // the daemon; the fake daemon acknowledges it. Routed through the top-level
        // `WorktreesCommand::execute` so its `Focus` dispatch arm is exercised too.
        let (_dir, sock, server) =
            fake_daemon_reply(json!({ "ok": true, "payload": { "ok": true } }));
        let target = tempfile::tempdir().unwrap();
        let cmd = WorktreesCommand {
            command: WorktreesSubcommands::Focus(FocusCommand {
                path: target.path().to_path_buf(),
                socket: Some(sock),
            }),
        };
        cmd.execute().await.unwrap();
        server.await.unwrap();
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

    // --- #1361 typed op-parity commands -------------------------------------

    #[test]
    fn new_subcommands_route_and_require_their_args() {
        assert!(matches!(
            parse(&["close", "/home/me/wt"]),
            WorktreesSubcommands::Close(_)
        ));
        assert!(matches!(
            parse(&["show-closed"]),
            WorktreesSubcommands::ShowClosed(_)
        ));
        assert!(matches!(
            parse(&["register", "--key", "w1"]),
            WorktreesSubcommands::Register(_)
        ));
        assert!(matches!(
            parse(&["heartbeat", "--key", "w1"]),
            WorktreesSubcommands::Heartbeat(_)
        ));
        assert!(matches!(
            parse(&["unregister", "--key", "w1"]),
            WorktreesSubcommands::Unregister(_)
        ));

        // Required args are enforced.
        assert!(CloseCommand::try_parse_from(["close"]).is_err());
        assert!(RegisterCommand::try_parse_from(["register"]).is_err());
        assert!(HeartbeatCommand::try_parse_from(["heartbeat"]).is_err());
        assert!(UnregisterCommand::try_parse_from(["unregister"]).is_err());
    }

    #[test]
    fn close_parses_flags() {
        let cmd = CloseCommand::try_parse_from([
            "close",
            "/home/me/wt",
            "--window-only",
            "--dry-run",
            "-y",
            "--socket",
            "/tmp/d.sock",
        ])
        .unwrap();
        assert_eq!(cmd.path, Path::new("/home/me/wt"));
        assert!(cmd.window_only && cmd.dry_run && cmd.yes);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));

        // Defaults: no flags set.
        let cmd = CloseCommand::try_parse_from(["close", "/home/me/wt"]).unwrap();
        assert!(!cmd.window_only && !cmd.dry_run && !cmd.yes);
    }

    #[test]
    fn tree_follow_flag_parses() {
        let cmd = TreeCommand::try_parse_from(["tree", "--follow"]).unwrap();
        assert!(cmd.follow);
        let cmd = TreeCommand::try_parse_from(["tree", "-f", "-o", "json"]).unwrap();
        assert!(cmd.follow);
        assert_eq!(cmd.output, TableOrJson::Json);
        let cmd = TreeCommand::try_parse_from(["tree"]).unwrap();
        assert!(!cmd.follow);
    }

    #[test]
    fn show_closed_parses_optional_bool() {
        assert!(ShowClosedCommand::try_parse_from(["show-closed"])
            .unwrap()
            .value
            .is_none());
        assert_eq!(
            ShowClosedCommand::try_parse_from(["show-closed", "false"])
                .unwrap()
                .value,
            Some(false)
        );
        assert_eq!(
            ShowClosedCommand::try_parse_from(["show-closed", "true"])
                .unwrap()
                .value,
            Some(true)
        );
        // A non-boolean value is rejected.
        assert!(ShowClosedCommand::try_parse_from(["show-closed", "maybe"]).is_err());
    }

    #[test]
    fn register_collects_repeated_folders() {
        let cmd = RegisterCommand::try_parse_from([
            "register", "--key", "w1", "--folder", "/a", "--folder", "/b", "--repo", "r", "--pid",
            "42",
        ])
        .unwrap();
        assert_eq!(cmd.key, "w1");
        assert_eq!(cmd.folders, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(cmd.repo.as_deref(), Some("r"));
        assert_eq!(cmd.pid, Some(42));
    }

    #[test]
    fn answer_is_yes_accepts_only_affirmatives() {
        for yes in ["y", "Y", "yes", "YES", " yes \n"] {
            assert!(answer_is_yes(yes), "{yes:?}");
        }
        for no in ["", "n", "no", "nope", "true", "\n"] {
            assert!(!answer_is_yes(no), "{no:?}");
        }
    }

    #[test]
    fn confirm_prompt_mentions_risks_only_when_present() {
        assert!(
            confirm_prompt(true).contains("risks"),
            "{}",
            confirm_prompt(true)
        );
        assert!(
            !confirm_prompt(false).contains("risks"),
            "{}",
            confirm_prompt(false)
        );
        // Both wordings default to No.
        assert!(confirm_prompt(true).contains("[y/N]"));
        assert!(confirm_prompt(false).contains("[y/N]"));
    }

    #[test]
    fn render_safety_report_renders_fields_and_notes() {
        let report = json!({
            "removable": true,
            "is_main": false,
            "open": true,
            "window_key": "w1",
            "window_folder_count": 2,
            "risks": [{ "kind": "dirty", "detail": "uncommitted changes" }],
            "info": [{ "kind": "unpushed", "detail": "2 unpushed commits" }],
        });
        let out = render_safety_report(Path::new("/home/me/wt"), &report);
        assert!(out.contains("/home/me/wt"), "{out}");
        assert!(out.contains("removable:        true"), "{out}");
        assert!(
            out.contains("open in a window:  yes (key w1, 2 folder(s))"),
            "{out}"
        );
        assert!(out.contains("[dirty] uncommitted changes"), "{out}");
        assert!(out.contains("[unpushed] 2 unpushed commits"), "{out}");
    }

    #[test]
    fn render_safety_report_handles_no_window_and_no_notes() {
        let report = json!({ "removable": false, "is_main": true, "open": false });
        let out = render_safety_report(Path::new("/r"), &report);
        assert!(out.contains("removable:        false"), "{out}");
        assert!(out.contains("main working tree: true"), "{out}");
        assert!(out.contains("open in a window:  no"), "{out}");
        // No risks/info sections are emitted when both are absent.
        assert!(!out.contains("risks:"), "{out}");
        assert!(!out.contains("info:"), "{out}");
    }

    #[test]
    fn render_safety_report_strips_control_bytes() {
        // Daemon-supplied strings (window key, note kind/detail) must not inject
        // terminal escapes (#1137).
        let report = json!({
            "removable": true, "is_main": false, "open": true,
            "window_key": "w\x1b[31m1", "window_folder_count": 1,
            "risks": [{ "kind": "di\x07rty", "detail": "lost\r\nrow" }],
            "info": [],
        });
        let out = render_safety_report(Path::new("/r"), &report);
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
    }

    /// Spawns a fake daemon that answers `replies.len()` sequential connections,
    /// each with the next reply, and **returns the request envelope(s) it
    /// received** (via the join handle) so a test can assert the exact wire shape
    /// — op and payload — that the client sent, not just the round-trip. Same
    /// short-path `/tmp` socket as `fake_daemon_reply`.
    fn fake_daemon_seq(
        replies: Vec<Value>,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::task::JoinHandle<Vec<Value>>,
    ) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            let mut requests = Vec::new();
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let mut framed = Framed::new(stream, LinesCodec::new());
                let req = framed.next().await.unwrap().unwrap();
                requests.push(serde_json::from_str::<Value>(&req).unwrap());
                framed
                    .send(serde_json::to_string(&reply).unwrap())
                    .await
                    .unwrap();
            }
            requests
        });
        (dir, sock, server)
    }

    #[tokio::test]
    async fn close_window_only_sends_remove_false() {
        let (_dir, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "closed": true } })]);
        let target = tempfile::tempdir().unwrap();
        CloseCommand {
            path: target.path().to_path_buf(),
            window_only: true,
            dry_run: false,
            yes: false,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        // Exactly one op, and it is a non-destructive close: remove:false, never
        // confirmed. A payload-field rename would fail here.
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["op"], "close");
        assert_eq!(reqs[0]["payload"]["remove"], json!(false));
        assert!(
            reqs[0]["payload"].get("confirmed").is_none(),
            "{:?}",
            reqs[0]
        );
        // The path is canonicalized client-side before it is sent.
        let want = std::fs::canonicalize(target.path()).unwrap();
        assert_eq!(reqs[0]["payload"]["path"], json!(want.to_string_lossy()));
    }

    #[tokio::test]
    async fn close_window_only_dry_run_never_contacts_the_daemon() {
        // `--window-only --dry-run` must have no side effect: it prints what would
        // happen and returns without a socket call, so a nonexistent socket is fine.
        let target = tempfile::tempdir().unwrap();
        CloseCommand {
            path: target.path().to_path_buf(),
            window_only: true,
            dry_run: true,
            yes: false,
            socket: Some(PathBuf::from("/nonexistent/omni-dev-close-dry.sock")),
        }
        .execute()
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn close_dry_run_only_runs_phase_one() {
        // A single connection: the safety check. `--dry-run` never sends phase-2.
        let (_dir, sock, server) = fake_daemon_seq(vec![json!({
            "ok": true,
            "payload": { "removable": true, "is_main": false, "open": false,
                         "window_folder_count": 0, "risks": [], "info": [] }
        })]);
        let target = tempfile::tempdir().unwrap();
        CloseCommand {
            path: target.path().to_path_buf(),
            window_only: false,
            dry_run: true,
            yes: false,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        // Only the phase-1 safety check: remove:true, unconfirmed. No phase-2.
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0]["op"], "close");
        assert_eq!(reqs[0]["payload"]["remove"], json!(true));
        assert!(
            reqs[0]["payload"].get("confirmed").is_none(),
            "{:?}",
            reqs[0]
        );
    }

    #[tokio::test]
    async fn close_yes_executes_phase_two() {
        // Two connections: phase-1 safety report (removable), then phase-2 delete.
        let (_dir, sock, server) = fake_daemon_seq(vec![
            json!({ "ok": true, "payload": { "removable": true, "is_main": false,
                    "open": false, "window_folder_count": 0, "risks": [], "info": [] } }),
            json!({ "ok": true, "payload": { "removed": true } }),
        ]);
        let target = tempfile::tempdir().unwrap();
        CloseCommand {
            path: target.path().to_path_buf(),
            window_only: false,
            dry_run: false,
            yes: true,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        // Phase 1 is the unconfirmed safety check; phase 2 carries confirmed:true.
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0]["op"], "close");
        assert_eq!(reqs[0]["payload"]["remove"], json!(true));
        assert!(
            reqs[0]["payload"].get("confirmed").is_none(),
            "{:?}",
            reqs[0]
        );
        assert_eq!(reqs[1]["op"], "close");
        assert_eq!(reqs[1]["payload"]["remove"], json!(true));
        assert_eq!(reqs[1]["payload"]["confirmed"], json!(true));
        // A CLI is never a VS Code window, so it never claims a requester_key.
        assert!(
            reqs[1]["payload"].get("requester_key").is_none(),
            "{:?}",
            reqs[1]
        );
    }

    #[tokio::test]
    async fn close_refuses_a_non_removable_target() {
        // Phase-1 reports not-removable (e.g. the main tree); the command prints
        // the report then errors without a phase-2 execute (one connection only).
        let (_dir, sock, server) = fake_daemon_seq(vec![json!({
            "ok": true,
            "payload": { "removable": false, "is_main": true, "open": false,
                         "window_folder_count": 0, "risks": [], "info": [] }
        })]);
        let target = tempfile::tempdir().unwrap();
        let err = CloseCommand {
            path: target.path().to_path_buf(),
            window_only: false,
            dry_run: false,
            yes: true,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("not a removable worktree"),
            "{err}"
        );
        // Only the phase-1 check ran — no destructive phase-2 was sent.
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn close_errors_on_a_nonexistent_path_before_any_socket_call() {
        let err = CloseCommand {
            path: PathBuf::from("/nonexistent/omni-dev-close-xyz"),
            window_only: false,
            dry_run: false,
            yes: true,
            socket: Some(PathBuf::from("/nonexistent/omni-dev-close.sock")),
        }
        .execute()
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("cannot resolve worktree path"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn show_closed_sets_and_reads() {
        // Set: one connection acknowledging set-show-closed.
        let (_dir, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "ok": true } })]);
        ShowClosedCommand {
            value: Some(false),
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        assert_eq!(reqs[0]["op"], "set-show-closed");
        assert_eq!(reqs[0]["payload"]["show_closed"], json!(false));

        // Read: one connection returning a `tree` snapshot's `show_closed`.
        let (_dir, sock, server) = fake_daemon_seq(vec![
            json!({ "ok": true, "payload": { "repos": [], "show_closed": false } }),
        ]);
        ShowClosedCommand {
            value: None,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        // The no-arg read is served by a plain `tree` fetch, not a dedicated op.
        assert_eq!(server.await.unwrap()[0]["op"], "tree");
    }

    #[tokio::test]
    async fn register_heartbeat_unregister_send_their_ops() {
        let (_dir, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "ok": true } })]);
        RegisterCommand {
            key: "w1".to_string(),
            folders: vec![PathBuf::from("/a")],
            repo: Some("r".to_string()),
            title: None,
            pid: Some(7),
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        // The RegisterRequest wire shape: op + every field the daemon reads.
        assert_eq!(reqs[0]["op"], "register");
        assert_eq!(reqs[0]["payload"]["key"], json!("w1"));
        assert_eq!(reqs[0]["payload"]["folders"], json!(["/a"]));
        assert_eq!(reqs[0]["payload"]["repo"], json!("r"));
        assert_eq!(reqs[0]["payload"]["pid"], json!(7));

        let (_dir, sock, server) = fake_daemon_seq(vec![
            json!({ "ok": true, "payload": { "known": true, "close": true } }),
        ]);
        HeartbeatCommand {
            key: "w1".to_string(),
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        assert_eq!(reqs[0]["op"], "heartbeat");
        assert_eq!(reqs[0]["payload"]["key"], json!("w1"));

        let (_dir, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "removed": true } })]);
        UnregisterCommand {
            key: "w1".to_string(),
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        assert_eq!(reqs[0]["op"], "unregister");
        assert_eq!(reqs[0]["payload"]["key"], json!("w1"));
    }

    #[tokio::test]
    async fn tree_follow_renders_each_pushed_frame() {
        use crate::daemon::testutil::fake_daemon_stream;

        // JSON follow: two non-empty frames printed as an NDJSON stream, then EOF.
        let (_dir, sock, server) = fake_daemon_stream(vec![
            json!({ "ok": true, "payload": { "repos": [], "show_closed": true } }),
            json!({ "ok": true, "payload": { "repos": [], "show_closed": false } }),
        ]);
        follow_tree_stream(&sock, TableOrJson::Json).await.unwrap();
        server.await.unwrap();

        // Table follow: empty-repos frames render "No repositories open." and never
        // trigger an ahead/behind socket call (the enrich guard early-returns).
        let (_dir, sock, server) = fake_daemon_stream(vec![
            json!({ "ok": true, "payload": { "repos": [], "show_closed": true } }),
        ]);
        follow_tree_stream(&sock, TableOrJson::Table).await.unwrap();
        server.await.unwrap();

        // Through `TreeCommand::execute` with `--follow`, covering the follow-dispatch
        // branch (not just the free `follow_tree_stream`).
        let (_dir, sock, server) = fake_daemon_stream(vec![
            json!({ "ok": true, "payload": { "repos": [], "show_closed": true } }),
        ]);
        TreeCommand {
            socket: Some(sock),
            output: TableOrJson::Json,
            follow: true,
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn worktrees_command_routes_each_new_subcommand() {
        // Route every new variant through the outer `WorktreesCommand::execute` so
        // its dispatch arms are exercised (the wire-shape tests drive the leaf
        // `execute` directly).
        let target = tempfile::tempdir().unwrap();
        // Close: `--window-only --dry-run` contacts no daemon.
        WorktreesCommand {
            command: WorktreesSubcommands::Close(CloseCommand {
                path: target.path().to_path_buf(),
                window_only: true,
                dry_run: true,
                yes: false,
                socket: Some(PathBuf::from("/nonexistent/omni-dev-route.sock")),
            }),
        }
        .execute()
        .await
        .unwrap();

        // ShowClosed (set).
        let (_d, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "ok": true } })]);
        WorktreesCommand {
            command: WorktreesSubcommands::ShowClosed(ShowClosedCommand {
                value: Some(true),
                socket: Some(sock),
            }),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();

        // Register.
        let (_d, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "ok": true } })]);
        WorktreesCommand {
            command: WorktreesSubcommands::Register(RegisterCommand {
                key: "w1".to_string(),
                folders: vec![],
                repo: None,
                title: None,
                pid: None,
                socket: Some(sock),
            }),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();

        // Heartbeat.
        let (_d, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "known": true } })]);
        WorktreesCommand {
            command: WorktreesSubcommands::Heartbeat(HeartbeatCommand {
                key: "w1".to_string(),
                socket: Some(sock),
            }),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();

        // Unregister.
        let (_d, sock, server) =
            fake_daemon_seq(vec![json!({ "ok": true, "payload": { "removed": true } })]);
        WorktreesCommand {
            command: WorktreesSubcommands::Unregister(UnregisterCommand {
                key: "w1".to_string(),
                socket: Some(sock),
            }),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn close_aborts_when_confirmation_is_declined() {
        // Phase-1 says removable; the injected confirmer declines → the "Aborted"
        // branch runs and no phase-2 delete is sent (one connection only). This
        // covers the interactive-decline path without driving real stdin.
        let (_dir, sock, server) = fake_daemon_seq(vec![json!({
            "ok": true,
            "payload": { "removable": true, "is_main": false, "open": false,
                         "window_folder_count": 0, "risks": [], "info": [] }
        })]);
        let target = tempfile::tempdir().unwrap();
        CloseCommand {
            path: target.path().to_path_buf(),
            window_only: false,
            dry_run: false,
            yes: false,
            socket: Some(sock),
        }
        .execute_with(|_has_risks| async { false })
        .await
        .unwrap();
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn close_deletes_when_confirmation_is_accepted() {
        // Phase-1 removable, the injected confirmer accepts → phase-2 executes with
        // confirmed:true.
        let (_dir, sock, server) = fake_daemon_seq(vec![
            json!({ "ok": true, "payload": { "removable": true, "is_main": false,
                    "open": false, "window_folder_count": 0, "risks": [], "info": [] } }),
            json!({ "ok": true, "payload": { "removed": true } }),
        ]);
        let target = tempfile::tempdir().unwrap();
        CloseCommand {
            path: target.path().to_path_buf(),
            window_only: false,
            dry_run: false,
            yes: false,
            socket: Some(sock),
        }
        .execute_with(|_has_risks| async { true })
        .await
        .unwrap();
        let reqs = server.await.unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[1]["payload"]["confirmed"], json!(true));
    }

    #[tokio::test]
    async fn confirm_removal_with_decides_from_the_answer() {
        // A "yes"/"y" answer confirms; "no", an empty line, and a `None` (EOF/read
        // error) all decline — for both the risky and clean prompt wordings.
        assert!(confirm_removal_with(false, async { Some("y\n".to_string()) }).await);
        assert!(confirm_removal_with(true, async { Some("YES".to_string()) }).await);
        assert!(!confirm_removal_with(false, async { Some("n".to_string()) }).await);
        assert!(!confirm_removal_with(true, async { Some(String::new()) }).await);
        assert!(!confirm_removal_with(false, async { None }).await);
    }
}
