//! Primitives shared by the batch-worktree git engines — [`worktree_rebase`] and
//! [`worktree_push`].
//!
//! Both engines answer the same two questions before they do anything specific:
//! *which* worktrees is this batch about, and *what* is checked out in each. They
//! also both need one carefully-configured `git` subprocess seam. Extracting that
//! common half here keeps a second engine from re-deriving it — the two would
//! drift, and the subprocess seam in particular encodes a non-obvious
//! environment-snapshot rule (see [`run_git_in`]) that is not safe to re-invent.
//!
//! Nothing here decides anything: classification, the mutation, and the outcome
//! shapes belong to each engine.
//!
//! [`worktree_rebase`]: crate::git::worktree_rebase
//! [`worktree_push`]: crate::git::worktree_push

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use git2::{Oid, Repository};

/// Which worktrees a batch operation should target.
///
/// Shared by the rebase and push engines: both take an explicit selection and
/// neither has a bare "do everything everywhere" mode. What a path that turns out
/// to be unsuitable means — skipped and why — is each engine's own business.
#[derive(Debug, Clone)]
pub enum Selection {
    /// Operate on exactly these worktree folders (each resolved to the worktree
    /// that contains it). A path that is unsuitable is reported and skipped,
    /// never acted on. The main working tree is a valid target like any other
    /// (ADR-0060); a push additionally refuses to *force*-push the repository's
    /// remote default branch, but that gate is on the branch, not the worktree
    /// (ADR-0061).
    Paths(Vec<PathBuf>),
    /// Operate on every worktree of the repository that contains `base` (usually
    /// the process working directory) — the main working tree included, alongside
    /// every linked one (ADR-0060).
    All {
        /// The directory whose repository's worktrees are the target set.
        base: PathBuf,
    },
}

/// The concrete worktree paths a [`Selection`] targets.
pub(crate) fn resolve_selection(selection: &Selection) -> Result<Vec<PathBuf>> {
    match selection {
        Selection::Paths(paths) => Ok(paths.clone()),
        Selection::All { base } => all_worktree_paths(base),
    }
}

/// Every worktree path of the repository containing `base` — the main working tree
/// plus every linked one (ADR-0060). Mirrors the daemon service's repo enumeration:
/// discover the repo, resolve its shared common dir's parent as the main root, then
/// list the worktrees registered on the main repository.
pub(crate) fn all_worktree_paths(base: &Path) -> Result<Vec<PathBuf>> {
    let repo = Repository::discover(base)
        .with_context(|| format!("not inside a git repository: {}", base.display()))?;
    let root = main_root(&repo);
    let main_repo = Repository::open(&root)
        .with_context(|| format!("cannot open main repository: {}", root.display()))?;
    let names = main_repo
        .worktrees()
        .context("cannot enumerate worktrees")?;
    let mut paths = vec![root];
    // `iter()` yields `Result<Option<&str>, _>`: the first `flatten` drops per-name
    // errors, the second drops non-UTF-8 names (same idiom as the daemon service).
    for name in names.iter().flatten().flatten() {
        if let Ok(worktree) = main_repo.find_worktree(name) {
            paths.push(worktree.path().to_path_buf());
        }
    }
    Ok(paths)
}

/// The main working-tree root of `repo`: the parent of its shared common dir. For a
/// linked worktree this is the original checkout every worktree shares.
pub(crate) fn main_root(repo: &Repository) -> PathBuf {
    let commondir = repo.commondir();
    let commondir = std::fs::canonicalize(commondir).unwrap_or_else(|_| commondir.to_path_buf());
    let parent = commondir.parent().map(Path::to_path_buf);
    parent.unwrap_or(commondir)
}

/// The checked-out branch shorthand and HEAD oid. Both are `None` for a detached or
/// unborn HEAD — which is exactly the "there is no branch to act on" case both
/// engines skip. (A worktree sitting mid-rebase has a *detached* HEAD, so this is
/// also what keeps a push away from one.)
pub(crate) fn head_branch(repo: &Repository) -> (Option<String>, Option<Oid>) {
    match repo.head() {
        Ok(head) if head.is_branch() => (
            head.shorthand().ok().map(ToString::to_string),
            head.target(),
        ),
        Ok(head) => (None, head.target()),
        Err(_) => (None, None),
    }
}

/// Runs `git <args>` in `dir`, capturing its output.
///
/// The child receives a snapshot of the current environment (`env_clear` + `envs`)
/// so the spawn stays out of the data race against concurrent `std::env::set_var`
/// (issue #1022; same idiom as `crate::cli::git::worktree`). Shelling out to the
/// user's `git` — rather than libgit2's network transport — is deliberate: it works
/// across SSH/HTTPS and honours the user's authentication configuration (ADR-0003,
/// issue #903).
///
/// That environment snapshot is also what makes the daemon host viable: under
/// launchd the daemon's own environment carries the per-user `SSH_AUTH_SOCK`, so
/// the child inherits the user's `ssh-agent` unchanged (ADR-0059). `git` itself is
/// passed in resolved, because that environment's `PATH` is minimal.
pub(crate) fn run_git_in(git: &Path, dir: &Path, args: &[&str]) -> Result<std::process::Output> {
    let mut cmd = Command::new(git);
    cmd.env_clear();
    cmd.envs(std::env::vars_os());
    cmd.current_dir(dir)
        .args(args)
        .output()
        .with_context(|| format!("failed to execute {} in {}", git.display(), dir.display()))
}

/// `skip_serializing_if` predicate for a `bool` defaulting to `false`, so the field
/// is dropped on the wire unless set — the protocol's forward-compatibility
/// convention (the twin of the daemon service's helper of the same name).
#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn is_false(b: &bool) -> bool {
    !*b
}

/// The trimmed stderr of a git subprocess (falling back to stdout when stderr is
/// empty), for a single-line error message.
pub(crate) fn trimmed_stderr(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// A process-wide lock serializing the git-subprocess-heavy tests, shared across
/// modules (the rebase and push engines' own tests, and the `worktrees
/// rebase`/`push` CLI tests).
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

    #[test]
    fn is_false_is_the_skip_predicate_for_a_defaulted_bool() {
        assert!(is_false(&false), "an unset flag is dropped from the wire");
        assert!(!is_false(&true), "a set flag is serialized");
    }

    #[test]
    fn trimmed_stderr_falls_back_to_stdout_when_stderr_is_empty() {
        let with_stderr = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b"out\n".to_vec(),
            stderr: b"  boom  \n".to_vec(),
        };
        assert_eq!(trimmed_stderr(&with_stderr), "boom");

        let stdout_only = std::process::Output {
            status: std::process::Command::new("true").status().unwrap(),
            stdout: b" fallback \n".to_vec(),
            stderr: b"  \n".to_vec(),
        };
        assert_eq!(
            trimmed_stderr(&stdout_only),
            "fallback",
            "a whitespace-only stderr must not shadow a real stdout message"
        );
    }
}
