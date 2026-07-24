//! Batch-rebase a repository's worktrees onto its remote default branch, fetching
//! the remote **exactly once per repository** (issue #1400).
//!
//! Keeping a fan-out of feature worktrees current with `main` otherwise means
//! `cd`-ing into each one and running `git fetch` + `git rebase origin/main` by
//! hand. Doing that naively re-fetches the remote for every worktree, which is both
//! wasteful (N network round-trips) and subtly wrong: if `origin/main` advances
//! between fetches, different worktrees rebase onto different tips and stop sharing
//! a base. Linked worktrees share one object database, so a single
//! `git fetch <remote> <branch>` updates the `refs/remotes/<remote>/<branch>`
//! tracking ref every worktree already sees — which is exactly why "fetch once per
//! repo, then rebase each onto that pinned ref" is the natural design.
//!
//! **Split of concerns (see ADR-0003 and ADR-0055).** Every git *read* — enumerate
//! the worktrees, resolve the onto ref, classify divergence / dirty / repo state —
//! goes through `git2`. The two *mutations*, the fetch and the rebase, shell out to
//! the user's `git`: libgit2's vendored build here has no reliable SSH transport
//! (issue #903) and the shell inherits the user's `ssh-agent` / `~/.ssh/config` /
//! credential-helper configuration for free, and `git rebase` brings full conflict
//! handling, hooks, and `--autostash`. This engine is therefore **CLI-side only** —
//! it never runs in the daemon, whose minimal launchd/systemd environment lacks the
//! user's credential context.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use git2::{Oid, Repository, RepositoryState, StatusOptions};
use serde::Serialize;

use crate::git::remote::RemoteInfo;

/// Which worktrees a batch rebase should target.
#[derive(Debug, Clone)]
pub enum Selection {
    /// Rebase exactly these worktree folders (each resolved to the worktree that
    /// contains it). A path that is the main working tree, is detached, is dirty,
    /// or is not a git worktree is reported and skipped, never rebased.
    Paths(Vec<PathBuf>),
    /// Rebase every **linked** worktree of the repository that contains `base`
    /// (the process working directory). The main working tree is never included.
    All {
        /// The directory whose repository's linked worktrees are the target set.
        base: PathBuf,
    },
}

/// Knobs for a batch rebase.
#[derive(Debug, Clone, Default)]
pub struct RebaseOptions {
    /// Override the rebase target ref (default: the repository's remote default
    /// branch, e.g. `origin/main`). A `<remote>/<branch>` value is still fetched
    /// once up front; a local ref or raw commit is used as-is (no fetch).
    pub onto: Option<String>,
    /// Stash uncommitted changes before each rebase and restore them after, rather
    /// than skipping a dirty worktree.
    pub autostash: bool,
    /// Fetch and classify, but perform no rebase.
    pub dry_run: bool,
}

/// The result of planning and (optionally) executing a batch rebase.
#[derive(Debug, Clone, Serialize)]
pub struct Plan {
    /// One entry per repository: the fetch that was performed (or, for a local
    /// onto ref, skipped). The length is the fetch-once-per-repo count.
    pub fetches: Vec<FetchOutcome>,
    /// One entry per selected worktree, in selection order.
    pub worktrees: Vec<WorktreeOutcome>,
}

impl Plan {
    /// Whether any worktree still needs a rebase (a [`RebaseResult::WouldRebase`]).
    /// The CLI uses this to decide whether to confirm and execute.
    #[must_use]
    pub fn has_pending_rebases(&self) -> bool {
        self.worktrees
            .iter()
            .any(|w| matches!(w.result, RebaseResult::WouldRebase { .. }))
    }
}

/// The one-shot fetch performed for a single repository.
#[derive(Debug, Clone, Serialize)]
pub struct FetchOutcome {
    /// The main working-tree root of the repository (the fetch is run here).
    pub repo_root: PathBuf,
    /// The resolved onto ref this repository's worktrees rebase onto.
    pub onto: String,
    /// Whether a fetch was actually run (false for a local onto ref).
    pub fetched: bool,
    /// Whether the fetch succeeded (always true when `fetched` is false).
    pub ok: bool,
    /// The `git fetch` error, when it failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// What happened (or, in a dry run, would happen) to one worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorktreeOutcome {
    /// The worktree folder.
    pub path: PathBuf,
    /// The checked-out branch, when on one (absent for a detached/unborn HEAD).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The ref this worktree rebases onto (empty when it could not be resolved).
    pub onto: String,
    /// The classification / outcome.
    #[serde(flatten)]
    pub result: RebaseResult,
}

/// The per-worktree classification and outcome.
///
/// [`plan`](self::plan) only ever produces `WouldRebase` / `UpToDate` / `Skipped` /
/// `FetchFailed`; [`execute`](self::execute) turns each `WouldRebase` into `Rebased`
/// or `Conflict`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum RebaseResult {
    /// Rebased onto the fetched ref; it was `behind` commits behind beforehand.
    Rebased {
        /// Commits the worktree was behind the onto ref before the rebase.
        behind: usize,
    },
    /// Would be rebased (dry run); it is `behind` commits behind.
    WouldRebase {
        /// Commits the worktree is behind the onto ref.
        behind: usize,
    },
    /// Already on top of the onto ref — nothing to do.
    UpToDate,
    /// Skipped without touching the worktree, for a structural reason.
    Skipped {
        /// Why it was skipped.
        reason: SkipReason,
    },
    /// The rebase hit conflicts; it was aborted and the worktree left untouched.
    Conflict {
        /// The `git rebase` error output (trimmed).
        detail: String,
    },
    /// The repository's one-shot fetch failed, so no worktree of it was attempted.
    FetchFailed {
        /// The fetch error (trimmed).
        detail: String,
    },
}

/// Why a worktree was skipped rather than rebased.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum SkipReason {
    /// The main working tree — rebasing it is never the intent (structural
    /// `is_main`, not a branch-name heuristic).
    MainWorkingTree,
    /// A detached or unborn HEAD — there is no branch to rebase.
    DetachedHead,
    /// Uncommitted changes to tracked files (pass `--autostash` to rebase anyway).
    Dirty,
    /// A rebase/merge/cherry-pick is already in progress.
    OperationInProgress,
    /// The path is not a git worktree.
    NotAWorktree,
    /// The onto ref could not be resolved (e.g. no remote default branch).
    NoOntoRef,
}

/// Plans a batch rebase, fetching each repository's onto ref **exactly once**.
///
/// Enumerates the selected worktrees, resolves the onto ref and fetches it once per
/// repository, then classifies each worktree against the freshly fetched ref. The
/// returned [`Plan`] is exactly what a dry run reports; a real run passes it to
/// [`execute`].
///
/// The fetch runs even for a dry run: it is non-destructive (it only advances the
/// shared remote-tracking ref) and is what pins the single snapshot every worktree
/// is measured — and would be rebased — against.
pub fn plan(selection: &Selection, opts: &RebaseOptions) -> Result<Plan> {
    let paths = resolve_selection(selection)?;

    // Phase A — inspect each path with git2 (no network): structural facts + HEAD.
    let inspected: Vec<Inspected> = paths.iter().map(|p| Inspected::read(p)).collect();

    // Phase B — resolve the onto ref once per distinct repository.
    let onto_by_repo = resolve_onto_by_repo(&inspected, opts.onto.as_deref());

    // Phase C — fetch once per repository (the fetch-once-per-repo invariant).
    let (fetches, fetch_ok) = fetch_all(&onto_by_repo);

    // Phase D — classify each worktree against the now-fresh refs.
    let worktrees = inspected
        .iter()
        .map(|i| i.classify(&onto_by_repo, &fetch_ok, opts.autostash))
        .collect();

    Ok(Plan { fetches, worktrees })
}

/// Executes a [`Plan`], rebasing every [`RebaseResult::WouldRebase`] worktree.
///
/// The rest pass through unchanged. Rebases run sequentially (deterministic output;
/// no contention on the shared object database). A conflicting rebase is aborted so
/// the worktree is left exactly as it was.
#[must_use]
pub fn execute(plan: Plan, opts: &RebaseOptions) -> Vec<WorktreeOutcome> {
    plan.worktrees
        .into_iter()
        .map(|mut outcome| {
            if let RebaseResult::WouldRebase { behind } = outcome.result {
                outcome.result = match rebase_worktree(&outcome.path, &outcome.onto, opts.autostash)
                {
                    Ok(()) => RebaseResult::Rebased { behind },
                    Err(detail) => RebaseResult::Conflict { detail },
                };
            }
            outcome
        })
        .collect()
}

// ── selection ────────────────────────────────────────────────────────────────

/// The concrete worktree paths a [`Selection`] targets.
fn resolve_selection(selection: &Selection) -> Result<Vec<PathBuf>> {
    match selection {
        Selection::Paths(paths) => Ok(paths.clone()),
        Selection::All { base } => linked_worktree_paths(base),
    }
}

/// Every **linked** worktree path of the repository containing `base` (never the
/// main working tree). Mirrors the daemon service's repo enumeration: discover the
/// repo, resolve its shared common dir's parent as the main root, then list the
/// worktrees registered on the main repository.
fn linked_worktree_paths(base: &Path) -> Result<Vec<PathBuf>> {
    let repo = Repository::discover(base)
        .with_context(|| format!("not inside a git repository: {}", base.display()))?;
    let root = main_root(&repo);
    let main_repo = Repository::open(&root)
        .with_context(|| format!("cannot open main repository: {}", root.display()))?;
    let names = main_repo
        .worktrees()
        .context("cannot enumerate worktrees")?;
    let mut paths = Vec::new();
    // `iter()` yields `Result<Option<&str>, _>`: the first `flatten` drops per-name
    // errors, the second drops non-UTF-8 names (same idiom as the daemon service).
    for name in names.iter().flatten().flatten() {
        if let Ok(worktree) = main_repo.find_worktree(name) {
            paths.push(worktree.path().to_path_buf());
        }
    }
    Ok(paths)
}

// ── inspection (git2 reads) ──────────────────────────────────────────────────

/// A single selected path after its structural git2 inspection.
enum Inspected {
    /// The path resolved to a git worktree.
    Ok(Inspection),
    /// The path is not a git worktree (or does not exist).
    Unresolvable {
        /// The path, as given (best-effort canonicalized).
        path: PathBuf,
    },
}

/// The structural facts read from one worktree, independent of any fetch.
struct Inspection {
    path: PathBuf,
    repo_root: PathBuf,
    branch: Option<String>,
    head_oid: Option<Oid>,
    is_main: bool,
    state_clean: bool,
    dirty: bool,
}

impl Inspected {
    /// Inspects `path` with git2, degrading a non-worktree path to
    /// [`Inspected::Unresolvable`] rather than an error so one bad path never fails
    /// the whole batch.
    fn read(path: &Path) -> Self {
        let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        let Ok(repo) = Repository::discover(&canon) else {
            return Self::Unresolvable { path: canon };
        };
        let is_main = !repo.is_worktree();
        let repo_root = main_root(&repo);
        let (branch, head_oid) = head_branch(&repo);
        let state_clean = repo.state() == RepositoryState::Clean;
        let dirty = is_dirty(&repo);
        Self::Ok(Inspection {
            path: canon,
            repo_root,
            branch,
            head_oid,
            is_main,
            state_clean,
            dirty,
        })
    }

    /// The repository root, for the resolvable case (used to group fetches).
    fn repo_root(&self) -> Option<&Path> {
        match self {
            Self::Ok(i) => Some(&i.repo_root),
            Self::Unresolvable { .. } => None,
        }
    }

    /// Classifies this worktree against the resolved onto refs and fetch outcomes.
    fn classify(
        &self,
        onto_by_repo: &BTreeMap<PathBuf, OntoSpec>,
        fetch_ok: &BTreeMap<PathBuf, bool>,
        autostash: bool,
    ) -> WorktreeOutcome {
        let i = match self {
            Self::Unresolvable { path } => {
                return WorktreeOutcome::skipped(
                    path.clone(),
                    None,
                    String::new(),
                    SkipReason::NotAWorktree,
                );
            }
            Self::Ok(i) => i,
        };

        let onto = onto_by_repo.get(&i.repo_root);
        let onto_display = onto.map_or_else(String::new, |s| s.display.clone());
        let branch = i.branch.clone();
        let skip = |reason| {
            WorktreeOutcome::skipped(i.path.clone(), branch.clone(), onto_display.clone(), reason)
        };

        // Structural skips (independent of the fetch), safe-before-destructive order.
        if i.is_main {
            return skip(SkipReason::MainWorkingTree);
        }
        let (Some(head), Some(_)) = (i.head_oid, i.branch.as_ref()) else {
            return skip(SkipReason::DetachedHead);
        };
        if !i.state_clean {
            return skip(SkipReason::OperationInProgress);
        }
        if i.dirty && !autostash {
            return skip(SkipReason::Dirty);
        }
        let Some(onto) = onto else {
            return skip(SkipReason::NoOntoRef);
        };

        // The repository's single fetch must have succeeded.
        if fetch_ok.get(&i.repo_root) == Some(&false) {
            let detail = "the repository's fetch failed".to_string();
            return WorktreeOutcome {
                path: i.path.clone(),
                branch,
                onto: onto_display,
                result: RebaseResult::FetchFailed { detail },
            };
        }

        // Divergence against the freshly fetched ref.
        match behind_count(&i.repo_root, head, &onto.display) {
            None => skip(SkipReason::NoOntoRef),
            Some(0) => WorktreeOutcome {
                path: i.path.clone(),
                branch,
                onto: onto_display,
                result: RebaseResult::UpToDate,
            },
            Some(behind) => WorktreeOutcome {
                path: i.path.clone(),
                branch,
                onto: onto_display,
                result: RebaseResult::WouldRebase { behind },
            },
        }
    }
}

impl WorktreeOutcome {
    /// A `Skipped` outcome.
    fn skipped(path: PathBuf, branch: Option<String>, onto: String, reason: SkipReason) -> Self {
        Self {
            path,
            branch,
            onto,
            result: RebaseResult::Skipped { reason },
        }
    }
}

/// The main working-tree root of `repo`: the parent of its shared common dir. For a
/// linked worktree this is the original checkout every worktree shares.
fn main_root(repo: &Repository) -> PathBuf {
    let commondir = repo.commondir();
    let commondir = std::fs::canonicalize(commondir).unwrap_or_else(|_| commondir.to_path_buf());
    let parent = commondir.parent().map(Path::to_path_buf);
    parent.unwrap_or(commondir)
}

/// The checked-out branch shorthand and HEAD oid. Both are `None` for a detached or
/// unborn HEAD (no branch to rebase).
fn head_branch(repo: &Repository) -> (Option<String>, Option<Oid>) {
    match repo.head() {
        Ok(head) if head.is_branch() => (
            head.shorthand().ok().map(ToString::to_string),
            head.target(),
        ),
        Ok(head) => (None, head.target()),
        Err(_) => (None, None),
    }
}

/// Whether the worktree has uncommitted changes to **tracked** files (staged or
/// unstaged). Untracked and ignored files do not block a rebase, so they are
/// excluded.
fn is_dirty(repo: &Repository) -> bool {
    let mut opts = StatusOptions::new();
    opts.include_untracked(false)
        .include_ignored(false)
        .exclude_submodules(true);
    repo.statuses(Some(&mut opts))
        .is_ok_and(|statuses| !statuses.is_empty())
}

/// Commits `onto` is ahead of `head` (i.e. how far `head` is behind `onto`), or
/// `None` when `onto` does not resolve to a commit in the repository at `repo_root`.
fn behind_count(repo_root: &Path, head: Oid, onto: &str) -> Option<usize> {
    let repo = Repository::open(repo_root).ok()?;
    let onto_oid = repo.revparse_single(onto).ok()?.peel_to_commit().ok()?.id();
    let (_ahead, behind) = repo.graph_ahead_behind(head, onto_oid).ok()?;
    Some(behind)
}

// ── onto resolution ──────────────────────────────────────────────────────────

/// The rebase target for one repository.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OntoSpec {
    /// The git revspec every worktree of this repo rebases onto (e.g. `origin/main`).
    display: String,
    /// `Some((remote, branch))` when `display` is a remote-tracking ref to fetch
    /// once up front; `None` for a local ref / raw commit (no fetch).
    fetch: Option<(String, String)>,
}

/// Resolves the onto ref for every distinct repository among the resolvable
/// inspections. The map's one-entry-per-root shape **is** the fetch-once-per-repo
/// grouping: N worktrees of one repo collapse to a single entry.
fn resolve_onto_by_repo(
    inspected: &[Inspected],
    override_ref: Option<&str>,
) -> BTreeMap<PathBuf, OntoSpec> {
    let mut map: BTreeMap<PathBuf, OntoSpec> = BTreeMap::new();
    for root in inspected.iter().filter_map(Inspected::repo_root) {
        if map.contains_key(root) {
            continue;
        }
        if let Ok(repo) = Repository::open(root) {
            map.insert(root.to_path_buf(), resolve_onto(&repo, override_ref));
        }
    }
    map
}

/// The onto spec for a single repository: the `--onto` override if given, else the
/// remote (`origin`) default branch resolved from local refs.
fn resolve_onto(repo: &Repository, override_ref: Option<&str>) -> OntoSpec {
    if let Some(reference) = override_ref {
        return onto_from_override(repo, reference);
    }
    let remote = "origin";
    let branch =
        RemoteInfo::detect_main_branch_local(repo, remote).unwrap_or_else(|| "main".to_string());
    OntoSpec {
        display: format!("{remote}/{branch}"),
        fetch: Some((remote.to_string(), branch)),
    }
}

/// Interprets an explicit `--onto` value: a `<remote>/<branch>` whose first segment
/// is a configured remote is fetched once; anything else (a local branch, a raw
/// commit) is used verbatim with no fetch.
fn onto_from_override(repo: &Repository, reference: &str) -> OntoSpec {
    if let Some((remote, branch)) = reference.split_once('/') {
        if repo.find_remote(remote).is_ok() {
            return OntoSpec {
                display: reference.to_string(),
                fetch: Some((remote.to_string(), branch.to_string())),
            };
        }
    }
    OntoSpec {
        display: reference.to_string(),
        fetch: None,
    }
}

// ── fetch (shell-out, once per repo) ─────────────────────────────────────────

/// Fetches each repository's onto ref once, returning the per-repo outcomes and a
/// `root -> ok` map the classifier consults. A repo with a local onto ref records a
/// `fetched: false, ok: true` entry.
fn fetch_all(
    onto_by_repo: &BTreeMap<PathBuf, OntoSpec>,
) -> (Vec<FetchOutcome>, BTreeMap<PathBuf, bool>) {
    let mut fetches = Vec::new();
    let mut fetch_ok = BTreeMap::new();
    for (root, spec) in onto_by_repo {
        let outcome = match &spec.fetch {
            Some((remote, branch)) => {
                let result = fetch_once(root, remote, branch);
                let ok = result.is_ok();
                FetchOutcome {
                    repo_root: root.clone(),
                    onto: spec.display.clone(),
                    fetched: true,
                    ok,
                    detail: result.err().map(|e| e.to_string()),
                }
            }
            None => FetchOutcome {
                repo_root: root.clone(),
                onto: spec.display.clone(),
                fetched: false,
                ok: true,
                detail: None,
            },
        };
        fetch_ok.insert(root.clone(), outcome.ok);
        fetches.push(outcome);
    }
    (fetches, fetch_ok)
}

/// Runs `git fetch <remote> <branch>` once in `repo_root`. The shared object
/// database means this single fetch updates the tracking ref every worktree sees.
fn fetch_once(repo_root: &Path, remote: &str, branch: &str) -> Result<()> {
    let output = run_git_in(repo_root, &["fetch", remote, branch])?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "git fetch {remote} {branch} failed: {}",
        trimmed_stderr(&output)
    )
}

// ── rebase (shell-out, per worktree) ─────────────────────────────────────────

/// Rebases the branch checked out in `path` onto `onto`. On failure (a conflict, or
/// anything else) the rebase is aborted so the worktree is left exactly as it was,
/// and the trimmed error is returned. With `autostash`, `git rebase --abort` also
/// restores the stashed changes.
fn rebase_worktree(path: &Path, onto: &str, autostash: bool) -> std::result::Result<(), String> {
    let args = rebase_args(onto, autostash);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match run_git_in(path, &argv) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let detail = trimmed_stderr(&output);
            // Best-effort abort; harmlessly errors if no rebase is in progress.
            let _ = run_git_in(path, &["rebase", "--abort"]);
            Err(detail)
        }
        Err(err) => Err(err.to_string()),
    }
}

/// The `git rebase` argument vector, with `--autostash` inserted when requested.
/// Pure, so the argument shape is unit-testable.
fn rebase_args(onto: &str, autostash: bool) -> Vec<String> {
    let mut args = vec!["rebase".to_string()];
    if autostash {
        args.push("--autostash".to_string());
    }
    args.push(onto.to_string());
    args
}

// ── git subprocess seam ──────────────────────────────────────────────────────

/// Runs `git <args>` in `dir`, capturing its output.
///
/// The child receives a snapshot of the current environment (`env_clear` + `envs`)
/// so the spawn stays out of the data race against concurrent `std::env::set_var`
/// (issue #1022; same idiom as `crate::cli::git::worktree`). Shelling out to the
/// user's `git` — rather than libgit2's network transport — is deliberate: it works
/// across SSH/HTTPS and honours the user's authentication configuration (ADR-0003,
/// issue #903).
fn run_git_in(dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.env_clear();
    cmd.envs(std::env::vars_os());
    cmd.current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute git in {}", dir.display()))
}

/// The trimmed stderr of a git subprocess (falling back to stdout when stderr is
/// empty), for a single-line error message.
fn trimmed_stderr(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// A process-wide lock serializing the git-subprocess-heavy tests, shared across
/// modules (this module's tests and the `worktrees rebase` CLI test).
///
/// Each such test builds several repos by shelling out to `git`; run in parallel
/// across the whole suite they burst dozens of processes at once, starving
/// unrelated timing-sensitive tests (the daemon PR-poll debounce test). Holding one
/// lock caps the combined concurrent `git` load at a single scenario, which keeps
/// coverage without destabilising the suite. Poison is ignored — a panicking test
/// still releases the guard's exclusion.
#[cfg(test)]
pub(crate) fn test_serial_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The shared git-load serialization guard (see [`super::test_serial_lock`]).
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        super::test_serial_lock()
    }

    // ── pure helpers ──────────────────────────────────────────────────────

    #[test]
    fn rebase_args_omits_autostash_by_default() {
        assert_eq!(
            rebase_args("origin/main", false),
            vec!["rebase", "origin/main"]
        );
    }

    #[test]
    fn rebase_args_inserts_autostash_before_the_ref() {
        assert_eq!(
            rebase_args("origin/main", true),
            vec!["rebase", "--autostash", "origin/main"]
        );
    }

    #[test]
    fn onto_from_override_fetches_a_remote_tracking_ref() {
        let (_dir, repo) = repo_with_origin();
        let spec = onto_from_override(&repo, "origin/release");
        assert_eq!(spec.display, "origin/release");
        assert_eq!(
            spec.fetch,
            Some(("origin".to_string(), "release".to_string()))
        );
    }

    #[test]
    fn onto_from_override_keeps_a_multi_segment_branch_whole() {
        let (_dir, repo) = repo_with_origin();
        let spec = onto_from_override(&repo, "origin/feature/foo");
        assert_eq!(
            spec.fetch,
            Some(("origin".to_string(), "feature/foo".to_string()))
        );
    }

    #[test]
    fn onto_from_override_does_not_fetch_a_local_ref() {
        let (_dir, repo) = repo_with_origin();
        // `develop` has no `/`, and `upstream/x`'s first segment is not a remote.
        assert_eq!(onto_from_override(&repo, "develop").fetch, None);
        assert_eq!(onto_from_override(&repo, "upstream/x").fetch, None);
        assert_eq!(onto_from_override(&repo, "HEAD~2").fetch, None);
    }

    #[test]
    fn resolve_onto_defaults_to_origin_main() {
        let (_dir, repo) = repo_with_origin();
        let spec = resolve_onto(&repo, None);
        assert_eq!(spec.display, "origin/main");
        assert_eq!(spec.fetch, Some(("origin".to_string(), "main".to_string())));
    }

    // ── the fetch-once-per-repo invariant ─────────────────────────────────

    #[test]
    fn one_repo_with_many_worktrees_fetches_exactly_once() {
        // Three linked worktrees sharing one repository must yield a single fetch
        // entry — the whole point of #1400.
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.add_worktree("feature-a");
        scenario.add_worktree("feature-b");
        scenario.add_worktree("feature-c");

        let plan = plan(
            &Selection::All {
                base: scenario.local,
            },
            &RebaseOptions::default(),
        )
        .unwrap();

        assert_eq!(
            plan.fetches.len(),
            1,
            "fetch must run once per repo, not per worktree"
        );
        assert_eq!(plan.worktrees.len(), 3);
        assert!(plan.fetches[0].ok);
    }

    #[test]
    fn resolve_onto_by_repo_collapses_worktrees_of_one_repo() {
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.add_worktree("feature-a");
        scenario.add_worktree("feature-b");
        let paths = linked_worktree_paths(&scenario.local).unwrap();
        let inspected: Vec<Inspected> = paths.iter().map(|p| Inspected::read(p)).collect();
        let map = resolve_onto_by_repo(&inspected, None);
        assert_eq!(
            map.len(),
            1,
            "two worktrees of one repo resolve to one onto entry"
        );
    }

    // ── end-to-end classify + execute ─────────────────────────────────────

    #[test]
    fn behind_worktree_is_rebased_onto_the_fetched_ref() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature");
        // Advance origin/main by one commit from a separate clone, so the local
        // tracking ref only learns of it on fetch.
        scenario.advance_origin_main("second\n");

        let plan = plan(
            &Selection::Paths(vec![wt.clone()]),
            &RebaseOptions::default(),
        )
        .unwrap();
        assert_eq!(plan.worktrees.len(), 1);
        assert_eq!(
            plan.worktrees[0].result,
            RebaseResult::WouldRebase { behind: 1 },
            "the feature worktree is one commit behind the fetched origin/main"
        );

        let outcomes = execute(plan, &RebaseOptions::default());
        assert_eq!(outcomes[0].result, RebaseResult::Rebased { behind: 1 });
        // The worktree is clean and its branch now contains origin/main's commit.
        assert!(head_contains(&wt, &scenario.origin_main_oid()));
    }

    #[test]
    fn up_to_date_worktree_is_not_rebased() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature");
        // No advance: feature sits on origin/main already.
        let plan = plan(&Selection::Paths(vec![wt]), &RebaseOptions::default()).unwrap();
        assert_eq!(plan.worktrees[0].result, RebaseResult::UpToDate);
        assert!(!plan.has_pending_rebases());
    }

    #[test]
    fn dirty_worktree_is_skipped_but_autostash_rebases_it() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature");
        scenario.advance_origin_main("second\n");
        // Dirty a file `origin/main` does not touch, so the autostash pop is clean.
        std::fs::write(wt.join("keep.txt"), "dirty change\n").unwrap();

        let skipped = plan(
            &Selection::Paths(vec![wt.clone()]),
            &RebaseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            skipped.worktrees[0].result,
            RebaseResult::Skipped {
                reason: SkipReason::Dirty
            }
        );

        let opts = RebaseOptions {
            autostash: true,
            ..RebaseOptions::default()
        };
        let planned = plan(&Selection::Paths(vec![wt.clone()]), &opts).unwrap();
        assert_eq!(
            planned.worktrees[0].result,
            RebaseResult::WouldRebase { behind: 1 }
        );
        let outcomes = execute(planned, &opts);
        assert_eq!(outcomes[0].result, RebaseResult::Rebased { behind: 1 });
        // Autostash restored the local edit on top of the rebased branch.
        assert_eq!(
            std::fs::read_to_string(wt.join("keep.txt")).unwrap(),
            "dirty change\n"
        );
    }

    #[test]
    fn main_working_tree_is_skipped() {
        let _guard = serial();
        let scenario = Scenario::new();
        let plan = plan(
            &Selection::Paths(vec![scenario.local]),
            &RebaseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            plan.worktrees[0].result,
            RebaseResult::Skipped {
                reason: SkipReason::MainWorkingTree
            }
        );
    }

    #[test]
    fn non_worktree_path_is_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan(
            &Selection::Paths(vec![dir.path().to_path_buf()]),
            &RebaseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            plan.worktrees[0].result,
            RebaseResult::Skipped {
                reason: SkipReason::NotAWorktree
            }
        );
    }

    #[test]
    fn conflicting_rebase_aborts_and_leaves_the_worktree_untouched() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature");
        // Feature edits file.txt; origin/main edits the same line differently.
        scenario.commit_in_worktree(&wt, "file.txt", "feature side\n", "feature edit");
        scenario.advance_origin_main("main side\n");
        let head_before = head_oid(&wt);

        let plan = plan(
            &Selection::Paths(vec![wt.clone()]),
            &RebaseOptions::default(),
        )
        .unwrap();
        assert!(matches!(
            plan.worktrees[0].result,
            RebaseResult::WouldRebase { .. }
        ));
        let outcomes = execute(plan, &RebaseOptions::default());
        assert!(
            matches!(outcomes[0].result, RebaseResult::Conflict { .. }),
            "a conflicting rebase is reported, not silently half-applied"
        );
        // Aborted: HEAD unchanged and no rebase left in progress.
        assert_eq!(head_oid(&wt), head_before);
        let repo = Repository::open(&wt).unwrap();
        assert_eq!(repo.state(), RepositoryState::Clean);
    }

    #[test]
    fn dry_run_fetches_but_rebases_nothing() {
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature");
        scenario.advance_origin_main("second\n");
        let head_before = head_oid(&wt);

        let opts = RebaseOptions {
            dry_run: true,
            ..RebaseOptions::default()
        };
        let plan = plan(&Selection::Paths(vec![wt.clone()]), &opts).unwrap();
        // Planned as would-rebase, and the fetch did happen (tracking ref advanced),
        // but we do not call execute in a dry run.
        assert_eq!(
            plan.worktrees[0].result,
            RebaseResult::WouldRebase { behind: 1 }
        );
        assert_eq!(plan.fetches.len(), 1);
        assert!(plan.fetches[0].fetched && plan.fetches[0].ok);
        assert_eq!(
            head_oid(&wt),
            head_before,
            "dry run must not move the branch"
        );
    }

    #[test]
    fn json_shape_is_kebab_tagged() {
        let outcome = WorktreeOutcome {
            path: PathBuf::from("/wt"),
            branch: Some("feature".to_string()),
            onto: "origin/main".to_string(),
            result: RebaseResult::Skipped {
                reason: SkipReason::Dirty,
            },
        };
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(value["status"], "skipped");
        assert_eq!(value["reason"], "dirty");
        assert_eq!(value["onto"], "origin/main");
    }

    // ── test scaffolding ──────────────────────────────────────────────────

    /// A repo with a bare `origin` (so `resolve_onto` sees a real remote) and one
    /// commit on `main`; no worktrees yet.
    fn repo_with_origin() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        config_identity(&repo);
        repo.remote("origin", "https://example.invalid/x.git")
            .unwrap();
        let oid = empty_commit(&repo, "refs/heads/main", &[]);
        repo.reference("refs/remotes/origin/main", oid, true, "seed")
            .unwrap();
        (dir, repo)
    }

    /// An `origin` bare repo, a `local` clone with `main` pushed, and helpers to add
    /// worktrees and advance `origin/main` out-of-band (as a second clone would).
    struct Scenario {
        root: tempfile::TempDir,
        origin: PathBuf,
        local: PathBuf,
    }

    impl Scenario {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let origin = root.path().join("origin.git");
            let local = root.path().join("local");
            std::fs::create_dir_all(&origin).unwrap();
            std::fs::create_dir_all(&local).unwrap();
            git(&origin, &["init", "--bare", "-b", "main"]);
            git(&local, &["init", "-b", "main"]);
            config_repo(&local, "Test", "test@example.com");
            std::fs::write(local.join("file.txt"), "first\n").unwrap();
            // A second tracked file that `origin/main` never touches, so a dirty
            // edit to it stashes/pops cleanly across a rebase (no false conflict).
            std::fs::write(local.join("keep.txt"), "keep\n").unwrap();
            git(&local, &["add", "file.txt", "keep.txt"]);
            git(&local, &["commit", "-m", "first"]);
            git(
                &local,
                &["remote", "add", "origin", origin.to_str().unwrap()],
            );
            git(&local, &["push", "-u", "origin", "main"]);
            Self {
                root,
                origin,
                local,
            }
        }

        /// Adds a linked worktree branched off the current `main` and returns its path.
        fn add_worktree(&self, name: &str) -> PathBuf {
            let path = self.root.path().join(name);
            git(
                &self.local,
                &[
                    "worktree",
                    "add",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                    "main",
                ],
            );
            path
        }

        /// Advances `origin/main` by one commit that changes `file.txt`, writing
        /// directly into the bare origin's object database with `git2` — the
        /// `local` repo only learns of it on fetch.
        ///
        /// Done in-process (no `git clone`/subprocess) so the git-heavy test suite
        /// stays light enough not to starve unrelated timing-sensitive tests.
        fn advance_origin_main(&self, content: &str) {
            let repo = Repository::open_bare(&self.origin).unwrap();
            let parent = repo
                .find_commit(repo.refname_to_id("refs/heads/main").unwrap())
                .unwrap();
            // Seed the tree from the parent so `keep.txt` survives; only `file.txt`
            // changes (so a worktree that also edited `file.txt` conflicts).
            let mut builder = repo.treebuilder(Some(&parent.tree().unwrap())).unwrap();
            let blob = repo.blob(content.as_bytes()).unwrap();
            builder.insert("file.txt", blob, 0o100_644).unwrap();
            let tree = repo.find_tree(builder.write().unwrap()).unwrap();
            let sig = git2::Signature::now("Other", "other@example.com").unwrap();
            repo.commit(
                Some("refs/heads/main"),
                &sig,
                &sig,
                "advance",
                &tree,
                &[&parent],
            )
            .unwrap();
        }

        /// Commits a change inside a worktree (to set up a conflict).
        fn commit_in_worktree(&self, wt: &Path, file: &str, content: &str, msg: &str) {
            std::fs::write(wt.join(file), content).unwrap();
            git(wt, &["add", file]);
            git(wt, &["commit", "-m", msg]);
        }

        /// The current tip oid of `origin/main` on the server.
        fn origin_main_oid(&self) -> Oid {
            let repo = Repository::open_bare(&self.origin).unwrap();
            repo.refname_to_id("refs/heads/main").unwrap()
        }
    }

    /// Pins a test repo's identity and, crucially, **disables commit signing**.
    ///
    /// Test repos otherwise inherit the developer's global git config; a global
    /// `commit.gpgsign = true` makes every commit shell out to gpg, which fails
    /// under the parallel test suite ("gpg: signing failed: Cannot allocate
    /// memory"). Repo-local config wins over global, and worktrees share the main
    /// repo's config file — so this also covers the commits the production
    /// `git rebase` creates.
    fn config_repo(dir: &Path, name: &str, email: &str) {
        git(dir, &["config", "user.name", name]);
        git(dir, &["config", "user.email", email]);
        git(dir, &["config", "commit.gpgsign", "false"]);
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = run_git_in(dir, args).unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn config_identity(repo: &Repository) {
        let mut cfg = repo.config().unwrap();
        cfg.set_str("user.name", "Test").unwrap();
        cfg.set_str("user.email", "test@example.com").unwrap();
    }

    fn empty_commit(repo: &Repository, refname: &str, parents: &[&git2::Commit<'_>]) -> Oid {
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(Some(refname), &sig, &sig, "seed", &tree, parents)
            .unwrap()
    }

    fn head_oid(wt: &Path) -> Oid {
        let repo = Repository::open(wt).unwrap();
        let head = repo.head().unwrap();
        head.target().unwrap()
    }

    fn head_contains(wt: &Path, oid: &Oid) -> bool {
        let repo = Repository::open(wt).unwrap();
        let head = repo.head().unwrap().target().unwrap();
        repo.graph_descendant_of(head, *oid).unwrap_or(false) || head == *oid
    }
}
