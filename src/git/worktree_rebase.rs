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
//! handling, hooks, and `--autostash`.
//!
//! **Where it runs (ADR-0059, superseding ADR-0055 in part).** This engine is host
//! agnostic: it drives both the local CLI (`omni-dev worktrees rebase`) and the
//! daemon's two-phase `rebase` op. ADR-0055 originally confined it to the CLI on
//! the premise that the daemon's minimal environment lacked `SSH_AUTH_SOCK` and so
//! could not authenticate a fetch. That premise was wrong — launchd exports
//! `SSH_AUTH_SOCK` into the per-user session, so a LaunchAgent inherits the user's
//! `ssh-agent`. What the daemon genuinely lacks is a useful `PATH`, which is why
//! the `git` binary is resolved through
//! [`crate::git::resolve_git_binary`] rather than by name.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use git2::{Oid, Repository, RepositoryState, StatusOptions};
use serde::Serialize;

use crate::git::remote::RemoteInfo;
use crate::git::resolve_git_binary;
use crate::git::worktree_batch::{
    head_branch, is_false, main_root, resolve_selection, run_git_in, trimmed_stderr,
};

pub use crate::git::worktree_batch::Selection;

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
    /// Leave a conflicting worktree **mid-rebase** instead of `git rebase
    /// --abort`-ing it (#1415).
    ///
    /// The default (abort) keeps the worktree exactly as it was, which is the
    /// right conservative choice for a batch the user is watching scroll past. But
    /// it also throws away every conflict already resolved by `git rerere` and
    /// every hunk git applied cleanly before the collision — work the user then has
    /// to reproduce by hand. With this set, the worktree stays in its conflicted
    /// state so the conflict can be resolved in place and finished with
    /// `git rebase --continue`, and the batch moves on to the next worktree
    /// regardless.
    pub keep_conflicts: bool,
    /// The `git` executable to shell out to. `None` resolves it via
    /// [`resolve_git_binary`], which is what a caller with a minimal `PATH` (the
    /// daemon) needs; the field exists so a caller can resolve **once** and reuse,
    /// and so a test can point the engine at a stub.
    pub git_bin: Option<PathBuf>,
}

impl RebaseOptions {
    /// The `git` executable these options select, resolving the default lazily.
    /// Called once per [`plan`] / [`execute`] rather than per subprocess.
    fn git_bin(&self) -> PathBuf {
        self.git_bin.clone().unwrap_or_else(resolve_git_binary)
    }
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
/// [`plan`] only ever produces `WouldRebase` / `UpToDate` / `Skipped` /
/// `FetchFailed`; [`execute`] turns each `WouldRebase` into `Rebased`
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
    /// The rebase hit conflicts. By default it was aborted and the worktree left
    /// untouched; with [`RebaseOptions::keep_conflicts`] the worktree is instead
    /// left mid-rebase, which `left_in_place` records.
    Conflict {
        /// The `git rebase` error output (trimmed).
        detail: String,
        /// Whether the worktree was **left mid-rebase** for the user to resolve
        /// (`true`) rather than aborted back to its previous state (`false`).
        /// Omitted on the wire when false, so a pre-#1415 client sees the exact
        /// bytes it saw before.
        #[serde(skip_serializing_if = "is_false")]
        left_in_place: bool,
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
    // Resolved once for the whole plan, not per subprocess: the probe stats the
    // candidate paths, and the answer is process-stable.
    let git = opts.git_bin();

    // Phase A — inspect each path with git2 (no network): structural facts + HEAD.
    let inspected: Vec<Inspected> = paths.iter().map(|p| Inspected::read(p)).collect();

    // Phase B — resolve the onto ref once per distinct repository.
    let onto_by_repo = resolve_onto_by_repo(&inspected, opts.onto.as_deref());

    // Phase C — fetch once per repository (the fetch-once-per-repo invariant).
    let (fetches, fetch_ok) = fetch_all(&git, &onto_by_repo);

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
/// the worktree is left exactly as it was — unless
/// [`RebaseOptions::keep_conflicts`] is set, in which case it is left mid-rebase.
/// Either way the batch continues with the remaining worktrees.
#[must_use]
pub fn execute(plan: Plan, opts: &RebaseOptions) -> Vec<WorktreeOutcome> {
    let git = opts.git_bin();
    plan.worktrees
        .into_iter()
        .map(|mut outcome| {
            if let RebaseResult::WouldRebase { behind } = outcome.result {
                outcome.result = match rebase_worktree(&git, &outcome.path, &outcome.onto, opts) {
                    Ok(()) => RebaseResult::Rebased { behind },
                    Err(detail) => RebaseResult::Conflict {
                        detail,
                        left_in_place: opts.keep_conflicts,
                    },
                };
            }
            outcome
        })
        .collect()
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

/// The structural facts read from one worktree, independent of any fetch. Whether
/// this is the main working tree is deliberately not among them (ADR-0060) — the
/// rebase engine no longer distinguishes it from a linked worktree.
struct Inspection {
    path: PathBuf,
    repo_root: PathBuf,
    branch: Option<String>,
    head_oid: Option<Oid>,
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
        let repo_root = main_root(&repo);
        let (branch, head_oid) = head_branch(&repo);
        let state_clean = repo.state() == RepositoryState::Clean;
        let dirty = is_dirty(&repo);
        Self::Ok(Inspection {
            path: canon,
            repo_root,
            branch,
            head_oid,
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
        // The main working tree is not exempted here (ADR-0060) — it is classified
        // exactly like any other worktree.
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
    git: &Path,
    onto_by_repo: &BTreeMap<PathBuf, OntoSpec>,
) -> (Vec<FetchOutcome>, BTreeMap<PathBuf, bool>) {
    let mut fetches = Vec::new();
    let mut fetch_ok = BTreeMap::new();
    for (root, spec) in onto_by_repo {
        let outcome = match &spec.fetch {
            Some((remote, branch)) => {
                let result = fetch_once(git, root, remote, branch);
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
fn fetch_once(git: &Path, repo_root: &Path, remote: &str, branch: &str) -> Result<()> {
    let output = run_git_in(git, repo_root, &["fetch", remote, branch])?;
    if output.status.success() {
        return Ok(());
    }
    bail!(
        "git fetch {remote} {branch} failed: {}",
        trimmed_stderr(&output)
    )
}

// ── rebase (shell-out, per worktree) ─────────────────────────────────────────

/// Rebases the branch checked out in `path` onto `onto`, returning the trimmed
/// error on failure (a conflict, or anything else).
///
/// By default the rebase is aborted on failure so the worktree is left exactly as
/// it was; with `autostash`, `git rebase --abort` also restores the stashed
/// changes. With [`RebaseOptions::keep_conflicts`] the abort is **skipped** and the
/// worktree stays mid-rebase for in-place resolution — including any autostash
/// entry, which git re-applies when the rebase eventually concludes (via
/// `--continue` or a later `--abort`), exactly as for a hand-run rebase.
fn rebase_worktree(
    git: &Path,
    path: &Path,
    onto: &str,
    opts: &RebaseOptions,
) -> std::result::Result<(), String> {
    let args = rebase_args(onto, opts.autostash);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match run_git_in(git, path, &argv) {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let detail = trimmed_stderr(&output);
            if !opts.keep_conflicts {
                // Best-effort abort; harmlessly errors if no rebase is in progress.
                let _ = run_git_in(git, path, &["rebase", "--abort"]);
            }
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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    use crate::git::worktree_batch::all_worktree_paths;

    /// The shared git-load serialization guard (see
    /// [`crate::git::worktree_batch::test_serial_lock`]).
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        crate::git::worktree_batch::test_serial_lock()
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
        // The main tree plus three linked worktrees sharing one repository must
        // yield a single fetch entry — the whole point of #1400.
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.add_worktree("feature-a");
        scenario.add_worktree("feature-b");
        scenario.add_worktree("feature-c");

        let plan = plan(
            &Selection::All {
                base: scenario.local.clone(),
            },
            &RebaseOptions::default(),
        )
        .unwrap();

        assert_eq!(
            plan.fetches.len(),
            1,
            "fetch must run once per repo, not per worktree"
        );
        assert_eq!(
            plan.worktrees.len(),
            4,
            "--all now includes the main working tree alongside its three linked \
             worktrees (#1438)"
        );
        let main_canon = std::fs::canonicalize(&scenario.local).unwrap();
        assert!(plan.worktrees.iter().any(|w| w.path == main_canon));
        assert!(plan.fetches[0].ok);
    }

    #[test]
    fn all_worktree_paths_includes_the_main_working_tree() {
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.add_worktree("feature-a");
        let paths = all_worktree_paths(&scenario.local).unwrap();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&std::fs::canonicalize(&scenario.local).unwrap()));
    }

    #[test]
    fn resolve_onto_by_repo_collapses_worktrees_of_one_repo() {
        let _guard = serial();
        let scenario = Scenario::new();
        scenario.add_worktree("feature-a");
        scenario.add_worktree("feature-b");
        let paths = all_worktree_paths(&scenario.local).unwrap();
        let inspected: Vec<Inspected> = paths.iter().map(|p| Inspected::read(p)).collect();
        let map = resolve_onto_by_repo(&inspected, None);
        assert_eq!(
            map.len(),
            1,
            "the main tree and two linked worktrees of one repo resolve to one onto \
             entry"
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
    fn main_working_tree_is_rebased_like_any_worktree() {
        let _guard = serial();
        let scenario = Scenario::new();
        // Advance origin/main by one commit, so the main tree is behind on fetch.
        scenario.advance_origin_main("second\n");

        let plan = plan(
            &Selection::Paths(vec![scenario.local.clone()]),
            &RebaseOptions::default(),
        )
        .unwrap();
        assert_eq!(
            plan.worktrees[0].result,
            RebaseResult::WouldRebase { behind: 1 },
            "the main working tree is a valid rebase target like any other (#1438)"
        );

        let outcomes = execute(plan, &RebaseOptions::default());
        assert_eq!(outcomes[0].result, RebaseResult::Rebased { behind: 1 });
        assert!(head_contains(&scenario.local, &scenario.origin_main_oid()));
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
            matches!(
                outcomes[0].result,
                RebaseResult::Conflict {
                    left_in_place: false,
                    ..
                }
            ),
            "a conflicting rebase is reported, not silently half-applied"
        );
        // Aborted: HEAD unchanged and no rebase left in progress.
        assert_eq!(head_oid(&wt), head_before);
        let repo = Repository::open(&wt).unwrap();
        assert_eq!(repo.state(), RepositoryState::Clean);
    }

    #[test]
    fn keep_conflicts_leaves_the_worktree_mid_rebase() {
        // The inverse of the test above (#1415): the conflicted worktree must be
        // left in its conflicted state so the user can resolve it in place, rather
        // than aborted back to where it started.
        let _guard = serial();
        let scenario = Scenario::new();
        let wt = scenario.add_worktree("feature");
        scenario.commit_in_worktree(&wt, "file.txt", "feature side\n", "feature edit");
        scenario.advance_origin_main("main side\n");

        let opts = RebaseOptions {
            keep_conflicts: true,
            ..RebaseOptions::default()
        };
        let plan = plan(&Selection::Paths(vec![wt.clone()]), &opts).unwrap();
        let outcomes = execute(plan, &opts);
        assert!(
            matches!(
                outcomes[0].result,
                RebaseResult::Conflict {
                    left_in_place: true,
                    ..
                }
            ),
            "the outcome records that the worktree was left mid-rebase"
        );
        // The load-bearing assertion: a rebase really is still in progress, which
        // is what makes `git rebase --continue` (and the tree's cue) meaningful.
        let repo = Repository::open(&wt).unwrap();
        assert_ne!(
            repo.state(),
            RepositoryState::Clean,
            "the worktree must still be mid-rebase, not aborted back to clean"
        );
        // And the conflict markers are on disk for the user to resolve.
        let conflicted = std::fs::read_to_string(wt.join("file.txt")).unwrap();
        assert!(
            conflicted.contains("<<<<<<<"),
            "expected conflict markers, got: {conflicted}"
        );
    }

    #[test]
    fn a_kept_conflict_does_not_stop_the_rest_of_the_batch() {
        // A conflicting worktree left in place must not sink its siblings: the
        // batch continues, and the next worktree still rebases.
        let _guard = serial();
        let scenario = Scenario::new();
        let clashing = scenario.add_worktree("clashing");
        let clean = scenario.add_worktree("clean");
        scenario.commit_in_worktree(&clashing, "file.txt", "feature side\n", "feature edit");
        scenario.advance_origin_main("main side\n");

        let opts = RebaseOptions {
            keep_conflicts: true,
            ..RebaseOptions::default()
        };
        let plan = plan(&Selection::Paths(vec![clashing, clean.clone()]), &opts).unwrap();
        let outcomes = execute(plan, &opts);
        assert!(matches!(
            outcomes[0].result,
            RebaseResult::Conflict {
                left_in_place: true,
                ..
            }
        ));
        assert_eq!(
            outcomes[1].result,
            RebaseResult::Rebased { behind: 1 },
            "the second worktree rebases despite the first being left conflicted"
        );
        assert!(head_contains(&clean, &scenario.origin_main_oid()));
    }

    #[test]
    fn left_in_place_is_omitted_from_json_when_false() {
        // Forward-compatibility: an aborted conflict must serialize byte-identically
        // to the pre-#1415 shape, so an older client is unaffected.
        let aborted = serde_json::to_value(RebaseResult::Conflict {
            detail: "boom".to_string(),
            left_in_place: false,
        })
        .unwrap();
        assert_eq!(aborted["status"], "conflict");
        assert!(aborted.get("left_in_place").is_none());

        let kept = serde_json::to_value(RebaseResult::Conflict {
            detail: "boom".to_string(),
            left_in_place: true,
        })
        .unwrap();
        assert_eq!(kept["left_in_place"], true);
    }

    #[test]
    fn git_bin_defaults_to_the_resolver_and_honours_an_override() {
        assert_eq!(
            RebaseOptions::default().git_bin(),
            crate::git::resolve_git_binary(),
            "an unset git_bin falls back to the shared resolver"
        );
        let opts = RebaseOptions {
            git_bin: Some(PathBuf::from("/custom/git")),
            ..RebaseOptions::default()
        };
        assert_eq!(opts.git_bin(), PathBuf::from("/custom/git"));
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

    // ── classify branches (pure — no git, no subprocess) ──────────────────

    /// An `Inspected::Ok` with a fake (never-opened) repo root, for exercising the
    /// classification branches that return before any repo is opened.
    fn inspected(
        branch: Option<&str>,
        head: Option<Oid>,
        state_clean: bool,
        dirty: bool,
    ) -> Inspected {
        Inspected::Ok(Inspection {
            path: PathBuf::from("/wt"),
            repo_root: PathBuf::from("/repo"),
            branch: branch.map(str::to_string),
            head_oid: head,
            state_clean,
            dirty,
        })
    }

    fn onto_map() -> BTreeMap<PathBuf, OntoSpec> {
        let mut map = BTreeMap::new();
        map.insert(
            PathBuf::from("/repo"),
            OntoSpec {
                display: "origin/main".to_string(),
                fetch: Some(("origin".to_string(), "main".to_string())),
            },
        );
        map
    }

    fn ok_map(ok: bool) -> BTreeMap<PathBuf, bool> {
        let mut map = BTreeMap::new();
        map.insert(PathBuf::from("/repo"), ok);
        map
    }

    fn classify_reason(
        inspected: &Inspected,
        onto: &BTreeMap<PathBuf, OntoSpec>,
        autostash: bool,
    ) -> RebaseResult {
        inspected.classify(onto, &ok_map(true), autostash).result
    }

    #[test]
    fn classify_skips_a_detached_head() {
        let out = classify_reason(
            &inspected(None, Some(Oid::ZERO_SHA1), true, false),
            &onto_map(),
            false,
        );
        assert_eq!(
            out,
            RebaseResult::Skipped {
                reason: SkipReason::DetachedHead
            }
        );
    }

    #[test]
    fn classify_skips_an_in_progress_operation() {
        let out = classify_reason(
            &inspected(Some("f"), Some(Oid::ZERO_SHA1), false, false),
            &onto_map(),
            false,
        );
        assert_eq!(
            out,
            RebaseResult::Skipped {
                reason: SkipReason::OperationInProgress
            }
        );
    }

    #[test]
    fn classify_skips_dirty_only_without_autostash() {
        let dirty = inspected(Some("f"), Some(Oid::ZERO_SHA1), true, true);
        assert_eq!(
            classify_reason(&dirty, &onto_map(), false),
            RebaseResult::Skipped {
                reason: SkipReason::Dirty
            }
        );
        // With autostash the dirty gate is passed; the fake repo root then yields no
        // onto commit, so it lands on the later `NoOntoRef` rather than `Dirty`.
        assert_eq!(
            classify_reason(&dirty, &onto_map(), true),
            RebaseResult::Skipped {
                reason: SkipReason::NoOntoRef
            }
        );
    }

    #[test]
    fn classify_reports_no_onto_ref_when_the_repo_is_unresolved() {
        let out = classify_reason(
            &inspected(Some("f"), Some(Oid::ZERO_SHA1), true, false),
            &BTreeMap::new(),
            false,
        );
        assert_eq!(
            out,
            RebaseResult::Skipped {
                reason: SkipReason::NoOntoRef
            }
        );
    }

    #[test]
    fn classify_reports_fetch_failed_when_the_repos_fetch_failed() {
        let out = inspected(Some("f"), Some(Oid::ZERO_SHA1), true, false)
            .classify(&onto_map(), &ok_map(false), false)
            .result;
        assert!(matches!(out, RebaseResult::FetchFailed { .. }));
    }

    #[test]
    fn classify_reports_not_a_worktree_for_an_unresolvable_path() {
        let out = Inspected::Unresolvable {
            path: PathBuf::from("/x"),
        }
        .classify(&onto_map(), &ok_map(true), false)
        .result;
        assert_eq!(
            out,
            RebaseResult::Skipped {
                reason: SkipReason::NotAWorktree
            }
        );
    }

    #[test]
    fn head_branch_reports_branch_detached_and_unborn() {
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        config_identity(&repo);
        // Unborn: HEAD points at an unborn branch, no commit yet.
        assert_eq!(head_branch(&repo), (None, None));
        // On a branch.
        let oid = empty_commit(&repo, "refs/heads/main", &[]);
        repo.set_head("refs/heads/main").unwrap();
        let (branch, head) = head_branch(&repo);
        assert_eq!(branch.as_deref(), Some("main"));
        assert_eq!(head, Some(oid));
        // Detached.
        repo.set_head_detached(oid).unwrap();
        assert_eq!(head_branch(&repo), (None, Some(oid)));
    }

    #[test]
    fn resolve_onto_honours_an_override() {
        let (_dir, repo) = repo_with_origin();
        assert_eq!(
            resolve_onto(&repo, Some("origin/main")).fetch,
            Some(("origin".to_string(), "main".to_string()))
        );
        assert_eq!(resolve_onto(&repo, Some("develop")).fetch, None);
    }

    #[test]
    fn fetch_all_skips_the_fetch_for_a_local_onto() {
        let mut map = BTreeMap::new();
        map.insert(
            PathBuf::from("/repo"),
            OntoSpec {
                display: "HEAD~1".to_string(),
                fetch: None,
            },
        );
        let (fetches, ok) = fetch_all(Path::new("git"), &map);
        assert_eq!(fetches.len(), 1);
        assert!(!fetches[0].fetched && fetches[0].ok);
        assert_eq!(ok.get(Path::new("/repo")), Some(&true));
    }

    #[test]
    fn fetch_once_errors_when_the_remote_is_missing() {
        let _guard = serial();
        let dir = tempfile::tempdir().unwrap();
        let repo = Repository::init(dir.path()).unwrap();
        config_identity(&repo);
        let err = fetch_once(&resolve_git_binary(), dir.path(), "origin", "main")
            .unwrap_err()
            .to_string();
        assert!(err.contains("git fetch"), "got: {err}");
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
        let output = run_git_in(&resolve_git_binary(), dir, args).unwrap();
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
