//! Recently closed worktrees: the git half (#2211, [ADR-0096]).
//!
//! The engine ([`crate::worktrees::closed`]) is pure bookkeeping; this is where a
//! departed window's folder is resolved to a branch, repository and head with
//! `git2`, and where the `recent-closed` and `reopen` ops live. Everything that
//! touches the disk runs on a blocking thread, never under a registry lock.
//!
//! [ADR-0096]: https://github.com/rust-works/omni-dev/blob/main/docs/adrs/adr-0096.md

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use git2::{BranchType, Repository, WorktreeAddOptions, WorktreePruneOptions};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{
    canonical, focus_window, git_status_cheap, main_repo_name, remote_github_identity,
    WorktreesService,
};
use crate::worktrees::closed::{ClosedGithub, ClosedWorktree};
use crate::worktrees::{Departure, WorktreesRegistry};

/// The caveat every recreate plan carries: a removed worktree's working-tree
/// state went with its directory and git has no copy of it.
const UNCOMMITTED_WARNING: &str =
    "Uncommitted changes in the removed worktree cannot be recovered; only its committed history is restored.";

/// Resolves `folder` to the [`ClosedWorktree`] it was, as of `closed_at`.
///
/// `None` when it cannot be one: the folder no longer exists, it is not in a
/// git repository (a window on a plain directory has nothing to reopen into a
/// *worktree*), or the repository has no working tree to name. The recorded path
/// is the worktree's root, not the folder, so a window opened on a subdirectory
/// reopens the worktree.
pub(super) fn closed_worktree_for(
    folder: &Path,
    closed_at: DateTime<Utc>,
) -> Option<ClosedWorktree> {
    let repo = Repository::discover(folder).ok()?;
    let workdir = canonical(repo.workdir()?);
    // The common dir is shared by the main checkout and every linked worktree;
    // the working tree of the repository *it* belongs to is the main one. Asking
    // git for that, rather than taking the common dir's parent, keeps a submodule
    // (`<super>/.git/modules/<name>`) or a `--separate-git-dir` repository from
    // being recorded under a bogus root.
    let commondir = canonical(repo.commondir());
    let repo_root = canonical(Repository::open(&commondir).ok()?.workdir()?);
    // Wire shape needs UTF-8 paths; a path that is not has nothing to show.
    workdir.to_str()?;
    repo_root.to_str()?;
    let status = git_status_cheap(&workdir);
    Some(ClosedWorktree {
        is_main: workdir == repo_root,
        main_repo: status
            .main_repo
            .or_else(|| main_repo_name(&commondir))
            .unwrap_or_default(),
        github: remote_github_identity(&repo).map(|id| ClosedGithub {
            owner: id.owner,
            name: id.name,
        }),
        branch: status.branch,
        head_sha: status.head_sha,
        path: workdir,
        repo_root,
        removed: false,
        closed_at,
    })
}

/// The root of the worktree `folder` is in — what a closed entry's `path` is
/// keyed by — or the folder itself when it is not in a repository. A window may be
/// opened on a subdirectory or through a symlink, so a raw folder compares equal to
/// a recorded path only by luck.
fn worktree_root_of(folder: &Path) -> PathBuf {
    Repository::discover(folder)
        .ok()
        .and_then(|repo| repo.workdir().map(canonical))
        .unwrap_or_else(|| canonical(folder))
}

/// Settles every window that has left the registry into the recently-closed log,
/// then reconciles the log with the disk.
///
/// Called from the tree snapshot (after its own reap, so a window that just aged
/// out is included) and from the ops that read the list, which is what makes a
/// closure visible as soon as it can be: a window leaving bumps the change-notify,
/// the stream rebuilds the snapshot, and the snapshot settles it.
pub(super) async fn settle_departures(registry: &WorktreesRegistry) {
    let departures = registry.take_departures();
    let open = registry.open_folders();
    let log_entries = registry.closed_entries();
    if departures.is_empty() && log_entries.iter().all(|e| e.removed) {
        return;
    }
    let settled = tokio::task::spawn_blocking(move || {
        let open: Vec<PathBuf> = open.iter().map(|f| worktree_root_of(f)).collect();
        let recorded = resolve_departures(&departures, &open);
        let vanished: Vec<PathBuf> = log_entries
            .iter()
            // The repository must still be there: a worktree on a volume that is
            // merely unmounted is missing along with its repository, and is not
            // thereby removed.
            .filter(|e| !e.removed && !e.path.exists() && e.repo_root.exists())
            .map(|e| e.path.clone())
            .collect();
        (recorded, vanished)
    })
    .await;
    match settled {
        Ok((recorded, vanished)) => {
            registry.record_closed(recorded);
            // A closed window whose directory has since gone: the worktree was
            // removed (outside omni-dev, for we only see our own removals).
            registry.mark_closed_removed(&vanished);
        }
        Err(err) => tracing::warn!("worktrees: settling closed windows panicked: {err}"),
    }
}

/// Resolves each departed window's folders to closed worktrees, skipping any a
/// live window still has open (another window on the same worktree, or a window
/// that re-registered). `open` holds the roots of the worktrees windows have
/// open ([`worktree_root_of`]), so a nested worktree does not mask its parent. One entry per worktree: a
/// multi-root window with two folders in one worktree records it once.
fn resolve_departures(departures: &[Departure], open: &[PathBuf]) -> Vec<ClosedWorktree> {
    let mut recorded: Vec<ClosedWorktree> = Vec::new();
    for departure in departures {
        for folder in &departure.folders {
            let Some(entry) = closed_worktree_for(folder, departure.at) else {
                continue;
            };
            let still_open = open.contains(&entry.path);
            if still_open || recorded.iter().any(|r| r.path == entry.path) {
                continue;
            }
            recorded.push(entry);
        }
    }
    recorded
}

/// The `reopen` op's payload.
#[derive(Debug, Clone, Deserialize)]
pub(super) struct ReopenRequest {
    /// The recorded worktree's path. Only a path the daemon recorded is accepted;
    /// there is no way to name a branch or a destination.
    path: PathBuf,
    /// Set on the phase-2 call that actually recreates a removed worktree. Absent
    /// is the side-effect-free plan; ignored when the worktree is still on disk.
    #[serde(default)]
    confirmed: bool,
}

/// What reopening a removed worktree would do, for the extension to put in front
/// of the user before it does it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(super) struct ReopenPlan {
    /// Whether the worktree can be recreated at all.
    restorable: bool,
    /// What it is recreated from: the recorded `branch` if it still exists, or
    /// the recorded `head-sha` if only the commit survives.
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<&'static str>,
    /// The recorded branch.
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    /// The recorded head commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    head_sha: Option<String>,
    /// Why it cannot be restored; present only when `restorable` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    /// What the user should know before confirming. Always includes the
    /// uncommitted-changes caveat for a restorable plan.
    warnings: Vec<String>,
}

impl ReopenPlan {
    fn unrestorable(entry: &ClosedWorktree, reason: impl Into<String>) -> Self {
        Self {
            restorable: false,
            source: None,
            branch: entry.branch.clone(),
            head_sha: entry.head_sha.clone(),
            reason: Some(reason.into()),
            warnings: Vec::new(),
        }
    }
}

/// How a removed worktree is recreated, once [`plan_recreate`] has decided.
enum Recreate {
    /// Check out the existing local branch.
    Branch(String),
    /// Recreate the branch at the recorded commit, then check it out.
    BranchAtCommit(String, git2::Oid),
}

/// Plans recreating the removed worktree `entry`: pure inspection, nothing is
/// created. The one place that decides restorability, so the phase-1 plan the
/// user confirms and the phase-2 execute cannot disagree.
fn plan_recreate(entry: &ClosedWorktree) -> (ReopenPlan, Option<Recreate>) {
    if entry.is_main {
        return (
            ReopenPlan::unrestorable(
                entry,
                "it was the repository's main working tree, which is never recreated",
            ),
            None,
        );
    }
    let Some(branch) = entry.branch.clone() else {
        return (
            ReopenPlan::unrestorable(
                entry,
                "it was on a detached HEAD, so there is no branch to restore it onto",
            ),
            None,
        );
    };
    if entry.path.exists() {
        return (
            ReopenPlan::unrestorable(entry, format!("{} already exists", entry.path.display())),
            None,
        );
    }
    let repo = match open_main_repo(&entry.repo_root) {
        Ok(repo) => repo,
        Err(reason) => return (ReopenPlan::unrestorable(entry, reason), None),
    };
    if let Some(at) = branch_checked_out_at(&repo, &branch) {
        return (
            ReopenPlan::unrestorable(
                entry,
                format!(
                    "branch `{branch}` is already checked out at {}",
                    at.display()
                ),
            ),
            None,
        );
    }
    let mut warnings = vec![UNCOMMITTED_WARNING.to_string()];
    // `Some(tip)` when the branch still exists (its tip is `None` only for a
    // symbolic ref, which a local branch never is).
    let existing_tip = repo
        .find_branch(&branch, BranchType::Local)
        .ok()
        .map(|found| found.get().target().map(|oid| oid.to_string()));
    let (source, how) = if let Some(tip) = existing_tip {
        if tip.is_some() && tip != entry.head_sha {
            warnings.push(format!(
                "Branch `{branch}` has moved since the worktree closed; it will check out the branch's current tip."
            ));
        }
        ("branch", Recreate::Branch(branch.clone()))
    } else {
        let oid = entry
            .head_sha
            .as_deref()
            .and_then(|sha| git2::Oid::from_str(sha).ok())
            .filter(|oid| repo.find_commit(*oid).is_ok());
        let Some(oid) = oid else {
            return (
                ReopenPlan::unrestorable(
                    entry,
                    format!(
                        "branch `{branch}` no longer exists and the commit it was on is no longer in the repository"
                    ),
                ),
                None,
            );
        };
        warnings.push(format!(
            "Branch `{branch}` no longer exists; it will be recreated at the commit the worktree was on ({}).",
            short(&oid.to_string())
        ));
        ("head-sha", Recreate::BranchAtCommit(branch.clone(), oid))
    };
    let plan = ReopenPlan {
        restorable: true,
        source: Some(source),
        branch: Some(branch),
        head_sha: entry.head_sha.clone(),
        reason: None,
        warnings,
    };
    (plan, Some(how))
}

/// The first seven characters of a commit id, for a human-readable message.
fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// Opens the repository whose main working tree is `repo_root`, refusing anything
/// that is not one: the recorded root is only trusted to the extent git agrees.
fn open_main_repo(repo_root: &Path) -> std::result::Result<Repository, String> {
    let repo = Repository::open(repo_root)
        .map_err(|_| format!("the repository at {} is gone", repo_root.display()))?;
    if repo.is_worktree() || repo.is_bare() {
        return Err(format!(
            "{} is no longer the repository's main working tree",
            repo_root.display()
        ));
    }
    Ok(repo)
}

/// The worktree that has `branch` checked out, if any. git refuses to check one
/// branch out in two worktrees, and saying so up front names where it is.
fn branch_checked_out_at(repo: &Repository, branch: &str) -> Option<PathBuf> {
    let on_branch = |r: &Repository| {
        r.head()
            .is_ok_and(|head| head.is_branch() && head.shorthand().ok() == Some(branch))
    };
    if on_branch(repo) {
        return repo.workdir().map(Path::to_path_buf);
    }
    let names = repo.worktrees().ok()?;
    names
        .iter()
        .flatten() // Result<Option<&str>, _> → Option<&str> (drop per-name errors)
        .flatten() // Option<&str> → &str (drop non-UTF-8 names)
        .filter_map(|name| repo.find_worktree(name).ok())
        .find(|wt| {
            Repository::open_from_worktree(wt).is_ok_and(|r| on_branch(&r))
        })
        .map(|wt| wt.path().to_path_buf())
}

/// A worktree admin name free in `repo`, derived from `path`'s directory name.
///
/// A name whose admin entry is left over from a removal git never finished
/// (the checkout is gone, the metadata is not) is pruned and reused; a name a
/// live worktree holds is skipped with a numeric suffix.
fn free_worktree_name(repo: &Repository, path: &Path) -> Result<String> {
    let base = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("{} has no usable directory name", path.display()))?
        .to_string();
    for attempt in 1..100 {
        let name = if attempt == 1 {
            base.clone()
        } else {
            format!("{base}-{attempt}")
        };
        let Ok(existing) = repo.find_worktree(&name) else {
            return Ok(name);
        };
        // Only metadata left over by *this* worktree's own removal is cleared. A
        // same-named worktree whose checkout is merely missing (an unmounted
        // volume) belongs to someone else, and git would forget it.
        if existing.path() == path && !existing.path().exists() {
            let mut opts = WorktreePruneOptions::new();
            existing
                .prune(Some(&mut opts))
                .with_context(|| format!("failed to clear stale worktree metadata `{name}`"))?;
            return Ok(name);
        }
    }
    bail!("no free worktree name derived from `{base}`")
}

/// Recreates the removed worktree `entry` the way [`plan_recreate`] decided.
/// Blocking.
fn recreate_worktree(entry: &ClosedWorktree, how: &Recreate) -> Result<()> {
    let repo = open_main_repo(&entry.repo_root).map_err(|reason| anyhow!(reason))?;
    if let Some(parent) = entry.path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let name = free_worktree_name(&repo, &entry.path)?;
    let (reference, created_branch) = match how {
        Recreate::Branch(branch) => {
            let found = repo.find_branch(branch, BranchType::Local)?;
            (found.into_reference(), None)
        }
        Recreate::BranchAtCommit(branch, oid) => {
            let commit = repo.find_commit(*oid)?;
            let created = repo.branch(branch, &commit, false)?;
            (created.into_reference(), Some(branch.as_str()))
        }
    };
    let mut opts = WorktreeAddOptions::new();
    opts.reference(Some(&reference));
    if let Err(err) = repo.worktree(&name, &entry.path, Some(&opts)) {
        // A branch this op just made would otherwise outlive the failure that
        // made it pointless.
        if let Some(branch) = created_branch {
            if let Err(cleanup) = repo
                .find_branch(branch, BranchType::Local)
                .and_then(|mut b| b.delete())
            {
                tracing::debug!(
                    "could not remove branch `{branch}` after a failed recreate: {cleanup}"
                );
            }
        }
        return Err(err).with_context(|| {
            format!(
                "failed to recreate the worktree at {}",
                entry.path.display()
            )
        });
    }
    Ok(())
}

impl WorktreesService {
    /// Handles the `recent-closed` op: the recently-closed list, newest first.
    pub(super) async fn recent_closed(&self) -> Value {
        settle_departures(&self.registry).await;
        json!({ "closed": self.registry.closed_entries() })
    }

    /// Handles the `reopen` op (#2211).
    ///
    /// - A worktree still on disk — a closed *window* — is opened with the same
    ///   launcher `open` uses. Nothing is created, so there is nothing to confirm.
    /// - A **removed** worktree is two-phase, keyed off `confirmed`, like
    ///   `close`/`rebase`: phase 1 returns the [`ReopenPlan`] and creates nothing;
    ///   phase 2 **re-plans from scratch** (never trusting a plan the client saw),
    ///   creates the worktree, and opens it.
    ///
    /// The path must match a recorded closure. The op accepts no branch, base or
    /// destination of its own, so it can recreate only what the daemon watched
    /// close — a socket writer gains no way to make git write anywhere new.
    pub(super) async fn reopen(&self, req: ReopenRequest) -> Result<Value> {
        self.reopen_with(req, &focus_window).await
    }

    /// [`reopen`](Self::reopen) with an explicit window launcher, so a test can
    /// record what would be opened instead of spawning an editor.
    async fn reopen_with(
        &self,
        req: ReopenRequest,
        launch: &(dyn Fn(&Path) -> Result<()> + Sync),
    ) -> Result<Value> {
        settle_departures(&self.registry).await;
        let entry = self.recorded_closure(&req.path)?;
        let on_disk = {
            let path = entry.path.clone();
            tokio::task::spawn_blocking(move || path.exists())
                .await
                .unwrap_or(false)
        };
        if on_disk {
            // Worktree still there (the window was closed, or the removal flag was
            // set while a drive was unmounted): just open it.
            // The entry is not dropped here: the window's own `register` forgets it,
            // so a launcher that never produces a window loses nothing.
            launch(&entry.path)?;
            return Ok(json!({ "reopened": true, "recreated": false }));
        }
        let planned = entry.clone();
        let (plan, how) = tokio::task::spawn_blocking(move || plan_recreate(&planned))
            .await
            .map_err(|e| anyhow!("reopen plan task panicked: {e}"))?;
        let Some(how) = how.filter(|_| req.confirmed) else {
            return Ok(json!({ "reopened": false, "plan": plan }));
        };
        // The same resource `close` serializes on: both write `.git/worktrees`.
        let _guard = self.prune_lock.lock().await;
        let created = entry.clone();
        tokio::task::spawn_blocking(move || recreate_worktree(&created, &how))
            .await
            .map_err(|e| anyhow!("reopen task panicked: {e}"))
            .and_then(|inner| inner)
            .inspect_err(|err| {
                tracing::warn!(
                    path = %entry.path.display(),
                    "worktrees reopen: recreate failed: {err:#}"
                );
            })?;
        tracing::info!(
            path = %entry.path.display(),
            source = plan.source.unwrap_or("-"),
            "worktrees reopen: removed worktree recreated"
        );
        // It exists again, so it is no longer "removed". The entry stays until the
        // window registers (and forgets it), for the same reason as above.
        self.registry.record_closed(vec![ClosedWorktree {
            removed: false,
            ..entry.clone()
        }]);
        // The worktree exists now; failing to open its window is not a failed
        // recreate, so say so rather than erroring.
        let open_error = launch(&entry.path).err().map(|e| format!("{e:#}"));
        let mut reply = json!({
            "reopened": true,
            "recreated": true,
            "opened": open_error.is_none(),
        });
        if let Some(source) = plan.source {
            reply["source"] = json!(source);
        }
        if let Some(err) = open_error {
            reply["open_error"] = json!(err);
        }
        Ok(reply)
    }

    /// Forgets the closures of the worktrees `folders` belong to — they are open
    /// (again). Resolves each folder to its worktree root first, so a window opened
    /// on a subdirectory or through a symlink still matches its entry.
    pub(super) async fn forget_opened(&self, folders: Vec<PathBuf>) {
        if self.registry.closed_entries().is_empty() {
            return;
        }
        let roots = tokio::task::spawn_blocking(move || {
            folders
                .iter()
                .map(|f| worktree_root_of(f))
                .collect::<Vec<_>>()
        })
        .await
        .unwrap_or_default();
        self.registry.forget_closed(&roots);
    }

    /// The recorded closure for `path` — exactly, or by its canonical form (a
    /// symlinked spelling of a path that still exists).
    fn recorded_closure(&self, path: &Path) -> Result<ClosedWorktree> {
        self.registry
            .closed_entry(path)
            .or_else(|| self.registry.closed_entry(&canonical(path)))
            .ok_or_else(|| {
                anyhow!(
                    "no recently closed worktree at {} (it may have aged out, or been reopened)",
                    path.display()
                )
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::Mutex;

    use chrono::Duration as ChronoDuration;

    use super::*;
    use crate::daemon::service::DaemonService;

    /// A temp dir whose path is canonical, so it compares equal to what the code
    /// under test canonicalizes (`/var` is a symlink to `/private/var` on macOS).
    fn tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    /// A repository at `root/main` with one commit on `main`.
    fn main_repo(root: &Path) -> (Repository, PathBuf) {
        let path = root.join("main");
        std::fs::create_dir_all(&path).unwrap();
        let repo = Repository::init(&path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        {
            let tree = repo
                .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
                .unwrap();
            repo.commit(Some("refs/heads/main"), &sig, &sig, "A", &tree, &[])
                .unwrap();
        }
        repo.set_head("refs/heads/main").unwrap();
        (repo, path)
    }

    /// Adds a linked worktree at `path` on a new branch `branch`.
    fn add_worktree(repo: &Repository, path: &Path, branch: &str) {
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        let created = repo.branch(branch, &head, false).unwrap();
        let mut opts = WorktreeAddOptions::new();
        opts.reference(Some(created.get()));
        let name = path.file_name().unwrap().to_str().unwrap();
        repo.worktree(name, path, Some(&opts)).unwrap();
    }

    fn register(svc: &WorktreesService, key: &str, folder: &Path) {
        svc.registry.register(crate::worktrees::RegisterRequest {
            key: key.to_string(),
            folders: vec![folder.to_path_buf()],
            repo: None,
            title: None,
            pid: None,
        });
    }

    fn closed_paths(reply: &Value) -> Vec<String> {
        reply["closed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["path"].as_str().unwrap().to_string())
            .collect()
    }

    /// A launcher that records what it was asked to open.
    #[derive(Default)]
    struct Launched(Mutex<Vec<PathBuf>>);

    impl Launched {
        fn call(&self, path: &Path) -> Result<()> {
            self.0.lock().unwrap().push(path.to_path_buf());
            Ok(())
        }
        fn paths(&self) -> Vec<PathBuf> {
            self.0.lock().unwrap().clone()
        }
    }

    async fn reopen(
        svc: &WorktreesService,
        path: &Path,
        confirmed: bool,
        launched: &Launched,
    ) -> Result<Value> {
        svc.reopen_with(
            ReopenRequest {
                path: path.to_path_buf(),
                confirmed,
            },
            &|p| launched.call(p),
        )
        .await
    }

    /// A linked worktree `root/wt` on `feature`, recorded as closed and then
    /// removed from disk the way the `close` op removes one.
    fn removed_worktree(svc: &WorktreesService, root: &Path) -> (Repository, PathBuf) {
        let (repo, _) = main_repo(root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let mut entry = closed_worktree_for(&wt, Utc::now()).unwrap();
        super::super::remove_worktree(&wt, &[]).unwrap();
        entry.removed = true;
        svc.registry.record_closed(vec![entry]);
        (repo, wt)
    }

    #[test]
    fn closed_worktree_for_describes_main_and_linked_worktrees() {
        let (_guard, root) = tempdir();
        let (repo, main) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let now = Utc::now();

        let linked = closed_worktree_for(&wt, now).unwrap();
        assert_eq!(linked.path, wt);
        assert_eq!(linked.repo_root, main);
        assert_eq!(linked.main_repo, "main");
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        assert!(!linked.is_main);
        assert!(!linked.removed);
        assert_eq!(linked.closed_at, now);
        assert_eq!(
            linked.head_sha.as_deref(),
            Some(repo.head().unwrap().target().unwrap().to_string().as_str())
        );

        let primary = closed_worktree_for(&main, now).unwrap();
        assert!(primary.is_main);
        assert_eq!(primary.branch.as_deref(), Some("main"));
    }

    #[test]
    fn closed_worktree_for_resolves_a_subdirectory_to_its_worktree_root() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        std::fs::create_dir_all(wt.join("src/deep")).unwrap();
        let entry = closed_worktree_for(&wt.join("src/deep"), Utc::now()).unwrap();
        assert_eq!(entry.path, wt);
    }

    #[test]
    fn closed_worktree_for_is_none_without_a_repository_or_a_folder() {
        let (_guard, root) = tempdir();
        assert!(closed_worktree_for(&root, Utc::now()).is_none());
        assert!(closed_worktree_for(&root.join("missing"), Utc::now()).is_none());
    }

    #[tokio::test]
    async fn an_unregistered_window_is_recorded_as_closed() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let svc = WorktreesService::new();
        register(&svc, "w1", &wt);

        assert!(closed_paths(&svc.recent_closed().await).is_empty());
        svc.handle("unregister", json!({ "key": "w1" }))
            .await
            .unwrap();

        let reply = svc.recent_closed().await;
        assert_eq!(closed_paths(&reply), [wt.to_str().unwrap()]);
        assert_eq!(reply["closed"][0]["branch"], "feature");
        assert_eq!(reply["closed"][0]["removed"], false);
    }

    #[tokio::test]
    async fn closing_a_repos_last_window_records_it() {
        // The "repo vanished from the tree" case: with no window left the tree
        // has nothing to show, but the closure is still offered back.
        let (_guard, root) = tempdir();
        let (_repo, main) = main_repo(&root);
        let svc = WorktreesService::new();
        register(&svc, "w1", &main);
        svc.registry.unregister("w1");

        let tree = svc.handle("tree", Value::Null).await.unwrap();
        assert_eq!(tree["repos"], json!([]));
        assert_eq!(tree["recently_closed"][0]["path"], main.to_str().unwrap());
        assert_eq!(tree["recently_closed"][0]["is_main"], true);
    }

    #[tokio::test]
    async fn a_window_reload_leaves_nothing_behind() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let svc = WorktreesService::new();
        register(&svc, "w1", &wt);
        // A reload is an unregister followed by a register of the same folders.
        svc.registry.unregister("w1");
        register(&svc, "w1", &wt);

        assert!(closed_paths(&svc.recent_closed().await).is_empty());
    }

    #[tokio::test]
    async fn a_worktree_another_window_still_has_open_is_not_recorded() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let svc = WorktreesService::new();
        register(&svc, "w1", &wt);
        register(&svc, "w2", &wt);
        svc.registry.unregister("w1");

        assert!(closed_paths(&svc.recent_closed().await).is_empty());
    }

    #[tokio::test]
    async fn opening_a_subdirectory_forgets_the_worktrees_closure() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let svc = WorktreesService::new();
        register(&svc, "w1", &wt);
        svc.registry.unregister("w1");
        assert_eq!(closed_paths(&svc.recent_closed().await).len(), 1);

        svc.handle(
            "register",
            json!({ "key": "w2", "folders": [wt.join("src")] }),
        )
        .await
        .unwrap();

        assert!(closed_paths(&svc.recent_closed().await).is_empty());
    }

    #[tokio::test]
    async fn a_worktree_nested_inside_another_does_not_mask_it() {
        // A repo that keeps linked worktrees inside its own working tree: the
        // window on the nested one stays open while the one on the main tree closes.
        let (_guard, root) = tempdir();
        let (repo, main) = main_repo(&root);
        let nested = main.join(".worktrees").join("x");
        std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
        add_worktree(&repo, &nested, "feature");
        let svc = WorktreesService::new();
        register(&svc, "main", &main);
        register(&svc, "nested", &nested);
        svc.registry.unregister("main");

        assert_eq!(
            closed_paths(&svc.recent_closed().await),
            [main.to_str().unwrap()]
        );
    }

    #[tokio::test]
    async fn a_missing_checkout_is_not_removed_while_its_repository_is_missing_too() {
        // An unmounted volume takes both away; neither was removed.
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let mut entry = closed_worktree_for(&main_repo(&root).1, Utc::now()).unwrap();
        entry.path = root.join("gone").join("wt");
        entry.repo_root = root.join("gone").join("repo");
        svc.registry.record_closed(vec![entry]);

        assert_eq!(svc.recent_closed().await["closed"][0]["removed"], false);
    }

    #[test]
    fn the_repository_root_of_a_separate_gitdir_is_its_working_tree() {
        let (_guard, root) = tempdir();
        let gitdir = root.join("sep.git");
        let workdir = root.join("wd");
        std::fs::create_dir_all(&workdir).unwrap();
        let mut opts = git2::RepositoryInitOptions::new();
        opts.workdir_path(&workdir);
        let repo = Repository::init_opts(&gitdir, &opts).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree = repo
            .find_tree(repo.treebuilder(None).unwrap().write().unwrap())
            .unwrap();
        repo.commit(Some("refs/heads/main"), &sig, &sig, "A", &tree, &[])
            .unwrap();
        repo.set_head("refs/heads/main").unwrap();

        let entry = closed_worktree_for(&workdir, Utc::now()).unwrap();

        assert_eq!(entry.repo_root, std::fs::canonicalize(&workdir).unwrap());
        assert!(entry.is_main);
    }

    #[tokio::test]
    async fn the_snapshot_omits_the_field_until_something_has_closed() {
        let svc = WorktreesService::new();
        let tree = svc.handle("tree", Value::Null).await.unwrap();
        assert!(tree.get("recently_closed").is_none());
    }

    #[tokio::test]
    async fn removing_a_worktree_with_the_close_op_records_it_as_removed() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let svc = WorktreesService::new();

        svc.handle(
            "close",
            json!({ "path": wt, "remove": true, "confirmed": true }),
        )
        .await
        .unwrap();

        assert!(!wt.exists());
        let reply = svc.recent_closed().await;
        assert_eq!(closed_paths(&reply), [wt.to_str().unwrap()]);
        assert_eq!(reply["closed"][0]["removed"], true);
        assert_eq!(reply["closed"][0]["branch"], "feature");
    }

    #[tokio::test]
    async fn a_closed_window_whose_directory_later_vanishes_becomes_removed() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let svc = WorktreesService::new();
        register(&svc, "w1", &wt);
        svc.registry.unregister("w1");
        assert_eq!(svc.recent_closed().await["closed"][0]["removed"], false);

        // Pruned outside omni-dev, as `git worktree remove` would.
        super::super::remove_worktree(&wt, &[]).unwrap();

        assert_eq!(svc.recent_closed().await["closed"][0]["removed"], true);
    }

    #[tokio::test]
    async fn reopening_a_closed_window_opens_it_and_the_windows_register_forgets_the_entry() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let svc = WorktreesService::new();
        register(&svc, "w1", &wt);
        svc.registry.unregister("w1");
        let launched = Launched::default();

        let reply = reopen(&svc, &wt, false, &launched).await.unwrap();

        assert_eq!(reply, json!({ "reopened": true, "recreated": false }));
        assert_eq!(launched.paths(), std::slice::from_ref(&wt));
        // Not dropped at launch: a launcher that never produces a window loses
        // nothing. The window's own `register` is what forgets it.
        assert_eq!(closed_paths(&svc.recent_closed().await).len(), 1);
        svc.handle("register", json!({ "key": "w2", "folders": [wt] }))
            .await
            .unwrap();
        assert!(closed_paths(&svc.recent_closed().await).is_empty());
    }

    #[tokio::test]
    async fn reopening_an_unrecorded_path_is_refused() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let launched = Launched::default();
        let err = reopen(&svc, &root.join("nope"), true, &launched)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no recently closed worktree"));
        assert!(launched.paths().is_empty());
    }

    #[tokio::test]
    async fn the_plan_for_a_removed_worktree_creates_nothing() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let (_repo, wt) = removed_worktree(&svc, &root);
        let launched = Launched::default();

        let reply = reopen(&svc, &wt, false, &launched).await.unwrap();

        assert_eq!(reply["reopened"], false);
        let plan = &reply["plan"];
        assert_eq!(plan["restorable"], true);
        assert_eq!(plan["source"], "branch");
        assert_eq!(plan["branch"], "feature");
        let warnings = plan["warnings"].as_array().unwrap();
        assert!(warnings[0]
            .as_str()
            .unwrap()
            .contains("Uncommitted changes"));
        assert!(!wt.exists(), "phase 1 must not create the worktree");
        assert!(launched.paths().is_empty());
        assert_eq!(closed_paths(&svc.recent_closed().await).len(), 1);
    }

    #[tokio::test]
    async fn a_confirmed_reopen_recreates_the_worktree_on_its_branch() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let (repo, wt) = removed_worktree(&svc, &root);
        let launched = Launched::default();

        let reply = reopen(&svc, &wt, true, &launched).await.unwrap();

        assert_eq!(reply["reopened"], true);
        assert_eq!(reply["recreated"], true);
        assert_eq!(reply["source"], "branch");
        let recreated = closed_worktree_for(&wt, Utc::now()).unwrap();
        assert_eq!(recreated.branch.as_deref(), Some("feature"));
        assert!(!recreated.is_main);
        assert_eq!(launched.paths(), std::slice::from_ref(&wt));
        // It exists again, so it is listed as closed rather than removed, until a
        // window registers on it.
        let listed = svc.recent_closed().await;
        assert_eq!(closed_paths(&listed), [wt.to_str().unwrap()]);
        assert_eq!(listed["closed"][0]["removed"], false);
        drop(repo);
    }

    #[tokio::test]
    async fn a_deleted_branch_is_recreated_at_the_recorded_commit() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let (repo, wt) = removed_worktree(&svc, &root);
        repo.find_branch("feature", BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
        let launched = Launched::default();

        let plan = reopen(&svc, &wt, false, &launched).await.unwrap();
        assert_eq!(plan["plan"]["source"], "head-sha");
        assert!(plan["plan"]["warnings"][1]
            .as_str()
            .unwrap()
            .contains("no longer exists"));

        let reply = reopen(&svc, &wt, true, &launched).await.unwrap();
        assert_eq!(reply["source"], "head-sha");
        let head = repo.head().unwrap().target().unwrap();
        let branch = repo.find_branch("feature", BranchType::Local).unwrap();
        assert_eq!(branch.get().target().unwrap(), head);
        assert_eq!(
            closed_worktree_for(&wt, Utc::now())
                .unwrap()
                .branch
                .as_deref(),
            Some("feature")
        );
    }

    #[tokio::test]
    async fn a_worktree_with_neither_branch_nor_commit_cannot_be_restored() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let (repo, wt) = removed_worktree(&svc, &root);
        repo.find_branch("feature", BranchType::Local)
            .unwrap()
            .delete()
            .unwrap();
        // Record a commit the repository has never had.
        let mut entry = svc.registry.closed_entry(&wt).unwrap();
        entry.head_sha = Some("0123456789012345678901234567890123456789".to_string());
        entry.closed_at += ChronoDuration::seconds(1);
        svc.registry.record_closed(vec![entry]);
        let launched = Launched::default();

        let plan = reopen(&svc, &wt, true, &launched).await.unwrap();

        assert_eq!(plan["reopened"], false);
        assert_eq!(plan["plan"]["restorable"], false);
        assert!(plan["plan"]["reason"]
            .as_str()
            .unwrap()
            .contains("no longer exists"));
        assert!(
            !wt.exists(),
            "an unrestorable plan must not create anything"
        );
    }

    #[tokio::test]
    async fn a_branch_checked_out_elsewhere_cannot_be_restored() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let (repo, wt) = removed_worktree(&svc, &root);
        // `feature` is now checked out in another worktree.
        let other = root.join("other");
        let feature = repo.find_branch("feature", BranchType::Local).unwrap();
        let mut opts = WorktreeAddOptions::new();
        opts.reference(Some(feature.get()));
        repo.worktree("other", &other, Some(&opts)).unwrap();
        let launched = Launched::default();

        let reply = reopen(&svc, &wt, true, &launched).await.unwrap();

        assert_eq!(reply["plan"]["restorable"], false);
        assert!(reply["plan"]["reason"]
            .as_str()
            .unwrap()
            .contains("already checked out"));
        assert!(!wt.exists());
    }

    #[test]
    fn a_main_working_tree_and_a_detached_head_are_never_recreated() {
        let (_guard, root) = tempdir();
        let (_repo, main) = main_repo(&root);
        let mut entry = closed_worktree_for(&main, Utc::now()).unwrap();
        entry.removed = true;
        let (plan, how) = plan_recreate(&entry);
        assert!(!plan.restorable && how.is_none());
        assert!(plan.reason.unwrap().contains("main working tree"));

        entry.is_main = false;
        entry.branch = None;
        let (plan, how) = plan_recreate(&entry);
        assert!(!plan.restorable && how.is_none());
        assert!(plan.reason.unwrap().contains("detached HEAD"));
    }

    #[test]
    fn a_gone_repository_cannot_be_restored() {
        let (_guard, root) = tempdir();
        let svc = WorktreesService::new();
        let (_repo, wt) = removed_worktree(&svc, &root);
        let mut entry = svc.registry.closed_entry(&wt).unwrap();
        entry.repo_root = root.join("vanished");
        let (plan, _) = plan_recreate(&entry);
        assert!(!plan.restorable);
        assert!(plan.reason.unwrap().contains("is gone"));
    }

    #[test]
    fn a_stale_admin_entry_from_an_unfinished_removal_is_cleared_and_reused() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        // The checkout vanished but git still tracks it, as after `rm -rf`.
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(free_worktree_name(&repo, &wt).unwrap(), "wt");
        assert!(
            repo.find_worktree("wt").is_err(),
            "stale metadata is pruned"
        );
    }

    #[test]
    fn another_worktrees_missing_checkout_is_never_pruned() {
        // `wt` is held by a worktree whose checkout is merely missing (say, an
        // unmounted volume). Recreating a different `…/wt` must not clear it.
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let other = root.join("volume").join("wt");
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        add_worktree(&repo, &other, "feature");
        std::fs::remove_dir_all(&other).unwrap();

        let name = free_worktree_name(&repo, &root.join("elsewhere").join("wt")).unwrap();

        assert_eq!(name, "wt-2");
        assert!(
            repo.find_worktree("wt").is_ok(),
            "its metadata is untouched"
        );
    }

    #[test]
    fn a_name_held_by_a_live_worktree_gets_a_suffix() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        add_worktree(&repo, &root.join("wt"), "feature");
        let elsewhere = root.join("nested").join("wt");
        assert_eq!(free_worktree_name(&repo, &elsewhere).unwrap(), "wt-2");
    }

    #[test]
    fn the_closed_list_survives_a_restart_and_the_file_is_private() {
        let (_guard, root) = tempdir();
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        let file = root.join("run").join("worktrees-closed.json");

        let svc = WorktreesService::new();
        svc.load_closed(file.clone());
        svc.registry
            .record_closed(vec![closed_worktree_for(&wt, Utc::now()).unwrap()]);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        assert!(
            !root.join("run").join("worktrees-closed.json.tmp").exists(),
            "the staging file is renamed over the list, not left behind"
        );

        let restarted = WorktreesService::new();
        restarted.load_closed(file);
        let entries = restarted.registry.closed_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, wt);
        assert_eq!(entries[0].branch.as_deref(), Some("feature"));
    }

    #[test]
    fn a_corrupt_or_unreadable_closed_file_is_treated_as_empty() {
        let (_guard, root) = tempdir();
        let corrupt = root.join("corrupt.json");
        std::fs::write(&corrupt, b"{ not json").unwrap();
        let svc = WorktreesService::new();
        svc.load_closed(corrupt.clone());
        assert!(svc.registry.closed_entries().is_empty());
        // The next change rewrites a clean file.
        let (repo, _) = main_repo(&root);
        let wt = root.join("wt");
        add_worktree(&repo, &wt, "feature");
        svc.registry
            .record_closed(vec![closed_worktree_for(&wt, Utc::now()).unwrap()]);
        let fresh = WorktreesService::new();
        fresh.load_closed(corrupt);
        assert_eq!(fresh.registry.closed_entries().len(), 1);

        // A directory where the file should be: unreadable, not fatal.
        let svc = WorktreesService::new();
        svc.load_closed(root);
        assert!(svc.registry.closed_entries().is_empty());
    }
}
