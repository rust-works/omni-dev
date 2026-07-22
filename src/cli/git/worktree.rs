//! Logged wrappers over `git worktree` (issue #1392).
//!
//! Each verb is a thin shell over the real `git worktree <verb>`; the value is
//! the `kind: "worktree"` request-log record it writes, carrying the
//! recovery-relevant metadata (path/branch/commit/dirtiness) that lets
//! `omni-dev log --command 'git worktree remove'` answer "what worktree did I
//! just remove?" — the row needed to run `git branch <branch> <commit>` and
//! re-attach. Metadata capture is strictly best-effort: enrichment failures
//! never change the wrapped git operation's behaviour or exit status.
//!
//! Distinct from the top-level `omni-dev worktrees` command, which lists the
//! worktrees open across VS Code windows via the daemon.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::request_log::{self, WorktreeOutcome};

/// Worktree operations: logged wrappers over `git worktree`.
#[derive(Parser)]
#[command(
    long_about = "Worktree operations: thin wrappers over `git worktree` that record \
recovery-relevant metadata (path, branch, commit, uncommitted status) to the omni-dev \
request log. If a worktree is removed by mistake, `omni-dev log --command 'git worktree \
remove'` returns the row needed to recover the branch.\n\nNot to be confused with \
`omni-dev worktrees`, which lists worktrees open across VS Code windows via the daemon."
)]
pub struct WorktreeCommand {
    /// Worktree subcommand to execute.
    #[command(subcommand)]
    pub command: WorktreeSubcommands,
}

/// Worktree subcommands.
#[derive(Subcommand)]
pub enum WorktreeSubcommands {
    /// Creates a worktree at `<path>` (wraps `git worktree add`).
    Add(AddArgs),
    /// Removes a worktree, recording branch/commit/dirtiness first (wraps `git worktree remove`).
    Remove(RemoveArgs),
    /// Lists worktrees (wraps `git worktree list`).
    List(ListArgs),
    /// Moves a worktree to a new location (wraps `git worktree move`).
    Move(MoveArgs),
    /// Prunes stale worktree metadata, recording what was pruned (wraps `git worktree prune`).
    Prune(PruneArgs),
    /// Repairs worktree administrative files (wraps `git worktree repair`).
    Repair(RepairArgs),
}

/// Arguments for `git worktree add`.
#[derive(Parser)]
pub struct AddArgs {
    /// Path for the new worktree.
    pub path: PathBuf,
    /// Commit-ish to check out (defaults to HEAD).
    pub commit_ish: Option<String>,
    /// Create a new branch for the worktree.
    #[arg(short = 'b', long = "branch", value_name = "BRANCH")]
    pub branch: Option<String>,
    /// Checkout even if another worktree holds the branch.
    #[arg(short = 'f', long)]
    pub force: bool,
    /// Detach HEAD in the new worktree.
    #[arg(long)]
    pub detach: bool,
}

/// Arguments for `git worktree remove`.
#[derive(Parser)]
pub struct RemoveArgs {
    /// Path of the worktree to remove.
    pub path: PathBuf,
    /// Remove even if the worktree is dirty or locked.
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Arguments for `git worktree list`.
#[derive(Parser)]
pub struct ListArgs {
    /// Emit git's machine-readable porcelain output verbatim.
    #[arg(long, conflicts_with = "output_json")]
    pub porcelain: bool,
    /// Emit the porcelain data as JSON.
    #[arg(long)]
    pub output_json: bool,
}

/// Arguments for `git worktree move`.
#[derive(Parser)]
pub struct MoveArgs {
    /// Current path of the worktree.
    pub from: PathBuf,
    /// New path for the worktree.
    pub to: PathBuf,
    /// Move even if the worktree is dirty or locked.
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Arguments for `git worktree prune`.
#[derive(Parser)]
pub struct PruneArgs {
    /// Do not remove anything; report what would be removed.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Report all removals.
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Expire working trees older than TIME.
    #[arg(long, value_name = "TIME")]
    pub expire: Option<String>,
}

/// Arguments for `git worktree repair`.
#[derive(Parser)]
pub struct RepairArgs {
    /// Worktree paths to repair (all, when omitted).
    pub paths: Vec<PathBuf>,
}

impl WorktreeCommand {
    /// Executes the worktree command.
    ///
    /// `repo` is the CLI-level `--repo` location; git runs with it as the
    /// working directory, so relative worktree paths resolve against it
    /// (matching `git -C` semantics).
    pub fn execute(self, repo: Option<&Path>) -> Result<()> {
        let base = base_dir(repo)?;
        match self.command {
            WorktreeSubcommands::Add(args) => run_add(&base, &args),
            WorktreeSubcommands::Remove(args) => run_remove(&base, &args),
            WorktreeSubcommands::List(args) => run_list(&base, &args),
            WorktreeSubcommands::Move(args) => run_move(&base, &args),
            WorktreeSubcommands::Prune(args) => run_prune(&base, &args),
            WorktreeSubcommands::Repair(args) => run_repair(&base, &args),
        }
    }
}

// --- per-verb flows -------------------------------------------------------

fn run_add(base: &Path, args: &AddArgs) -> Result<()> {
    let argv = add_argv(args);
    let mut context = base_context(base);
    let abs = absolutize(base, &args.path);
    let run = run_git(base, &argv);
    echo(&run);
    context.insert("path".to_string(), display_canonical(&abs));
    if run.exit_code == Some(0) {
        // The branch and HEAD only exist after a successful checkout.
        insert_snapshot(&mut context, &snapshot(&abs), false);
    }
    finish("add", argv, run, context)
}

fn run_remove(base: &Path, args: &RemoveArgs) -> Result<()> {
    let argv = remove_argv(args);
    let mut context = base_context(base);
    let abs = absolutize(base, &args.path);
    // Captured BEFORE git deletes the working directory — this is the record's
    // entire recovery value; on failure it is still written.
    context.insert("path".to_string(), display_canonical(&abs));
    context.insert("used_force".to_string(), args.force.to_string());
    let snap = snapshot(&abs);
    insert_snapshot(&mut context, &snap, true);
    let run = run_git(base, &argv);
    echo(&run);
    finish("remove", argv, run, context)
}

fn run_list(base: &Path, args: &ListArgs) -> Result<()> {
    let porcelain = args.porcelain || args.output_json;
    let argv = list_argv(porcelain);
    let mut context = base_context(base);
    let run = run_git(base, &argv);
    if args.output_json {
        // JSON replaces git's stdout; stderr still passes through.
        echo_stderr(&run);
        if run.exit_code == Some(0) {
            let stdout = String::from_utf8_lossy(&run.stdout);
            let entries = parse_worktree_porcelain(&stdout);
            context.insert("count".to_string(), entries.len().to_string());
            let json = serde_json::to_string_pretty(&entries)
                .context("Failed to serialize worktree list as JSON")?;
            println!("{json}");
        }
    } else {
        echo(&run);
        if run.exit_code == Some(0) {
            let stdout = String::from_utf8_lossy(&run.stdout);
            let count = if porcelain {
                parse_worktree_porcelain(&stdout).len()
            } else {
                stdout.lines().filter(|l| !l.trim().is_empty()).count()
            };
            context.insert("count".to_string(), count.to_string());
        }
    }
    finish("list", argv, run, context)
}

fn run_move(base: &Path, args: &MoveArgs) -> Result<()> {
    let argv = move_argv(args);
    let mut context = base_context(base);
    let from_abs = absolutize(base, &args.from);
    context.insert("from_path".to_string(), display_canonical(&from_abs));
    context.insert(
        "to_path".to_string(),
        absolutize(base, &args.to).display().to_string(),
    );
    // Branch captured before the move, in case the move fails midway.
    let snap = snapshot(&from_abs);
    if let Some(branch) = &snap.branch {
        context.insert("branch".to_string(), branch.clone());
    }
    let run = run_git(base, &argv);
    echo(&run);
    finish("move", argv, run, context)
}

fn run_prune(base: &Path, args: &PruneArgs) -> Result<()> {
    let argv = prune_argv(args);
    let mut context = base_context(base);
    if args.dry_run {
        context.insert("dry_run".to_string(), "true".to_string());
    }
    // Diffing the porcelain list before vs after yields the pruned entries
    // with the branch/commit that git still reports for prunable stanzas.
    let before = list_entries(base);
    let run = run_git(base, &argv);
    echo(&run);
    if let (Some(before), Some(after)) = (before, list_entries(base)) {
        let pruned = pruned_entries(&before, &after);
        match serde_json::to_string(&pruned) {
            Ok(json) => {
                context.insert("pruned".to_string(), json);
            }
            Err(e) => tracing::debug!("Cannot serialize pruned worktree list: {e}"),
        }
    }
    finish("prune", argv, run, context)
}

fn run_repair(base: &Path, args: &RepairArgs) -> Result<()> {
    let argv = repair_argv(args);
    let mut context = base_context(base);
    let run = run_git(base, &argv);
    echo(&run);
    // git reports repairs on both streams depending on the verb/version.
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr)
    );
    let repaired = parse_repair_paths(&combined);
    if !repaired.is_empty() {
        match serde_json::to_string(&repaired) {
            Ok(json) => {
                context.insert("repaired".to_string(), json);
            }
            Err(e) => tracing::debug!("Cannot serialize repaired worktree list: {e}"),
        }
    }
    finish("repair", argv, run, context)
}

/// Records the outcome and converts the subprocess result into the command's
/// result: success on exit 0, an error (non-zero omni-dev exit) otherwise.
fn finish(
    verb: &str,
    argv: Vec<String>,
    run: GitRun,
    context: BTreeMap<String, String>,
) -> Result<()> {
    request_log::record_worktree(WorktreeOutcome {
        verb: verb.to_string(),
        argv,
        exit_code: run.exit_code,
        duration: run.duration,
        error: run.error.clone(),
        context,
    });
    match (run.exit_code, run.error) {
        (Some(0), _) => Ok(()),
        // Stderr was already echoed, so the error chain stays short.
        (Some(code), _) => anyhow::bail!("git worktree {verb} failed with exit code {code}"),
        (None, err) => anyhow::bail!(
            "Failed to run git worktree {verb}: {}",
            err.unwrap_or_else(|| "unknown spawn error".to_string())
        ),
    }
}

// --- argv builders (pure, unit-tested) ------------------------------------

fn add_argv(args: &AddArgs) -> Vec<String> {
    let mut argv = vec!["worktree".to_string(), "add".to_string()];
    if args.force {
        argv.push("--force".to_string());
    }
    if args.detach {
        argv.push("--detach".to_string());
    }
    if let Some(branch) = &args.branch {
        argv.push("-b".to_string());
        argv.push(branch.clone());
    }
    argv.push(args.path.display().to_string());
    if let Some(commit_ish) = &args.commit_ish {
        argv.push(commit_ish.clone());
    }
    argv
}

fn remove_argv(args: &RemoveArgs) -> Vec<String> {
    let mut argv = vec!["worktree".to_string(), "remove".to_string()];
    if args.force {
        argv.push("--force".to_string());
    }
    argv.push(args.path.display().to_string());
    argv
}

fn list_argv(porcelain: bool) -> Vec<String> {
    if porcelain {
        // `core.quotePath=false` keeps non-ASCII paths literal for the parser.
        vec![
            "-c".to_string(),
            "core.quotePath=false".to_string(),
            "worktree".to_string(),
            "list".to_string(),
            "--porcelain".to_string(),
        ]
    } else {
        vec!["worktree".to_string(), "list".to_string()]
    }
}

fn move_argv(args: &MoveArgs) -> Vec<String> {
    let mut argv = vec!["worktree".to_string(), "move".to_string()];
    if args.force {
        argv.push("--force".to_string());
    }
    argv.push(args.from.display().to_string());
    argv.push(args.to.display().to_string());
    argv
}

fn prune_argv(args: &PruneArgs) -> Vec<String> {
    let mut argv = vec!["worktree".to_string(), "prune".to_string()];
    if args.dry_run {
        argv.push("--dry-run".to_string());
    }
    if args.verbose {
        argv.push("--verbose".to_string());
    }
    if let Some(expire) = &args.expire {
        argv.push("--expire".to_string());
        argv.push(expire.clone());
    }
    argv
}

fn repair_argv(args: &RepairArgs) -> Vec<String> {
    let mut argv = vec!["worktree".to_string(), "repair".to_string()];
    argv.extend(args.paths.iter().map(|p| p.display().to_string()));
    argv
}

// --- subprocess runner ----------------------------------------------------

/// The observed result of one `git` subprocess.
struct GitRun {
    exit_code: Option<i32>,
    duration: Duration,
    error: Option<String>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Builds a `git` [`Command`] whose child receives a snapshot of the current
/// environment (`env_clear` + `envs`), keeping the spawn out of the data race
/// against concurrent `std::env::set_var` (issue #1022; same idiom as
/// `crate::cli::ai::claude::skills`).
fn git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    cmd.envs(std::env::vars_os());
    cmd
}

/// Runs `git <argv>` in `base`, timing it. Never fails: a spawn error lands in
/// [`GitRun::error`] so the caller can record it before reporting.
fn run_git(base: &Path, argv: &[String]) -> GitRun {
    let started = Instant::now();
    let result = git_command().args(argv).current_dir(base).output();
    let duration = started.elapsed();
    match result {
        Ok(output) => GitRun {
            exit_code: output.status.code(),
            duration,
            error: None,
            stdout: output.stdout,
            stderr: output.stderr,
        },
        Err(e) => GitRun {
            exit_code: None,
            duration,
            error: Some(e.to_string()),
            stdout: Vec::new(),
            stderr: Vec::new(),
        },
    }
}

/// Echoes the captured git output to our own streams (thin-wrapper UX).
fn echo(run: &GitRun) {
    // Best-effort: a write failure here (e.g. EPIPE from a closed pager) is
    // the downstream consumer's condition, not ours.
    let _ = std::io::stdout().write_all(&run.stdout);
    echo_stderr(run);
}

/// Echoes only git's stderr (used when stdout is replaced, e.g. `--output-json`).
fn echo_stderr(run: &GitRun) {
    // Best-effort, as in `echo`.
    let _ = std::io::stderr().write_all(&run.stderr);
}

// --- metadata capture (best-effort) ---------------------------------------

/// Resolves the directory git runs in: `--repo` when given, else the cwd.
fn base_dir(repo: Option<&Path>) -> Result<PathBuf> {
    match repo {
        Some(p) => Ok(p.to_path_buf()),
        None => std::env::current_dir().context("Failed to resolve working directory"),
    }
}

/// Context common to every verb: the main worktree root, when resolvable.
fn base_context(base: &Path) -> BTreeMap<String, String> {
    let mut context = BTreeMap::new();
    if let Some(toplevel) = toplevel(base) {
        context.insert("repo".to_string(), toplevel);
    }
    context
}

/// Best-effort `git rev-parse --show-toplevel` from `base`.
fn toplevel(base: &Path) -> Option<String> {
    let output = git_command()
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(base)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    let trimmed = stdout.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Best-effort git2 snapshot of a worktree; every field degrades to absent.
#[derive(Default)]
struct WtSnapshot {
    branch: Option<String>,
    detached: bool,
    commit: Option<String>,
    dirty: Option<bool>,
}

/// Reads branch, HEAD commit, and dirtiness from the worktree at `path`.
fn snapshot(path: &Path) -> WtSnapshot {
    let mut snap = WtSnapshot::default();
    let repo = match git2::Repository::open(path) {
        Ok(repo) => repo,
        Err(e) => {
            tracing::debug!("Cannot open {} as a git repository: {e}", path.display());
            return snap;
        }
    };
    match repo.head() {
        Ok(head) => {
            if head.is_branch() {
                // Err = non-UTF-8 ref name; omit the branch rather than mangle it.
                snap.branch = head.shorthand().ok().map(str::to_string);
            } else {
                snap.detached = true;
            }
            snap.commit = head.target().map(|oid| oid.to_string());
        }
        Err(e) => tracing::debug!("Cannot read HEAD of {}: {e}", path.display()),
    }
    // Untracked files count as dirty: they are exactly what `remove --force`
    // destroys. Same StatusOptions recipe as the daemon worktrees service.
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(false)
        .exclude_submodules(true);
    match repo.statuses(Some(&mut opts)) {
        Ok(statuses) => snap.dirty = Some(!statuses.is_empty()),
        Err(e) => tracing::debug!("Cannot read status of {}: {e}", path.display()),
    }
    snap
}

/// Inserts the snapshot's fields into `context`, skipping unknown ones.
fn insert_snapshot(context: &mut BTreeMap<String, String>, snap: &WtSnapshot, with_dirty: bool) {
    if let Some(branch) = &snap.branch {
        context.insert("branch".to_string(), branch.clone());
    }
    if snap.detached {
        context.insert("detached".to_string(), "true".to_string());
    }
    if let Some(commit) = &snap.commit {
        context.insert("commit".to_string(), commit.clone());
    }
    if with_dirty {
        if let Some(dirty) = snap.dirty {
            context.insert("had_uncommitted".to_string(), dirty.to_string());
        }
    }
}

/// Resolves `path` against `base` when relative (matching `git -C` semantics).
fn absolutize(base: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

/// Canonical display form of `path`; falls back to the joined path when the
/// target does not exist (yet, or any more).
fn display_canonical(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .display()
        .to_string()
}

// --- porcelain parsing (pure, unit-tested) --------------------------------

/// One stanza of `git worktree list --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeEntry {
    /// Absolute worktree root path.
    pub path: String,
    /// HEAD commit sha.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Checked-out branch, `refs/heads/` prefix stripped.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// True when HEAD is detached.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub detached: bool,
    /// True for a bare checkout.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub bare: bool,
    /// Lock reason; empty string when locked without a reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locked: Option<String>,
    /// Prunable reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prunable: Option<String>,
}

impl WorktreeEntry {
    fn new(path: String) -> Self {
        Self {
            path,
            head: None,
            branch: None,
            detached: false,
            bare: false,
            locked: None,
            prunable: None,
        }
    }
}

/// Parses `git worktree list --porcelain` output. Unknown attributes are
/// ignored so future git versions cannot break the parse.
fn parse_worktree_porcelain(out: &str) -> Vec<WorktreeEntry> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeEntry> = None;
    for line in out.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.extend(current.take());
            current = Some(WorktreeEntry::new(path.to_string()));
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        if let Some(head) = line.strip_prefix("HEAD ") {
            entry.head = Some(head.to_string());
        } else if let Some(branch) = line.strip_prefix("branch ") {
            entry.branch = Some(
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_string(),
            );
        } else if line == "detached" {
            entry.detached = true;
        } else if line == "bare" {
            entry.bare = true;
        } else if line == "locked" {
            entry.locked = Some(String::new());
        } else if let Some(reason) = line.strip_prefix("locked ") {
            entry.locked = Some(reason.to_string());
        } else if let Some(reason) = line.strip_prefix("prunable ") {
            entry.prunable = Some(reason.to_string());
        }
        // Blank lines end a stanza implicitly; the next `worktree ` flushes.
    }
    entries.extend(current);
    entries
}

/// Best-effort porcelain listing of the repo at `base` (for the prune diff).
fn list_entries(base: &Path) -> Option<Vec<WorktreeEntry>> {
    let output = git_command()
        .args([
            "-c",
            "core.quotePath=false",
            "worktree",
            "list",
            "--porcelain",
        ])
        .current_dir(base)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(parse_worktree_porcelain(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

/// A pruned worktree, as recorded in the `pruned` context field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PrunedEntry {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
}

/// The entries present in `before` but gone from `after` — what prune removed,
/// with the branch/commit git still reported for the prunable stanzas.
fn pruned_entries(before: &[WorktreeEntry], after: &[WorktreeEntry]) -> Vec<PrunedEntry> {
    before
        .iter()
        .filter(|b| !after.iter().any(|a| a.path == b.path))
        .map(|b| PrunedEntry {
            path: b.path.clone(),
            branch: b.branch.clone(),
            commit: b.head.clone(),
        })
        .collect()
}

/// Extracts the paths from `git worktree repair` report lines, best-effort.
/// The messages are human-readable (`repair: <description>: <path>`), so an
/// upstream wording change yields an empty list rather than an error.
fn parse_repair_paths(out: &str) -> Vec<String> {
    out.lines()
        .filter_map(|line| line.strip_prefix("repair: "))
        .filter_map(|rest| rest.rsplit_once(": ").map(|(_, path)| path.to_string()))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().copied().map(String::from).collect()
    }

    // --- porcelain parser ---

    #[test]
    fn porcelain_parses_main_linked_and_detached_stanzas() {
        let out = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /wt/feature
HEAD 2222222222222222222222222222222222222222
branch refs/heads/issue-1392

worktree /wt/detached
HEAD 3333333333333333333333333333333333333333
detached
";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].path, "/repo");
        assert_eq!(entries[0].branch.as_deref(), Some("main"));
        assert_eq!(
            entries[0].head.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(entries[1].branch.as_deref(), Some("issue-1392"));
        assert!(entries[2].detached);
        assert!(entries[2].branch.is_none());
    }

    #[test]
    fn porcelain_parses_bare_locked_prunable_and_ignores_unknown() {
        let out = "\
worktree /bare
bare

worktree /locked-no-reason
HEAD 4444444444444444444444444444444444444444
locked

worktree /locked-with-reason
locked worktree is on a portable device
future-attribute some value

worktree /stale
prunable gitdir file points to non-existent location";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries.len(), 4);
        assert!(entries[0].bare);
        assert_eq!(entries[1].locked.as_deref(), Some(""));
        assert_eq!(
            entries[2].locked.as_deref(),
            Some("worktree is on a portable device")
        );
        assert_eq!(
            entries[3].prunable.as_deref(),
            Some("gitdir file points to non-existent location")
        );
    }

    #[test]
    fn porcelain_entry_serializes_compactly() {
        let entries = parse_worktree_porcelain("worktree /repo\nbranch refs/heads/main\n");
        let json = serde_json::to_string(&entries).unwrap();
        // Absent/default fields are skipped.
        assert_eq!(json, r#"[{"path":"/repo","branch":"main"}]"#);
    }

    // --- argv builders ---

    #[test]
    fn add_argv_covers_flags_positionals_and_order() {
        let args = AddArgs {
            path: PathBuf::from("../wt"),
            commit_ish: Some("origin/main".to_string()),
            branch: Some("issue-1".to_string()),
            force: true,
            detach: false,
        };
        assert_eq!(
            add_argv(&args),
            argv(&[
                "worktree",
                "add",
                "--force",
                "-b",
                "issue-1",
                "../wt",
                "origin/main"
            ])
        );
        let minimal = AddArgs {
            path: PathBuf::from("wt"),
            commit_ish: None,
            branch: None,
            force: false,
            detach: true,
        };
        assert_eq!(
            add_argv(&minimal),
            argv(&["worktree", "add", "--detach", "wt"])
        );
    }

    #[test]
    fn remove_move_prune_repair_argv() {
        assert_eq!(
            remove_argv(&RemoveArgs {
                path: PathBuf::from("/wt"),
                force: true,
            }),
            argv(&["worktree", "remove", "--force", "/wt"])
        );
        assert_eq!(
            move_argv(&MoveArgs {
                from: PathBuf::from("a"),
                to: PathBuf::from("b"),
                force: false,
            }),
            argv(&["worktree", "move", "a", "b"])
        );
        assert_eq!(
            prune_argv(&PruneArgs {
                dry_run: true,
                verbose: true,
                expire: Some("1.day".to_string()),
            }),
            argv(&[
                "worktree",
                "prune",
                "--dry-run",
                "--verbose",
                "--expire",
                "1.day"
            ])
        );
        assert_eq!(
            repair_argv(&RepairArgs {
                paths: vec![PathBuf::from("a"), PathBuf::from("b")],
            }),
            argv(&["worktree", "repair", "a", "b"])
        );
    }

    #[test]
    fn list_argv_adds_quote_path_only_for_porcelain() {
        assert_eq!(list_argv(false), argv(&["worktree", "list"]));
        assert_eq!(
            list_argv(true),
            argv(&[
                "-c",
                "core.quotePath=false",
                "worktree",
                "list",
                "--porcelain"
            ])
        );
    }

    // --- prune diff + repair parse ---

    #[test]
    fn pruned_entries_diffs_by_path_and_keeps_branch_commit() {
        let before = parse_worktree_porcelain(
            "worktree /repo\nbranch refs/heads/main\n\n\
             worktree /stale\nHEAD 5555555555555555555555555555555555555555\n\
             branch refs/heads/gone\nprunable gitdir gone\n",
        );
        let after = parse_worktree_porcelain("worktree /repo\nbranch refs/heads/main\n");
        let pruned = pruned_entries(&before, &after);
        assert_eq!(pruned.len(), 1);
        assert_eq!(pruned[0].path, "/stale");
        assert_eq!(pruned[0].branch.as_deref(), Some("gone"));
        assert_eq!(
            pruned[0].commit.as_deref(),
            Some("5555555555555555555555555555555555555555")
        );
        assert!(pruned_entries(&after, &after).is_empty());
    }

    #[test]
    fn repair_paths_parsed_from_report_lines_best_effort() {
        let out = "\
repair: gitdir file points to non-existent location: /wt/one/.git
not a repair line
repair: .git file broken: /wt/two/.git";
        assert_eq!(
            parse_repair_paths(out),
            argv(&["/wt/one/.git", "/wt/two/.git"])
        );
        assert!(parse_repair_paths("nothing to repair").is_empty());
    }

    // --- clap surface ---

    #[derive(Parser)]
    struct Wrapper {
        #[command(subcommand)]
        cmd: WorktreeSubcommands,
    }

    #[test]
    fn clap_parses_every_verb() {
        for args in [
            vec!["omni-dev", "add", "wt", "-b", "branch", "origin/main"],
            vec!["omni-dev", "remove", "--force", "wt"],
            vec!["omni-dev", "list", "--porcelain"],
            vec!["omni-dev", "list", "--output-json"],
            vec!["omni-dev", "move", "a", "b"],
            vec!["omni-dev", "prune", "-n", "--expire", "1.day"],
            vec!["omni-dev", "repair", "a", "b"],
            vec!["omni-dev", "repair"],
        ] {
            Wrapper::try_parse_from(&args)
                .unwrap_or_else(|e| panic!("failed to parse {args:?}: {e}"));
        }
    }

    #[test]
    fn clap_rejects_porcelain_with_output_json() {
        assert!(
            Wrapper::try_parse_from(["omni-dev", "list", "--porcelain", "--output-json"]).is_err()
        );
    }
}
