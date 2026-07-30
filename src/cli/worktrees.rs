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
use crate::git::worktree_rebase::{
    self, FetchOutcome, RebaseOptions, RebaseResult, Selection, SkipReason, WorktreeOutcome,
};

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
    /// Rebase worktrees onto the remote default branch, fetching it once per repo.
    Rebase(RebaseCommand),
    /// Enqueue eligible worktrees' PRs into the GitHub merge queue.
    MergeQueue(MergeQueueCommand),
    /// Move and resize worktrees' open windows to match a reference window.
    Reposition(RepositionCommand),
    /// Signal worktrees' open windows to reload themselves.
    Reload(ReloadCommand),
    /// Show or set whether closed worktrees are shown across all windows.
    ShowClosed(ShowClosedCommand),
    /// Register a window's open worktree folders (companion feed op).
    Register(RegisterCommand),
    /// Refresh a window's liveness and read any pending close/reload directive.
    Heartbeat(HeartbeatCommand),
    /// Remove a window's registration (companion feed op).
    Unregister(UnregisterCommand),
}

impl WorktreesCommand {
    /// Executes the worktrees command.
    ///
    /// `repo` is the global `-C/--repo` location, resolved once in [`crate::cli`]
    /// and threaded down rather than re-read from the ambient CWD. Only `rebase`
    /// uses it (it is the one subcommand that acts on the local repository); the
    /// daemon-client subcommands address worktrees by absolute path instead.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        match self.command {
            WorktreesSubcommands::List(cmd) => cmd.execute().await,
            WorktreesSubcommands::Tree(cmd) => cmd.execute().await,
            WorktreesSubcommands::Focus(cmd) => cmd.execute().await,
            WorktreesSubcommands::Close(cmd) => cmd.execute().await,
            WorktreesSubcommands::Rebase(cmd) => cmd.execute(repo).await,
            WorktreesSubcommands::MergeQueue(cmd) => cmd.execute().await,
            WorktreesSubcommands::Reposition(cmd) => cmd.execute().await,
            WorktreesSubcommands::Reload(cmd) => cmd.execute().await,
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

/// Rebases worktrees onto the repository's remote default branch, fetching that
/// branch **exactly once per repository** (#1400).
///
/// Unlike every other `worktrees` subcommand this runs **entirely locally** and
/// never talks to the daemon — **by choice, not by necessity** (ADR-0059). The
/// daemon hosts the same engine behind its two-phase `rebase` op, which is what
/// the tree view's "Rebase on main" drives; keeping this command local is what
/// makes a batch rebase work with **no daemon running at all**, and keeps
/// `--onto`/`--all` (CLI-only concerns) out of the wire protocol. The git work
/// lives in [`crate::git::worktree_rebase`]; see ADR-0059, ADR-0055, ADR-0003.
///
/// A rebase rewrites branch history, so it confirms by default (`--dry-run` to
/// preview, `-y` to skip the prompt) in the spirit of ADR-0027.
#[derive(Parser)]
pub struct RebaseCommand {
    /// Worktree folders to rebase. Omit these and pass `--all` to rebase every
    /// worktree of the current repository, including its main working tree.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// Rebase every worktree of the current repository, including its main
    /// working tree.
    #[arg(long)]
    pub all: bool,
    /// Rebase onto this ref instead of the remote default branch. A
    /// `<remote>/<branch>` value is still fetched once up front.
    #[arg(long, value_name = "REF")]
    pub onto: Option<String>,
    /// Stash uncommitted changes around each rebase instead of skipping the
    /// worktree.
    #[arg(long)]
    pub autostash: bool,
    /// Fetch and report what would be rebased, but rebase nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Leave a conflicting worktree mid-rebase to resolve in place, instead of
    /// aborting it back to its previous state.
    #[arg(long)]
    pub keep_conflicts: bool,
    /// Skip the interactive confirmation before rebasing.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
}

impl RebaseCommand {
    /// Executes the rebase command, confirming interactively via stdin.
    pub async fn execute(self, repo: Option<&Path>) -> Result<()> {
        self.execute_with(repo, confirm_rebase).await
    }

    /// The rebase core, with the confirm decision injected as
    /// `confirm(pending) -> bool`. Splitting it this way keeps the abort and
    /// confirmed branches unit-testable without driving real stdin (which would
    /// block a test on a TTY); production wires in [`confirm_rebase`].
    async fn execute_with<F, Fut>(self, repo: Option<&Path>, confirm: F) -> Result<()>
    where
        F: FnOnce(usize) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let selection = self.selection(repo)?;
        let opts = RebaseOptions {
            onto: self.onto.clone(),
            autostash: self.autostash,
            dry_run: self.dry_run,
            keep_conflicts: self.keep_conflicts,
            // Resolved by the engine. Unlike the daemon, the CLI runs in the
            // user's shell with their own `PATH`, so the well-known-path probe is
            // belt-and-braces here rather than load-bearing.
            git_bin: None,
        };

        // Planning shells out to `git fetch` (once per repo) and walks the object
        // database, so it runs on a blocking thread rather than an async worker.
        let plan_opts = opts.clone();
        let plan =
            tokio::task::spawn_blocking(move || worktree_rebase::plan(&selection, &plan_opts))
                .await
                .context("rebase planning task panicked")??;

        let json = matches!(self.output, TableOrJson::Json);
        // A dry run, or a plan with nothing left to do, reports and stops. The
        // fetch has still happened — that is what pins the snapshot every worktree
        // was measured against.
        if self.dry_run || !plan.has_pending_rebases() {
            self.print(json, &plan.fetches, &plan.worktrees)?;
            return Ok(());
        }

        // Show what is about to happen, then confirm: a rebase rewrites history.
        if !json {
            println!("{}", render_fetches(&plan.fetches));
            println!("{}", render_outcomes(&plan.worktrees));
        }
        let pending = plan.worktrees.iter().filter(|w| is_pending(w)).count();
        if !self.yes && !confirm(pending).await {
            println!("Aborted; no worktree was rebased.");
            return Ok(());
        }

        let fetches = plan.fetches.clone();
        let outcomes = tokio::task::spawn_blocking(move || worktree_rebase::execute(plan, &opts))
            .await
            .context("rebase task panicked")?;
        if !json {
            println!();
        }
        self.print(json, &fetches, &outcomes)
    }

    /// Resolves the CLI's target selection, rejecting an empty one rather than
    /// silently rebasing everything.
    ///
    /// `repo` is the global `-C/--repo` location: it is both the repository
    /// `--all` enumerates and the base that relative `<PATH>` arguments resolve
    /// against, so the command behaves "as if started in `<PATH>`". With no
    /// `-C` the base is `.`, which git resolves against the process CWD.
    fn selection(&self, repo: Option<&Path>) -> Result<Selection> {
        let base = repo.map_or_else(|| PathBuf::from("."), Path::to_path_buf);
        if self.all {
            if !self.paths.is_empty() {
                bail!("pass either <PATH>... or --all, not both");
            }
            return Ok(Selection::All { base });
        }
        if self.paths.is_empty() {
            bail!(
                "specify one or more <PATH> arguments, or --all to rebase \
                 every worktree of this repository"
            );
        }
        let paths = self
            .paths
            .iter()
            .map(|path| {
                if path.is_absolute() {
                    path.clone()
                } else {
                    base.join(path)
                }
            })
            .collect();
        Ok(Selection::Paths(paths))
    }

    /// Prints a report as either pretty JSON or the human table.
    fn print(
        &self,
        json: bool,
        fetches: &[FetchOutcome],
        outcomes: &[WorktreeOutcome],
    ) -> Result<()> {
        if json {
            let value = json!({
                "dry_run": self.dry_run,
                "fetches": fetches,
                "worktrees": outcomes,
            });
            println!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            println!("{}", render_fetches(fetches));
            println!("{}", render_outcomes(outcomes));
        }
        Ok(())
    }
}

/// Whether an outcome is still awaiting a rebase (drives the confirm count).
fn is_pending(outcome: &WorktreeOutcome) -> bool {
    matches!(outcome.result, RebaseResult::WouldRebase { .. })
}

/// Renders the per-repository fetch lines — one per repo, which is the visible
/// proof of the fetch-once-per-repo contract.
fn render_fetches(fetches: &[FetchOutcome]) -> String {
    if fetches.is_empty() {
        return "No repository selected.".to_string();
    }
    fetches
        .iter()
        .map(fetch_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One repository's fetch line.
fn fetch_line(fetch: &FetchOutcome) -> String {
    let root = sanitize(&fetch.repo_root.display().to_string());
    let onto = sanitize(&fetch.onto);
    if !fetch.fetched {
        return format!("Using {onto} in {root} (local ref; nothing fetched)");
    }
    if fetch.ok {
        format!("Fetched {onto} once for {root}")
    } else {
        let detail = brief(fetch.detail.as_deref().unwrap_or(""));
        format!("Fetch of {onto} FAILED for {root}: {detail}")
    }
}

/// Renders the per-worktree result table.
fn render_outcomes(outcomes: &[WorktreeOutcome]) -> String {
    if outcomes.is_empty() {
        return "No worktrees selected.".to_string();
    }
    let mut out = format!(
        "{:<12} {:<24} {:<16} {}",
        "STATUS", "BRANCH", "ONTO", "WORKTREE"
    );
    for outcome in outcomes {
        out.push('\n');
        out.push_str(&outcome_row(outcome));
    }
    out
}

/// One worktree row: status, branch, target ref, path, and a parenthesised detail.
fn outcome_row(outcome: &WorktreeOutcome) -> String {
    let (status, detail) = status_and_detail(&outcome.result);
    let branch = sanitize(outcome.branch.as_deref().unwrap_or("-"));
    let onto = sanitize(&outcome.onto);
    let path = sanitize(&outcome.path.display().to_string());
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!("  ({detail})")
    };
    format!("{status:<12} {branch:<24} {onto:<16} {path}{suffix}")
}

/// The status word and human detail for one outcome.
fn status_and_detail(result: &RebaseResult) -> (&'static str, String) {
    match result {
        RebaseResult::Rebased { behind } => ("rebased", format!("was {behind} behind")),
        RebaseResult::WouldRebase { behind } => ("would-rebase", format!("{behind} behind")),
        RebaseResult::UpToDate => ("up-to-date", String::new()),
        RebaseResult::Skipped { reason } => ("skipped", skip_reason_text(*reason).to_string()),
        // A left-in-place conflict is a *different instruction to the user* than an
        // aborted one — the worktree is still mid-rebase and needs finishing — so
        // the row says so instead of only quoting git's error.
        RebaseResult::Conflict {
            detail,
            left_in_place: true,
        } => (
            "conflict",
            format!(
                "left in place; resolve then `git rebase --continue`: {}",
                brief(detail)
            ),
        ),
        RebaseResult::Conflict { detail, .. } => ("conflict", brief(detail)),
        RebaseResult::FetchFailed { detail } => ("fetch-failed", brief(detail)),
    }
}

/// The human explanation for why a worktree was skipped.
fn skip_reason_text(reason: SkipReason) -> &'static str {
    match reason {
        SkipReason::DetachedHead => "detached HEAD",
        SkipReason::Dirty => "uncommitted changes; pass --autostash",
        SkipReason::OperationInProgress => "a rebase/merge is already in progress",
        SkipReason::NotAWorktree => "not a git worktree",
        SkipReason::NoOntoRef => "could not resolve the target ref",
    }
}

/// A one-line, control-character-free, length-capped summary of a multi-line git
/// error, so a long conflict message cannot wreck the table layout.
fn brief(detail: &str) -> String {
    let first = detail
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let clean = sanitize(first.trim());
    if clean.chars().count() > 100 {
        let truncated: String = clean.chars().take(97).collect();
        format!("{truncated}...")
    } else {
        clean
    }
}

/// Prompts on stderr before rewriting branch history, reading from real stdin.
async fn confirm_rebase(pending: usize) -> bool {
    confirm_rebase_with(pending, read_stdin_line()).await
}

/// Prints the rebase confirmation prompt and resolves the (injected) read into a
/// yes/no decision. Any read error, EOF, or join failure is treated as "no", so a
/// rebase never proceeds unattended.
async fn confirm_rebase_with(
    pending: usize,
    read: impl std::future::Future<Output = Option<String>>,
) -> bool {
    use std::io::Write;
    eprint!("{}", rebase_prompt(pending));
    let _ = std::io::stderr().flush();
    read.await.as_deref().is_some_and(answer_is_yes)
}

/// The confirmation prompt, naming how many worktrees would be rewritten. Pure, so
/// the wording is unit-testable.
fn rebase_prompt(pending: usize) -> String {
    let noun = if pending == 1 {
        "worktree"
    } else {
        "worktrees"
    };
    format!("Rebase {pending} {noun} (this rewrites branch history)? [y/N] ")
}

/// Enqueues eligible worktrees' PRs into the GitHub merge queue — the daemon's
/// two-phase `merge-queue` op driven from the CLI (#1401).
///
/// Only worktrees that pass every eligibility gate (clean, committed, pushed, with
/// an open non-draft, conflict-free, CI-green PR) are enqueued; the rest are
/// reported as skipped-with-reason. Like `close`, the daemon re-validates on
/// execute, so the CLI adds no authority — enqueue authenticates through the
/// user's own `gh`.
#[derive(Parser)]
pub struct MergeQueueCommand {
    /// Worktree folder(s) to consider. Each is canonicalized client-side, as the
    /// daemon runs in a different cwd and matches targets by canonical path.
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<PathBuf>,
    /// Print the eligibility report and exit; never enqueue.
    #[arg(long)]
    pub check: bool,
    /// Skip the interactive confirmation before enqueuing.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl MergeQueueCommand {
    /// Executes the merge-queue command, confirming the enqueue interactively via
    /// stdin.
    pub async fn execute(self) -> Result<()> {
        self.execute_with(confirm_enqueue).await
    }

    /// The merge-queue core, with the confirm decision injected as
    /// `confirm(eligible_count) -> bool`, so the abort and confirmed-execute
    /// branches are unit-testable without driving real stdin (which would block a
    /// test on a TTY); production wires in [`confirm_enqueue`].
    async fn execute_with<F, Fut>(self, confirm: F) -> Result<()>
    where
        F: FnOnce(usize) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        // Canonicalize every path client-side (like `close`/`focus`): the daemon
        // runs in a different cwd and matches targets by canonical path.
        let mut paths = Vec::with_capacity(self.paths.len());
        for p in &self.paths {
            let abs = std::fs::canonicalize(p)
                .with_context(|| format!("cannot resolve worktree path: {}", p.display()))?;
            paths.push(abs.to_string_lossy().to_string());
        }
        let socket = server::resolve_socket(self.socket)?;

        // Phase 1: the side-effect-free eligibility check.
        let report = call(
            &socket,
            "merge-queue",
            json!({ "paths": paths, "check": true }),
        )
        .await?;
        println!("{}", render_eligibility_report(&report));

        if self.check {
            return Ok(());
        }
        let eligible = report
            .get("eligible")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        if eligible == 0 {
            println!("Nothing to enqueue.");
            return Ok(());
        }
        if !self.yes && !confirm(eligible).await {
            println!("Aborted; nothing was enqueued.");
            return Ok(());
        }

        // Phase 2: execute the enqueue (the daemon re-validates eligibility).
        let result = call(
            &socket,
            "merge-queue",
            json!({ "paths": paths, "confirmed": true }),
        )
        .await?;
        println!("{}", render_enqueue_result(&result));
        Ok(())
    }
}

/// Moves and resizes worktrees' open VS Code windows to match a reference
/// window's geometry (#1407).
///
/// The CLI counterpart of the tree view's "Reposition Windows to Match", and the
/// diagnostic surface for it: `--dry-run` reports exactly which OS window each
/// worktree resolved to without touching anything, which is how a title-matching
/// problem is diagnosed.
///
/// Paths, not window keys, are the CLI's currency (as for `focus`/`close`), so a
/// `list` call up front maps each folder to the key of the window that has it
/// open. The daemon does all the OS work — it holds the macOS Accessibility grant,
/// which a terminal-launched process would not.
#[derive(Parser)]
pub struct RepositionCommand {
    /// Worktree folders whose windows to move. Each is canonicalized client-side,
    /// as the daemon runs in a different cwd and matches by canonical path.
    #[arg(value_name = "PATH")]
    pub paths: Vec<PathBuf>,
    /// The worktree whose window supplies the target position and size. It is
    /// never itself moved.
    #[arg(
        long,
        value_name = "PATH",
        required_unless_present = "undo",
        conflicts_with = "undo"
    )]
    pub reference: Option<PathBuf>,
    /// Report which window each worktree resolves to and stop; move nothing.
    #[arg(long, conflicts_with = "undo")]
    pub dry_run: bool,
    /// Put the windows the last reposition moved back where they were.
    #[arg(long)]
    pub undo: bool,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl RepositionCommand {
    /// Executes the reposition command.
    pub async fn execute(self) -> Result<()> {
        let output = self.output;
        let socket = server::resolve_socket(self.socket)?;
        if self.undo {
            let reply = call(&socket, "reposition-undo", Value::Null).await?;
            return print_reposition(output, &reply);
        }
        // `required_unless_present` guarantees this on the non-undo path.
        let Some(reference) = self.reference.as_deref() else {
            bail!("`reposition` requires `--reference <PATH>`");
        };

        // Resolve folders to the keys of the windows that have them open. The op
        // addresses *windows*, since geometry belongs to the OS window rather than
        // to the worktree, and only the registry knows which window that is.
        let windows = call(&socket, "list", Value::Null).await?;
        let reference_key = window_key_for(&windows, reference, "repositioned")?;
        let mut target_keys = Vec::with_capacity(self.paths.len());
        for path in &self.paths {
            target_keys.push(window_key_for(&windows, path, "repositioned")?);
        }

        let reply = call(
            &socket,
            "reposition",
            json!({
                "reference_key": reference_key,
                "target_keys": target_keys,
                "check": self.dry_run,
            }),
        )
        .await?;
        print_reposition(output, &reply)
    }
}

/// Prints a `reposition` / `reposition-undo` reply in the requested format.
fn print_reposition(output: TableOrJson, reply: &Value) -> Result<()> {
    match output {
        TableOrJson::Json => println!("{}", serde_json::to_string_pretty(reply)?),
        TableOrJson::Table => println!("{}", render_reposition(reply)),
    }
    Ok(())
}

/// Signals worktrees' open VS Code windows to reload themselves (#1417).
///
/// The CLI counterpart of the tree view's "Reload Window" — the batch form of
/// `Developer: Reload Window`, which otherwise has to be run by hand in each
/// window in turn.
///
/// Addresses windows by key like `reposition`, so a `list` call up front maps
/// each folder to the key of the window that has it open. Unlike the tree view,
/// a worktree with **no** open window is an error rather than a silent skip:
/// there the selection is a sweep, here the user named each target explicitly.
///
/// The daemon only marks a directive per target; each window acts on it on its
/// next heartbeat, up to ~10s later. Nothing here waits for that, which is why
/// the output says how many windows were *signalled*.
#[derive(Parser)]
pub struct ReloadCommand {
    /// Worktree folders whose windows to reload. Each is canonicalized
    /// client-side, as the daemon runs in a different cwd and matches by
    /// canonical path.
    #[arg(value_name = "PATH", required = true)]
    pub paths: Vec<PathBuf>,
    /// Output format.
    #[arg(short = 'o', long, value_enum, default_value_t = TableOrJson::Table)]
    pub output: TableOrJson,
    /// Control-socket path. Defaults to the per-user runtime location.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl ReloadCommand {
    /// Executes the reload command.
    pub async fn execute(self) -> Result<()> {
        let socket = server::resolve_socket(self.socket)?;
        let windows = call(&socket, "list", Value::Null).await?;
        let mut target_keys = Vec::with_capacity(self.paths.len());
        for path in &self.paths {
            target_keys.push(window_key_for(&windows, path, "reloaded")?);
        }

        let reply = call(&socket, "reload", json!({ "target_keys": target_keys })).await?;
        match self.output {
            TableOrJson::Json => println!("{}", serde_json::to_string_pretty(&reply)?),
            TableOrJson::Table => println!("{}", render_reload(&reply)),
        }
        Ok(())
    }
}

/// Renders a `reload` reply as a one-line human summary.
///
/// Says "Signalled", never "Reloaded": the directive rides each window's ~10s
/// heartbeat, so when this prints nothing has reloaded yet — and the daemon
/// could not observe it if it had, since the window re-registers under the same
/// key. Any key the daemon had no live window for is named rather than dropped.
fn render_reload(reply: &Value) -> String {
    let requested = reply.get("requested").and_then(Value::as_u64).unwrap_or(0);
    let signalled = reply.get("signalled").and_then(Value::as_u64).unwrap_or(0);
    let unknown: Vec<String> = reply
        .get("unknown")
        .and_then(Value::as_array)
        .map(|keys| {
            keys.iter()
                .filter_map(Value::as_str)
                .map(sanitize)
                .collect()
        })
        .unwrap_or_default();

    // The noun agrees with `requested`, the number it sits next to: "1 of 3
    // windows", not "1 of 3 window".
    let noun = if requested == 1 { "window" } else { "windows" };
    let mut out = format!("Signalled {signalled} of {requested} {noun} to reload.");
    if !unknown.is_empty() {
        // A window that closed between the `list` above and the op landing. Rare
        // but real, and never worth swallowing.
        out.push_str(&format!(
            "\nNo longer open, so not signalled: {}",
            unknown.join(", ")
        ));
    }
    out
}

/// Finds the registry key of the window that has `path` open.
///
/// Canonicalizes client-side and compares against each window's canonicalized
/// folders, the same convention `close`/`focus` use. A worktree with no open
/// window is an error rather than a silent skip: the CLI names its targets one by
/// one, so an unmatched one is a mistake worth reporting, unlike a multi-select in
/// the tree view where a stale row is expected.
///
/// `verb` is the past participle of the caller's action ("repositioned",
/// "reloaded"), so the error names what the user was actually trying to do.
fn window_key_for(windows: &Value, path: &Path, verb: &str) -> Result<String> {
    let wanted = std::fs::canonicalize(path)
        .with_context(|| format!("cannot resolve worktree path: {}", path.display()))?;
    windows
        .get("windows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .find(|window| {
            window
                .get("folders")
                .and_then(Value::as_array)
                .is_some_and(|folders| {
                    folders.iter().filter_map(Value::as_str).any(|folder| {
                        std::fs::canonicalize(folder).is_ok_and(|folder| folder == wanted)
                    })
                })
        })
        .and_then(|window| window.get("key").and_then(Value::as_str))
        .map(ToString::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no VS Code window has {} open (only open windows can be {verb})",
                wanted.display()
            )
        })
}

/// Renders a `reposition` / `reposition-undo` reply as a human-readable report:
/// the permission state, the reference geometry, a summary count, and one line per
/// target with its outcome.
fn render_reposition(reply: &Value) -> String {
    if reply.get("trusted").and_then(Value::as_bool) == Some(false) {
        return "omni-dev does not hold the macOS Accessibility permission, so no window \
                was touched.\nGrant it in System Settings → Privacy & Security → \
                Accessibility (add the omni-dev binary), then run `omni-dev daemon restart`."
            .to_string();
    }
    if let Some(blocked) = reply.get("blocked") {
        let reason = sanitize(blocked.get("reason").and_then(Value::as_str).unwrap_or("-"));
        let detail = sanitize(blocked.get("detail").and_then(Value::as_str).unwrap_or(""));
        return format!("Nothing was moved [{reason}]: {detail}");
    }

    let moved = reply.get("moved").and_then(Value::as_u64).unwrap_or(0);
    let skipped = reply.get("skipped").and_then(Value::as_u64).unwrap_or(0);
    let mut out = String::new();
    if let Some(reference) = reply.get("reference") {
        let title = sanitize(
            reference
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("-"),
        );
        out.push_str(&format!(
            "Reference: {title} {}\n",
            render_frame(reference.get("frame"))
        ));
    }
    out.push_str(&format!("Moved: {moved} / Skipped: {skipped}"));
    let results = reply
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    for result in results {
        let outcome = sanitize(result.get("outcome").and_then(Value::as_str).unwrap_or("-"));
        let title = sanitize(
            result
                .get("title")
                .and_then(Value::as_str)
                .or_else(|| result.get("key").and_then(Value::as_str))
                .unwrap_or("-"),
        );
        let detail = sanitize(result.get("detail").and_then(Value::as_str).unwrap_or(""));
        out.push_str(&format!("\n  {outcome}: {title} — {detail}"));
    }
    if results.is_empty() {
        out.push_str("\n  (nothing to report)");
    }
    out
}

/// Renders a frame as `WxH at (X, Y)`, or `-` when absent.
fn render_frame(frame: Option<&Value>) -> String {
    let Some(frame) = frame else {
        return "-".to_string();
    };
    let field = |name: &str| {
        frame
            .get(name)
            .and_then(Value::as_f64)
            .unwrap_or(0.0)
            .round()
    };
    format!(
        "{}×{} at ({}, {})",
        field("width"),
        field("height"),
        field("x"),
        field("y")
    )
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
    // Named `repo_name`, not `repo`, and so spelled `--repo-name`: clap
    // propagates a `global = true` arg by **arg id**, and the derive's id is the
    // field name. A local `repo` id therefore displaced the global `-C/--repo`
    // under this subcommand and its `String` was copied back into the root
    // matches, panicking `Cli`'s `PathBuf` read (#1420). Renaming only the long
    // spelling would not have been enough. The wire key stays `repo`.
    #[arg(long, value_name = "REPO")]
    pub repo_name: Option<String>,
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
            "repo": self.repo_name,
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
/// window to re-register after a daemon restart) and, when present, the
/// cross-window directives `close` and `reload`. Both are omitted from the reply
/// when nothing is pending, and both read as `false` here when absent.
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
        // Both directives are omitted from the reply when false; treat absent as
        // false, which is also what a pre-#1417 daemon's reply reads as.
        let close = reply.get("close").and_then(Value::as_bool).unwrap_or(false);
        let reload = reply
            .get("reload")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        println!("known: {known}");
        println!("close: {close}");
        println!("reload: {reload}");
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

/// Renders a `merge-queue` phase-1 `EligibilityReport`: a summary count, then one
/// line per enqueue-eligible worktree (`PR #N [branch] path`) and per
/// skipped-with-reason worktree (`[kind] path — detail`). Every daemon-supplied
/// string is `sanitize`d (#1137); the counts are daemon-computed and safe.
fn render_eligibility_report(report: &Value) -> String {
    let eligible = report
        .get("eligible")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let skipped = report
        .get("skipped")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let mut out = format!("Eligible: {} / Skipped: {}", eligible.len(), skipped.len());
    for pr in eligible {
        let number = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
        let branch = sanitize(pr.get("branch").and_then(Value::as_str).unwrap_or("-"));
        let path = sanitize(pr.get("path").and_then(Value::as_str).unwrap_or(""));
        out.push_str(&format!("\n  eligible: PR #{number} [{branch}] {path}"));
    }
    for skip in skipped {
        let kind = sanitize(skip.get("kind").and_then(Value::as_str).unwrap_or("-"));
        let detail = sanitize(skip.get("detail").and_then(Value::as_str).unwrap_or(""));
        let path = sanitize(skip.get("path").and_then(Value::as_str).unwrap_or(""));
        out.push_str(&format!("\n  skipped [{kind}]: {path} — {detail}"));
    }
    out
}

/// Renders a `merge-queue` phase-2 `EnqueueResult`: a summary count, then one line
/// per queued PR (`PR #N`, with `(already queued)` for an idempotent no-op) and per
/// failed PR (`PR #N — error`). Skips (a worktree that became ineligible between
/// phases) are folded into the summary count. Strings are `sanitize`d (#1137).
fn render_enqueue_result(result: &Value) -> String {
    let queued = result
        .get("queued")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let failed = result
        .get("failed")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let skipped = result
        .get("skipped")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut out = format!(
        "Queued: {} / Failed: {} / Skipped: {}",
        queued.len(),
        failed.len(),
        skipped
    );
    for pr in queued {
        let number = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
        let already = pr
            .get("already_queued")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let suffix = if already { " (already queued)" } else { "" };
        out.push_str(&format!("\n  queued: PR #{number}{suffix}"));
    }
    for pr in failed {
        let number = pr.get("number").and_then(Value::as_u64).unwrap_or(0);
        let error = sanitize(pr.get("error").and_then(Value::as_str).unwrap_or(""));
        out.push_str(&format!("\n  failed: PR #{number} — {error}"));
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

/// Prompts on stderr before enqueuing and returns whether the user assented,
/// reading the answer from real stdin. A thin wrapper over [`confirm_enqueue_with`]
/// supplying the live stdin reader.
async fn confirm_enqueue(count: usize) -> bool {
    confirm_enqueue_with(count, read_stdin_line()).await
}

/// Prints the enqueue confirmation prompt and resolves the (already-injected) read
/// of the user's answer into a yes/no decision. A read error, closed stdin (EOF),
/// or join failure is treated as "no", so an enqueue never proceeds unattended.
async fn confirm_enqueue_with(
    count: usize,
    read: impl std::future::Future<Output = Option<String>>,
) -> bool {
    use std::io::Write;
    eprint!("Add {count} PR(s) to the merge queue? [y/N] ");
    let _ = std::io::stderr().flush();
    read.await.as_deref().is_some_and(answer_is_yes)
}

/// Reads one line from stdin on a dedicated thread (`spawn_blocking`) so it never
/// stalls an async worker while it waits for input. Returns `None` on any read
/// error, EOF, or join failure.
async fn read_stdin_line() -> Option<String> {
    tokio::task::spawn_blocking(|| read_line_from(&mut std::io::stdin().lock()))
        .await
        .ok()
        .flatten()
}

/// Reads one line from `reader`, mapping EOF and read errors to the same
/// `Option<String>` the stdin caller consumes. Split out of [`read_stdin_line`]
/// so the read logic is testable with an in-memory reader — real stdin can't be
/// driven from a test without blocking on a TTY.
fn read_line_from(reader: &mut impl std::io::BufRead) -> Option<String> {
    let mut answer = String::new();
    reader.read_line(&mut answer).ok().map(|_| answer)
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
        cmd.execute(None).await.unwrap();
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

    /// A [`fake_daemon_reply`] that answers a **sequence** of requests — one fresh
    /// connection per reply, in order — so a two-phase client (a `merge-queue`
    /// check then execute) can be driven end-to-end over one socket.
    fn fake_daemon_replies(
        replies: Vec<Value>,
    ) -> (tempfile::TempDir, PathBuf, tokio::task::JoinHandle<()>) {
        use futures::{SinkExt, StreamExt};
        use tokio::net::UnixListener;
        use tokio_util::codec::{Framed, LinesCodec};

        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let sock = dir.path().join("d.sock");
        let listener = UnixListener::bind(&sock).unwrap();
        let server = tokio::spawn(async move {
            for reply in replies {
                let (stream, _) = listener.accept().await.unwrap();
                let mut framed = Framed::new(stream, LinesCodec::new());
                let _req = framed.next().await.unwrap().unwrap();
                framed
                    .send(serde_json::to_string(&reply).unwrap())
                    .await
                    .unwrap();
            }
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
            "register",
            "--key",
            "w1",
            "--folder",
            "/a",
            "--folder",
            "/b",
            "--repo-name",
            "r",
            "--pid",
            "42",
        ])
        .unwrap();
        assert_eq!(cmd.key, "w1");
        assert_eq!(cmd.folders, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(cmd.repo_name.as_deref(), Some("r"));
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
        // The risky wording names the risks; the clean one does not. Both default
        // to No. (No failure-message args — the conditions are self-describing, and
        // an unevaluated arg would just read as an uncovered line.)
        assert!(confirm_prompt(true).contains("risks"));
        assert!(!confirm_prompt(false).contains("risks"));
        assert!(confirm_prompt(true).contains("[y/N]"));
        assert!(confirm_prompt(false).contains("[y/N]"));
    }

    #[test]
    fn read_line_from_maps_input_and_eof() {
        use std::io::Cursor;
        // A line (with or without a trailing newline) comes back verbatim; EOF is
        // an empty read (`Ok(0)`), which maps to `Some("")` — the decision layer
        // then treats it as "no".
        assert_eq!(
            read_line_from(&mut Cursor::new("y\n")).as_deref(),
            Some("y\n")
        );
        assert_eq!(read_line_from(&mut Cursor::new("")).as_deref(), Some(""));
        assert_eq!(
            read_line_from(&mut Cursor::new("no-newline")).as_deref(),
            Some("no-newline")
        );
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
            repo_name: Some("r".to_string()),
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
        .execute(None)
        .await
        .unwrap();

        // Rebase: a non-worktree path with `--dry-run` reaches no daemon and no
        // remote (it classifies as `not a git worktree` and stops).
        WorktreesCommand {
            command: WorktreesSubcommands::Rebase(RebaseCommand {
                paths: vec![target.path().to_path_buf()],
                dry_run: true,
                ..rebase_cmd()
            }),
        }
        .execute(None)
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
        .execute(None)
        .await
        .unwrap();
        server.await.unwrap();

        // Reposition: `--undo` needs no path resolution, so one reply suffices.
        let (_d, sock, server) = fake_daemon_seq(vec![json!({
            "ok": true,
            "payload": { "trusted": true, "moved": 0, "skipped": 0, "results": [] },
        })]);
        WorktreesCommand {
            command: WorktreesSubcommands::Reposition(RepositionCommand {
                paths: Vec::new(),
                reference: None,
                dry_run: false,
                undo: true,
                output: TableOrJson::Table,
                socket: Some(sock),
            }),
        }
        .execute(None)
        .await
        .unwrap();
        server.await.unwrap();

        // Reload: an empty path list resolves nothing, so `list` is the only
        // request before the op.
        let (_d, sock, server) = fake_daemon_seq(vec![
            json!({ "ok": true, "payload": { "windows": [] } }),
            json!({ "ok": true, "payload": { "requested": 0, "signalled": 0, "unknown": [] } }),
        ]);
        WorktreesCommand {
            command: WorktreesSubcommands::Reload(ReloadCommand {
                paths: Vec::new(),
                output: TableOrJson::Table,
                socket: Some(sock),
            }),
        }
        .execute(None)
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
                repo_name: None,
                title: None,
                pid: None,
                socket: Some(sock),
            }),
        }
        .execute(None)
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
        .execute(None)
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
        .execute(None)
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

    // ── worktrees rebase (#1400) ──────────────────────────────────────────

    /// A `RebaseCommand` with every field defaulted, for terse test construction.
    fn rebase_cmd() -> RebaseCommand {
        RebaseCommand {
            paths: Vec::new(),
            all: false,
            onto: None,
            autostash: false,
            dry_run: false,
            keep_conflicts: false,
            yes: false,
            output: TableOrJson::Table,
        }
    }

    #[test]
    fn rebase_parses_paths_and_flags() {
        let cmd = RebaseCommand::try_parse_from([
            "rebase",
            "/wt/a",
            "/wt/b",
            "--onto",
            "origin/release",
            "--autostash",
            "--dry-run",
            "--keep-conflicts",
            "-y",
            "-o",
            "json",
        ])
        .unwrap();
        assert_eq!(
            cmd.paths,
            vec![PathBuf::from("/wt/a"), PathBuf::from("/wt/b")]
        );
        assert_eq!(cmd.onto.as_deref(), Some("origin/release"));
        assert!(cmd.autostash && cmd.dry_run && cmd.keep_conflicts && cmd.yes);
        assert!(matches!(cmd.output, TableOrJson::Json));
    }

    #[test]
    fn rebase_defaults_are_conservative() {
        let cmd = RebaseCommand::try_parse_from(["rebase", "/wt/a"]).unwrap();
        assert!(!cmd.all && !cmd.autostash && !cmd.dry_run && !cmd.yes);
        // Aborting a conflict stays the default: the opt-in is `--keep-conflicts`,
        // so an unattended batch never leaves a worktree mid-rebase by surprise.
        assert!(!cmd.keep_conflicts);
        assert_eq!(cmd.onto, None);
        assert!(matches!(cmd.output, TableOrJson::Table));
    }

    #[test]
    fn rebase_requires_a_target() {
        // A bare `rebase` parses, but resolving its selection refuses rather than
        // silently rebasing everything.
        let err = rebase_cmd().selection(None).unwrap_err().to_string();
        assert!(err.contains("--all"), "expected a usage hint, got: {err}");
    }

    #[test]
    fn rebase_rejects_paths_together_with_all() {
        let cmd = RebaseCommand {
            paths: vec![PathBuf::from("/wt/a")],
            all: true,
            ..rebase_cmd()
        };
        let err = cmd.selection(None).unwrap_err().to_string();
        assert!(err.contains("not both"), "got: {err}");
    }

    #[test]
    fn rebase_selection_maps_paths_and_all() {
        let cmd = RebaseCommand {
            paths: vec![PathBuf::from("/wt/a")],
            ..rebase_cmd()
        };
        assert!(matches!(cmd.selection(None).unwrap(), Selection::Paths(p) if p.len() == 1));
        let all = RebaseCommand {
            all: true,
            ..rebase_cmd()
        };
        assert!(matches!(
            all.selection(None).unwrap(),
            Selection::All { .. }
        ));
    }

    #[test]
    fn rebase_prompt_agrees_in_number() {
        assert!(rebase_prompt(1).contains("1 worktree ("));
        assert!(rebase_prompt(3).contains("3 worktrees ("));
        // Always names the consequence.
        assert!(rebase_prompt(2).contains("rewrites branch history"));
    }

    #[tokio::test]
    async fn confirm_rebase_with_decides_from_the_answer() {
        assert!(confirm_rebase_with(1, async { Some("y\n".to_string()) }).await);
        assert!(confirm_rebase_with(2, async { Some("YES".to_string()) }).await);
        assert!(!confirm_rebase_with(1, async { Some("n".to_string()) }).await);
        assert!(!confirm_rebase_with(1, async { Some(String::new()) }).await);
        assert!(!confirm_rebase_with(1, async { None }).await);
    }

    #[test]
    fn fetch_line_reports_each_repos_single_fetch() {
        let ok = FetchOutcome {
            repo_root: PathBuf::from("/repo"),
            onto: "origin/main".to_string(),
            fetched: true,
            ok: true,
            detail: None,
        };
        assert!(fetch_line(&ok).contains("Fetched origin/main once for /repo"));

        let failed = FetchOutcome {
            detail: Some("host unreachable".to_string()),
            ok: false,
            ..ok.clone()
        };
        assert!(fetch_line(&failed).contains("FAILED"));

        let local = FetchOutcome {
            fetched: false,
            onto: "develop".to_string(),
            ..ok
        };
        assert!(fetch_line(&local).contains("nothing fetched"));
    }

    #[test]
    fn outcome_rows_render_each_status() {
        let row = |result| {
            outcome_row(&WorktreeOutcome {
                path: PathBuf::from("/wt"),
                branch: Some("feature".to_string()),
                onto: "origin/main".to_string(),
                result,
            })
        };
        assert!(row(RebaseResult::Rebased { behind: 2 }).contains("rebased"));
        assert!(row(RebaseResult::Rebased { behind: 2 }).contains("was 2 behind"));
        assert!(row(RebaseResult::WouldRebase { behind: 1 }).contains("would-rebase"));
        assert!(row(RebaseResult::UpToDate).contains("up-to-date"));
        assert!(row(RebaseResult::Skipped {
            reason: SkipReason::Dirty
        })
        .contains("--autostash"));
        assert!(row(RebaseResult::Conflict {
            detail: "CONFLICT (content)".to_string(),
            left_in_place: false,
        })
        .contains("conflict"));
        // A left-in-place conflict tells the user the worktree still needs
        // finishing — a different instruction than an aborted one (#1415).
        let kept = row(RebaseResult::Conflict {
            detail: "CONFLICT (content)".to_string(),
            left_in_place: true,
        });
        assert!(kept.contains("conflict"), "{kept}");
        assert!(kept.contains("git rebase --continue"), "{kept}");
        assert!(row(RebaseResult::FetchFailed {
            detail: "host unreachable".to_string()
        })
        .contains("fetch-failed"));
        // The remaining skip reasons render their human text.
        assert!(row(RebaseResult::Skipped {
            reason: SkipReason::DetachedHead
        })
        .contains("detached HEAD"));
        assert!(row(RebaseResult::Skipped {
            reason: SkipReason::OperationInProgress
        })
        .contains("in progress"));
        assert!(row(RebaseResult::Skipped {
            reason: SkipReason::NotAWorktree
        })
        .contains("not a git worktree"));
        assert!(row(RebaseResult::Skipped {
            reason: SkipReason::NoOntoRef
        })
        .contains("resolve the target ref"));
    }

    #[test]
    fn print_emits_both_json_and_table_without_error() {
        let fetches = vec![FetchOutcome {
            repo_root: PathBuf::from("/r"),
            onto: "origin/main".to_string(),
            fetched: true,
            ok: true,
            detail: None,
        }];
        let outcomes = vec![WorktreeOutcome {
            path: PathBuf::from("/wt"),
            branch: Some("feature".to_string()),
            onto: "origin/main".to_string(),
            result: RebaseResult::UpToDate,
        }];
        // The JSON branch (serializes the whole report) and the table branch.
        let json_cmd = RebaseCommand {
            dry_run: true,
            output: TableOrJson::Json,
            ..rebase_cmd()
        };
        json_cmd.print(true, &fetches, &outcomes).unwrap();
        rebase_cmd().print(false, &fetches, &outcomes).unwrap();
    }

    #[test]
    fn brief_collapses_a_multiline_git_error_to_one_capped_line() {
        assert_eq!(brief("\n\nfirst line\nsecond line\n"), "first line");
        let long = "x".repeat(200);
        let out = brief(&long);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with("..."));
        // Control characters are stripped (the table is untrusted-string safe).
        assert_eq!(brief("a\u{7}b"), "ab");
    }

    #[test]
    fn empty_report_renders_placeholders() {
        assert_eq!(render_fetches(&[]), "No repository selected.");
        assert_eq!(render_outcomes(&[]), "No worktrees selected.");
    }

    // The serialization guard is a `std::sync::Mutex` held across the `.await`
    // below on purpose: the git fetch it serializes runs *during* that await (on a
    // `spawn_blocking` thread inside `execute_with`). It is deadlock-safe — the
    // awaited work never re-acquires this lock — so the general "no std mutex
    // across await" rule does not apply to this test-only load limiter.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn rebase_declined_confirmation_leaves_the_branch_untouched() {
        // The safety-critical branch of a history-rewriting command: declining the
        // prompt must return before `worktree_rebase::execute` is ever reached.
        // Shares the engine tests' git-load lock so the whole suite's concurrent
        // `git` spawns never starve the daemon's timing-sensitive poller tests.
        let _guard = crate::git::worktree_batch::test_serial_lock();
        let Some(scenario) = BehindScenario::build() else {
            return; // git unavailable — the engine tests cover the git behaviour.
        };
        let before = scenario.worktree_head();
        RebaseCommand {
            paths: vec![scenario.worktree.clone()],
            ..rebase_cmd()
        }
        .execute_with(None, |pending| async move {
            assert_eq!(pending, 1, "one worktree is behind and awaiting a rebase");
            false
        })
        .await
        .unwrap();
        assert_eq!(
            scenario.worktree_head(),
            before,
            "declining the confirm must not rebase"
        );
    }

    // Holds the git-load lock across `.await` for the same deadlock-safe reason as
    // the declined-confirm test above.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn rebase_confirmed_rebases_the_behind_worktree() {
        // The accepted branch: confirming drives plan → execute → report, and the
        // behind worktree fast-forwards onto the freshly fetched origin/main.
        let _guard = crate::git::worktree_batch::test_serial_lock();
        let Some(scenario) = BehindScenario::build() else {
            return; // git unavailable — the engine tests cover the git behaviour.
        };
        let before = scenario.worktree_head();
        RebaseCommand {
            paths: vec![scenario.worktree.clone()],
            ..rebase_cmd()
        }
        .execute_with(None, |pending| async move {
            assert_eq!(pending, 1);
            true
        })
        .await
        .unwrap();
        assert_ne!(
            scenario.worktree_head(),
            before,
            "confirming the prompt must rebase the worktree"
        );
    }

    /// A repo whose one linked worktree is a commit behind `origin/main`, built with
    /// the real `git` CLI (the command under test shells out too). Returns `None` if
    /// any setup step fails, so the suite degrades rather than flaking.
    struct BehindScenario {
        _root: tempfile::TempDir,
        worktree: PathBuf,
    }

    impl BehindScenario {
        fn build() -> Option<Self> {
            use git2::Repository;
            let root = tempfile::tempdir().ok()?;
            let origin = root.path().join("origin.git");
            let local = root.path().join("local");
            let worktree = root.path().join("feature");
            std::fs::create_dir_all(&origin).ok()?;
            std::fs::create_dir_all(&local).ok()?;
            run(&origin, &["init", "--bare", "-b", "main"])?;
            run(&local, &["init", "-b", "main"])?;
            Self::identity(&local)?;
            std::fs::write(local.join("f.txt"), "one\n").ok()?;
            run(&local, &["add", "f.txt"])?;
            run(&local, &["commit", "-m", "one"])?;
            run(&local, &["remote", "add", "origin", origin.to_str()?])?;
            run(&local, &["push", "-u", "origin", "main"])?;
            run(
                &local,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feature",
                    worktree.to_str()?,
                    "main",
                ],
            )?;
            // Advance origin/main in-process with git2 (no `git clone` subprocess),
            // so `local` only learns of it when the command under test fetches.
            let repo = Repository::open_bare(&origin).ok()?;
            let parent = repo
                .find_commit(repo.refname_to_id("refs/heads/main").ok()?)
                .ok()?;
            let mut builder = repo.treebuilder(Some(&parent.tree().ok()?)).ok()?;
            let blob = repo.blob(b"two\n").ok()?;
            builder.insert("f.txt", blob, 0o100_644).ok()?;
            let tree = repo.find_tree(builder.write().ok()?).ok()?;
            let sig = git2::Signature::now("Other", "other@example.com").ok()?;
            repo.commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "two",
                &tree,
                &[&parent],
            )
            .ok()?;
            Some(Self {
                _root: root,
                worktree,
            })
        }

        /// Pins identity and disables commit signing, so a developer's global
        /// `commit.gpgsign = true` cannot make these repos depend on gpg.
        fn identity(dir: &Path) -> Option<()> {
            run(dir, &["config", "user.name", "Test"])?;
            run(dir, &["config", "user.email", "test@example.com"])?;
            run(dir, &["config", "commit.gpgsign", "false"])
        }

        fn worktree_head(&self) -> String {
            let out = std::process::Command::new("git")
                .current_dir(&self.worktree)
                .args(["rev-parse", "HEAD"])
                .output();
            out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .unwrap_or_default()
        }
    }

    /// Runs `git` in `dir`, returning `None` on any failure.
    fn run(dir: &Path, args: &[&str]) -> Option<()> {
        let output = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .ok()?;
        output.status.success().then_some(())
    }

    // --- Merge-queue command (#1401) ---------------------------------------

    #[test]
    fn merge_queue_parses_paths_and_flags() {
        // Routing: `worktrees merge-queue` maps to the MergeQueue variant.
        assert!(matches!(
            parse(&["merge-queue", "/a"]),
            WorktreesSubcommands::MergeQueue(_)
        ));
        // Multiple positional paths plus `--check`.
        let cmd =
            MergeQueueCommand::try_parse_from(["merge-queue", "/a", "/b", "--check"]).unwrap();
        assert_eq!(cmd.paths.len(), 2);
        assert!(cmd.check);
        assert!(!cmd.yes);
        assert!(cmd.socket.is_none());
        // `-y` and `--socket`.
        let cmd = MergeQueueCommand::try_parse_from([
            "merge-queue",
            "/a",
            "-y",
            "--socket",
            "/tmp/d.sock",
        ])
        .unwrap();
        assert!(cmd.yes);
        assert_eq!(cmd.socket.as_deref(), Some(Path::new("/tmp/d.sock")));
        // At least one path is required.
        assert!(MergeQueueCommand::try_parse_from(["merge-queue"]).is_err());
    }

    #[test]
    fn render_eligibility_report_lists_eligible_and_skipped() {
        let report = json!({
            "eligible": [{ "number": 10, "branch": "feature", "url": "u", "path": "/wt/a" }],
            "skipped": [{ "path": "/wt/b", "kind": "dirty", "detail": "2 modified" }],
        });
        let out = render_eligibility_report(&report);
        assert!(out.contains("Eligible: 1 / Skipped: 1"), "{out}");
        assert!(out.contains("PR #10 [feature] /wt/a"), "{out}");
        assert!(out.contains("skipped [dirty]: /wt/b — 2 modified"), "{out}");
    }

    #[test]
    fn render_enqueue_result_marks_already_queued_and_failures() {
        let result = json!({
            "queued": [
                { "number": 10, "path": "/a" },
                { "number": 11, "path": "/b", "already_queued": true },
            ],
            "failed": [{ "number": 12, "path": "/c", "error": "merge queue not enabled" }],
            "skipped": [{ "path": "/d", "kind": "unpushed", "detail": "x" }],
        });
        let out = render_enqueue_result(&result);
        assert!(out.contains("Queued: 2 / Failed: 1 / Skipped: 1"), "{out}");
        assert!(out.contains("queued: PR #10"), "{out}");
        assert!(out.contains("PR #11 (already queued)"), "{out}");
        assert!(
            out.contains("failed: PR #12 — merge queue not enabled"),
            "{out}"
        );
    }

    #[test]
    fn render_eligibility_report_strips_control_bytes() {
        // Control bytes in every daemon-supplied string must not reach the terminal
        // (#1137), matching the close/list/tree renderers.
        let report = json!({
            "eligible": [{ "number": 1, "branch": "br\x1b[31manch", "path": "/a\rb" }],
            "skipped": [{ "path": "/e\x1b]0;x\x07vil", "kind": "d\x07irty", "detail": "l\u{9b}2J" }],
        });
        let out = render_eligibility_report(&report);
        assert!(
            !out.contains(|c: char| c.is_control() && c != '\n'),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn confirm_enqueue_with_decides_from_the_answer() {
        assert!(confirm_enqueue_with(3, async { Some("y\n".to_string()) }).await);
        assert!(confirm_enqueue_with(1, async { Some("YES".to_string()) }).await);
        assert!(!confirm_enqueue_with(3, async { Some("n".to_string()) }).await);
        assert!(!confirm_enqueue_with(3, async { Some(String::new()) }).await);
        assert!(!confirm_enqueue_with(3, async { None }).await);
    }

    #[tokio::test]
    async fn merge_queue_errors_on_a_nonexistent_path_before_any_socket_call() {
        // Canonicalization fails for a path that does not exist, so the command
        // reports a clear error without needing a daemon. Also drives `execute`.
        let cmd = MergeQueueCommand {
            paths: vec![PathBuf::from("/nonexistent/omni-dev-mq-xyz")],
            check: true,
            yes: false,
            socket: Some(PathBuf::from("/nonexistent/omni-dev-mq.sock")),
        };
        let err = cmd.execute().await.unwrap_err();
        assert!(
            err.to_string().contains("cannot resolve worktree path"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn merge_queue_check_prints_the_report_and_never_confirms() {
        let target = tempfile::tempdir().unwrap();
        let (_dir, sock, server) = fake_daemon_replies(vec![json!({
            "ok": true,
            "payload": {
                "eligible": [{ "path": "/a", "number": 9, "url": "u", "branch": "feature" }],
                "skipped": [{ "path": "/b", "kind": "dirty", "detail": "2 modified" }],
            }
        })]);
        let cmd = MergeQueueCommand {
            paths: vec![target.path().to_path_buf()],
            check: true,
            yes: false,
            socket: Some(sock),
        };
        // `--check` returns after phase 1, so the confirm closure must never run.
        cmd.execute_with(|_| async { panic!("must not confirm on --check") })
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn merge_queue_reports_nothing_to_enqueue_when_none_eligible() {
        let target = tempfile::tempdir().unwrap();
        let (_dir, sock, server) = fake_daemon_replies(vec![json!({
            "ok": true,
            "payload": {
                "eligible": [],
                "skipped": [{ "path": "/b", "kind": "no-pr", "detail": "no open PR" }],
            }
        })]);
        let cmd = MergeQueueCommand {
            paths: vec![target.path().to_path_buf()],
            check: false,
            yes: false,
            socket: Some(sock),
        };
        // Nothing eligible → no confirm, no phase-2 call.
        cmd.execute_with(|_| async { panic!("must not confirm when nothing is eligible") })
            .await
            .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn merge_queue_aborts_when_confirmation_is_declined() {
        let target = tempfile::tempdir().unwrap();
        let (_dir, sock, server) = fake_daemon_replies(vec![json!({
            "ok": true,
            "payload": {
                "eligible": [{ "path": "/a", "number": 9, "url": "u", "branch": "feature" }],
                "skipped": [],
            }
        })]);
        let cmd = MergeQueueCommand {
            paths: vec![target.path().to_path_buf()],
            check: false,
            yes: false,
            socket: Some(sock),
        };
        // Declining aborts before the phase-2 call, so only one reply is consumed.
        cmd.execute_with(|count| async move {
            assert_eq!(count, 1);
            false
        })
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn merge_queue_enqueues_after_confirmation() {
        let target = tempfile::tempdir().unwrap();
        let (_dir, sock, server) = fake_daemon_replies(vec![
            json!({
                "ok": true,
                "payload": {
                    "eligible": [{ "path": "/a", "number": 9, "url": "u", "branch": "feature" }],
                    "skipped": [],
                }
            }),
            json!({
                "ok": true,
                "payload": {
                    "queued": [{ "path": "/a", "number": 9 }],
                    "skipped": [],
                    "failed": [],
                }
            }),
        ]);
        let cmd = MergeQueueCommand {
            paths: vec![target.path().to_path_buf()],
            check: false,
            yes: false,
            socket: Some(sock),
        };
        // Confirming drives the phase-2 execute, consuming the second reply.
        cmd.execute_with(|_| async { true }).await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn merge_queue_check_routes_through_the_worktrees_dispatch() {
        // Drives `WorktreesCommand::execute` → `MergeQueue` arm → `execute` (the
        // real-`confirm_enqueue` wiring), which `--check` returns from before any
        // confirmation, so no stdin is touched.
        let target = tempfile::tempdir().unwrap();
        let (_dir, sock, server) = fake_daemon_replies(vec![json!({
            "ok": true,
            "payload": { "eligible": [], "skipped": [] }
        })]);
        let cmd = WorktreesCommand {
            command: WorktreesSubcommands::MergeQueue(MergeQueueCommand {
                paths: vec![target.path().to_path_buf()],
                check: true,
                yes: false,
                socket: Some(sock),
            }),
        };
        cmd.execute(None).await.unwrap();
        server.await.unwrap();
    }

    // --- Reposition (#1407) ---------------------------------------------------

    #[test]
    fn reposition_parses_flags_and_enforces_the_undo_split() {
        let WorktreesSubcommands::Reposition(cmd) = parse(&[
            "reposition",
            "--reference",
            "/wt/ref",
            "/wt/a",
            "/wt/b",
            "--dry-run",
            "-o",
            "json",
        ]) else {
            panic!("expected the Reposition variant");
        };
        assert_eq!(cmd.reference.as_deref(), Some(Path::new("/wt/ref")));
        assert_eq!(
            cmd.paths,
            vec![PathBuf::from("/wt/a"), PathBuf::from("/wt/b")]
        );
        assert!(cmd.dry_run);
        assert!(!cmd.undo);
        assert_eq!(cmd.output, TableOrJson::Json);

        // `--undo` stands alone: no reference needed.
        let WorktreesSubcommands::Reposition(undo) = parse(&["reposition", "--undo"]) else {
            panic!("expected the Reposition variant");
        };
        assert!(undo.undo);
        assert!(undo.reference.is_none());
    }

    #[test]
    fn reposition_rejects_a_missing_reference_and_undo_combinations() {
        // Without `--undo`, a reference is mandatory — otherwise there is no
        // geometry to copy and the request cannot mean anything.
        assert!(RepositionCommand::try_parse_from(["reposition", "/wt/a"]).is_err());
        // `--undo` restores a recorded batch, so pairing it with a reference or a
        // dry run would be contradictory rather than merely redundant.
        assert!(RepositionCommand::try_parse_from([
            "reposition",
            "--undo",
            "--reference",
            "/wt/ref",
        ])
        .is_err());
        assert!(RepositionCommand::try_parse_from(["reposition", "--undo", "--dry-run"]).is_err());
    }

    #[test]
    fn window_key_for_matches_a_canonicalized_folder() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let wt = dir.path().join("tree");
        std::fs::create_dir(&wt).unwrap();
        let canonical = std::fs::canonicalize(&wt).unwrap();
        let windows = json!({
            "windows": [
                { "key": "other", "folders": ["/definitely/not/here"] },
                { "key": "wanted", "folders": [canonical.to_string_lossy()] },
            ]
        });
        assert_eq!(
            window_key_for(&windows, &wt, "repositioned").unwrap(),
            "wanted"
        );
    }

    #[test]
    fn window_key_for_errors_when_no_window_has_it_open() {
        // The CLI names its targets one at a time, so an unmatched one is a
        // mistake worth reporting — unlike a tree multi-select, where a stale row
        // is expected and skipped.
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let err = window_key_for(&json!({ "windows": [] }), dir.path(), "repositioned")
            .expect_err("an unopened worktree must not resolve");
        assert!(err.to_string().contains("no VS Code window has"), "{err:#}");
        // The verb names the caller's action, so the same helper serves both
        // commands without either one's error mentioning the other.
        let err = window_key_for(&json!({ "windows": [] }), dir.path(), "reloaded")
            .expect_err("an unopened worktree must not resolve");
        assert!(err.to_string().contains("can be reloaded"), "{err:#}");
        // A path that does not exist at all fails earlier, on canonicalization.
        let missing = dir.path().join("gone");
        let err = window_key_for(&json!({ "windows": [] }), &missing, "repositioned")
            .expect_err("a nonexistent path must not resolve");
        assert!(err.to_string().contains("cannot resolve"), "{err:#}");
    }

    #[test]
    fn reload_command_requires_at_least_one_path() {
        // Unlike the tree view's sweep, the CLI names each target, so an empty
        // invocation is a mistake rather than an empty batch.
        assert!(ReloadCommand::try_parse_from(["reload"]).is_err());
        let cmd = ReloadCommand::try_parse_from(["reload", "/wt/a", "/wt/b"]).unwrap();
        assert_eq!(cmd.paths.len(), 2);
        assert!(matches!(cmd.output, TableOrJson::Table));
        assert!(cmd.socket.is_none());
    }

    #[test]
    fn render_reload_reports_what_was_signalled_not_reloaded() {
        // "Signalled" is the only honest word: the directive rides each window's
        // ~10s heartbeat, so nothing has reloaded when this prints.
        let out = render_reload(&json!({ "requested": 2, "signalled": 2, "unknown": [] }));
        assert_eq!(out, "Signalled 2 of 2 windows to reload.");
        assert!(!out.contains("Reloaded"), "{out}");
        // Singular when exactly one window was signalled.
        let one = render_reload(&json!({ "requested": 1, "signalled": 1, "unknown": [] }));
        assert_eq!(one, "Signalled 1 of 1 window to reload.");
    }

    #[test]
    fn render_reload_names_windows_that_had_already_closed() {
        // A window that closed between the `list` and the op landing is named,
        // never silently dropped from the count.
        let out = render_reload(&json!({
            "requested": 3,
            "signalled": 1,
            "unknown": ["w2", "w3"],
        }));
        assert!(
            out.starts_with("Signalled 1 of 3 windows to reload."),
            "{out}"
        );
        assert!(out.contains("No longer open"), "{out}");
        assert!(out.contains("w2, w3"), "{out}");
    }

    #[test]
    fn render_reload_tolerates_a_reply_missing_every_field() {
        // Forward-compatible like the other renderers: a field the daemon did not
        // send reads as zero rather than panicking.
        assert_eq!(
            render_reload(&json!({})),
            "Signalled 0 of 0 windows to reload."
        );
    }

    #[test]
    fn render_reposition_explains_a_missing_permission() {
        let out = render_reposition(&json!({ "trusted": false, "results": [] }));
        assert!(out.contains("Accessibility permission"), "{out}");
        assert!(out.contains("daemon restart"), "{out}");
    }

    #[test]
    fn render_reposition_reports_a_blocked_batch() {
        let out = render_reposition(&json!({
            "trusted": true,
            "blocked": { "reason": "reference-ambiguous", "detail": "2 windows match “main”" },
            "results": [],
        }));
        assert!(out.contains("Nothing was moved"), "{out}");
        assert!(out.contains("reference-ambiguous"), "{out}");
        assert!(out.contains("2 windows match"), "{out}");
    }

    #[test]
    fn render_reposition_renders_the_reference_and_per_target_outcomes() {
        let out = render_reposition(&json!({
            "trusted": true,
            "reference": {
                "key": "r",
                "title": "ref-tree",
                "frame": { "x": 10.4, "y": 20.6, "width": 800.0, "height": 600.0 },
            },
            "moved": 1,
            "skipped": 1,
            "results": [
                { "key": "a", "title": "a-tree", "outcome": "moved", "detail": "moved into position" },
                { "key": "b", "title": "b-tree", "outcome": "ambiguous", "detail": "2 match" },
            ],
        }));
        assert!(
            out.contains("Reference: ref-tree 800×600 at (10, 21)"),
            "{out}"
        );
        assert!(out.contains("Moved: 1 / Skipped: 1"), "{out}");
        assert!(out.contains("moved: a-tree"), "{out}");
        assert!(out.contains("ambiguous: b-tree"), "{out}");
    }

    #[test]
    fn render_reposition_falls_back_to_the_key_and_notes_an_empty_batch() {
        // A target the daemon could not name (no title reported) is still
        // identified, by key.
        let out = render_reposition(&json!({
            "trusted": true,
            "results": [{ "key": "keyless", "outcome": "no-window", "detail": "gone" }],
        }));
        assert!(out.contains("no-window: keyless"), "{out}");
        // An undo with nothing recorded reports rather than printing a bare header.
        let empty = render_reposition(&json!({ "trusted": true, "results": [] }));
        assert!(empty.contains("(nothing to report)"), "{empty}");
    }

    #[test]
    fn render_reposition_strips_control_bytes_from_daemon_strings() {
        // A window title is companion-supplied metadata, so it cannot be allowed
        // to inject escape sequences into the operator's terminal (#1137).
        let out = render_reposition(&json!({
            "trusted": true,
            "results": [{
                "key": "k",
                "title": "evil\u{1b}[31mred",
                "outcome": "moved",
                "detail": "ok\u{7}",
            }],
        }));
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains('\u{7}'), "{out:?}");
    }

    #[test]
    fn render_frame_formats_or_dashes() {
        assert_eq!(render_frame(None), "-");
        assert_eq!(
            render_frame(Some(
                &json!({ "x": 1.5, "y": -2.4, "width": 100.0, "height": 50.0 })
            )),
            "100×50 at (2, -2)"
        );
        // A malformed frame degrades to zeroes rather than panicking.
        assert_eq!(render_frame(Some(&json!({}))), "0×0 at (0, 0)");
    }

    #[test]
    fn print_reposition_emits_both_formats() {
        let reply = json!({ "trusted": true, "results": [], "moved": 0, "skipped": 0 });
        print_reposition(TableOrJson::Table, &reply).unwrap();
        print_reposition(TableOrJson::Json, &reply).unwrap();
    }

    #[tokio::test]
    async fn reposition_maps_paths_to_window_keys_and_sends_the_op() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let reference = dir.path().join("ref");
        let target = dir.path().join("tgt");
        std::fs::create_dir(&reference).unwrap();
        std::fs::create_dir(&target).unwrap();
        let (canon_ref, canon_tgt) = (
            std::fs::canonicalize(&reference).unwrap(),
            std::fs::canonicalize(&target).unwrap(),
        );

        // Two round trips: the `list` that resolves paths to window keys, then the
        // op itself.
        let (_sock_dir, sock, server) = fake_daemon_replies(vec![
            json!({ "ok": true, "payload": { "windows": [
                { "key": "ref-key", "folders": [canon_ref.to_string_lossy()] },
                { "key": "tgt-key", "folders": [canon_tgt.to_string_lossy()] },
            ] } }),
            json!({ "ok": true, "payload": {
                "trusted": true,
                "moved": 1,
                "skipped": 0,
                "results": [{ "key": "tgt-key", "outcome": "moved", "detail": "moved into position" }],
            } }),
        ]);

        RepositionCommand {
            paths: vec![target],
            reference: Some(reference),
            dry_run: false,
            undo: false,
            output: TableOrJson::Json,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reposition_undo_skips_the_list_lookup_entirely() {
        // Undo addresses whatever the daemon recorded, so it needs no path
        // resolution and issues exactly one request.
        let (_dir, sock, server) = fake_daemon_reply(json!({
            "ok": true,
            "payload": { "trusted": true, "moved": 2, "skipped": 0, "results": [] },
        }));
        RepositionCommand {
            paths: Vec::new(),
            reference: None,
            dry_run: false,
            undo: true,
            output: TableOrJson::Table,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reposition_fails_before_the_op_when_a_target_has_no_window() {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let reference = dir.path().join("ref");
        let target = dir.path().join("tgt");
        std::fs::create_dir(&reference).unwrap();
        std::fs::create_dir(&target).unwrap();
        let canon_ref = std::fs::canonicalize(&reference).unwrap();

        // Only the reference is open, so the target cannot be resolved — and the
        // `reposition` op is never sent, which is why one canned reply suffices.
        let (_sock_dir, sock, server) = fake_daemon_reply(json!({
            "ok": true,
            "payload": { "windows": [
                { "key": "ref-key", "folders": [canon_ref.to_string_lossy()] },
            ] },
        }));
        let err = RepositionCommand {
            paths: vec![target],
            reference: Some(reference),
            dry_run: true,
            undo: false,
            output: TableOrJson::Table,
            socket: Some(sock),
        }
        .execute()
        .await
        .expect_err("an unopened target must abort the command");
        assert!(err.to_string().contains("no VS Code window has"), "{err:#}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reposition_surfaces_a_daemon_error() {
        let (_dir, sock, server) = fake_daemon_reply(json!({
            "ok": false,
            "error": "unknown worktrees op: reposition",
        }));
        let err = RepositionCommand {
            paths: Vec::new(),
            reference: None,
            dry_run: false,
            undo: true,
            output: TableOrJson::Table,
            socket: Some(sock),
        }
        .execute()
        .await
        .expect_err("an `ok:false` reply must not be reported as success");
        assert!(err.to_string().contains("unknown worktrees op"), "{err:#}");
        server.await.unwrap();
    }

    /// Two open worktrees plus the canned `list` reply that resolves both, so a
    /// reload test only has to supply the op's own reply.
    fn reload_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, Value) {
        let dir = tempfile::tempdir_in("/tmp").unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        let list = json!({ "ok": true, "payload": { "windows": [
            { "key": "key-a", "folders": [std::fs::canonicalize(&a).unwrap().to_string_lossy()] },
            { "key": "key-b", "folders": [std::fs::canonicalize(&b).unwrap().to_string_lossy()] },
        ] } });
        (dir, a, b, list)
    }

    #[tokio::test]
    async fn reload_resolves_paths_to_window_keys_before_sending_the_op() {
        let (_dir, a, b, list) = reload_fixture();
        // Two requests: the `list` that maps paths to windows, then the op itself.
        // `fake_daemon_seq` (not `_replies`) so the sent payloads can be asserted.
        let (_sock_dir, sock, server) = fake_daemon_seq(vec![
            list,
            json!({ "ok": true, "payload": {
                "requested": 2, "signalled": 2, "unknown": [],
            } }),
        ]);

        ReloadCommand {
            paths: vec![a, b],
            output: TableOrJson::Table,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();

        // The op addresses *windows*: the daemon must receive the resolved keys,
        // never the paths the user typed.
        let requests = server.await.unwrap();
        assert_eq!(requests[1]["op"], "reload");
        assert_eq!(
            requests[1]["payload"]["target_keys"],
            json!(["key-a", "key-b"])
        );
        assert!(
            requests[1]["payload"].get("requester_key").is_none(),
            "a CLI process is not a window, so it must not claim to be one"
        );
    }

    #[tokio::test]
    async fn reload_json_output_passes_the_reply_through_verbatim() {
        let (_dir, a, _b, list) = reload_fixture();
        let (_sock_dir, sock, server) = fake_daemon_replies(vec![
            list,
            json!({ "ok": true, "payload": {
                "requested": 1, "signalled": 0, "unknown": ["key-a"],
            } }),
        ]);
        // The `-o json` arm is the machine-readable surface, so it must not go
        // through the human renderer.
        ReloadCommand {
            paths: vec![a],
            output: TableOrJson::Json,
            socket: Some(sock),
        }
        .execute()
        .await
        .unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reload_fails_before_the_op_when_a_target_has_no_window() {
        let (_dir, a, b, _list) = reload_fixture();
        // Only `a` is open, so `b` cannot be resolved and the `reload` op is never
        // sent — which is why one canned reply suffices. Unlike the tree view's
        // silent skip, the CLI named this target explicitly, so it is an error.
        let (_sock_dir, sock, server) = fake_daemon_reply(json!({
            "ok": true,
            "payload": { "windows": [
                { "key": "key-a", "folders": [std::fs::canonicalize(&a).unwrap().to_string_lossy()] },
            ] },
        }));
        let err = ReloadCommand {
            paths: vec![a, b],
            output: TableOrJson::Table,
            socket: Some(sock),
        }
        .execute()
        .await
        .expect_err("an unopened target must abort the command");
        assert!(err.to_string().contains("can be reloaded"), "{err:#}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn reload_surfaces_a_daemon_error() {
        let (_dir, a, _b, list) = reload_fixture();
        let (_sock_dir, sock, server) = fake_daemon_replies(vec![
            list,
            json!({ "ok": false, "error": "unknown worktrees op: reload" }),
        ]);
        // An older daemon that predates #1417 rejects the op; that must surface
        // rather than read as a successful no-op.
        let err = ReloadCommand {
            paths: vec![a],
            output: TableOrJson::Table,
            socket: Some(sock),
        }
        .execute()
        .await
        .expect_err("an `ok:false` reply must not be reported as success");
        assert!(err.to_string().contains("unknown worktrees op"), "{err:#}");
        server.await.unwrap();
    }
}
