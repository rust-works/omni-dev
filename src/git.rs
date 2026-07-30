//! Git operations and repository management.

use std::path::{Path, PathBuf};

pub mod amendment;
pub mod commit;
pub mod diff_split;
pub mod main_branches;
pub mod remote;
pub mod repository;
pub mod worktree_batch;
pub mod worktree_rebase;

pub use amendment::AmendmentHandler;
pub use commit::{
    refine_message_scope, resolve_scope, CommitAnalysis, CommitAnalysisForAI, CommitInfo,
    CommitInfoForAI, FileDiffRef,
};
pub use diff_split::{split_by_file, split_file_by_hunk, FileDiff, HunkDiff};
pub use main_branches::{branches_containing, detect_main_branch_tips, MainBranchTip};
pub use remote::RemoteInfo;
pub use repository::GitRepository;

/// Number of hex characters to show in abbreviated commit hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Length of a full SHA-1 commit hash in hex characters.
pub const FULL_HASH_LEN: usize = 40;

/// Environment override for the `git` binary, for when a process runs under
/// launchd/systemd with a minimal `PATH`. The exact analogue of
/// `OMNI_DEV_GH_BIN` (`crate::pr_status`) and `OMNI_DEV_VSCODE_BIN` (the tray's
/// `code` launcher).
const GIT_BIN_ENV: &str = "OMNI_DEV_GIT_BIN";

/// Absolute paths probed for `git` when [`GIT_BIN_ENV`] is unset, in order.
///
/// The daemon cannot rely on `PATH`: launchd hands it
/// `/usr/bin:/bin:/usr/sbin:/sbin`. On macOS that *does* contain `/usr/bin/git`
/// (the Xcode command-line-tools shim), so the fallback would work — but it
/// would silently pick a different `git` than the user's shell does, which for
/// a history-rewriting operation is exactly the kind of divergence worth ruling
/// out. Homebrew first, therefore, matching [`GH_BINARY_CANDIDATES`] order.
///
/// [`GH_BINARY_CANDIDATES`]: crate::pr_status
const GIT_BINARY_CANDIDATES: &[&str] = &[
    "/opt/homebrew/bin/git",
    "/usr/local/bin/git",
    "/home/linuxbrew/.linuxbrew/bin/git",
    "/usr/bin/git",
];

/// Resolves `git`, preferring [`GIT_BIN_ENV`], then the first existing
/// well-known absolute path, then bare `git` on `PATH`.
///
/// This is the fix for the *real* obstacle to running git from the daemon
/// (ADR-0059). The obstacle was never credentials — launchd exports
/// `SSH_AUTH_SOCK` into the per-user session, so a LaunchAgent inherits the
/// user's `ssh-agent` — it was the minimal `PATH`, the same problem
/// [ADR-0049](../docs/adrs/adr-0049.md) §3 solves for the `code` launcher and
/// [`resolve_gh_binary`](crate::pr_status::resolve_gh_binary) solves for `gh`.
///
/// Callers should do this **once** and pass the result down, rather than
/// re-reading the environment per subprocess.
#[must_use]
pub fn resolve_git_binary() -> PathBuf {
    resolve_git_binary_from(std::env::var_os(GIT_BIN_ENV), GIT_BINARY_CANDIDATES)
}

/// The testable core of [`resolve_git_binary`]. Split so the probe order can be
/// unit-tested without mutating the process environment (#1030).
fn resolve_git_binary_from(
    env_override: Option<std::ffi::OsString>,
    candidates: &[&str],
) -> PathBuf {
    if let Some(path) = env_override.filter(|p| !p.is_empty()) {
        return PathBuf::from(path);
    }
    for candidate in candidates {
        let path = Path::new(candidate);
        if path.exists() {
            return path.to_path_buf();
        }
    }
    PathBuf::from("git")
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn resolve_git_binary_from_prefers_env_then_candidate_then_fallback() {
        assert_eq!(
            resolve_git_binary_from(Some("/custom/git".into()), &["/usr/bin/git"]),
            PathBuf::from("/custom/git"),
            "an explicit override wins over every candidate"
        );
        // A path guaranteed to exist on every platform the suite runs on.
        let existing = std::env::current_exe().unwrap();
        let existing = existing.to_str().unwrap();
        assert_eq!(
            resolve_git_binary_from(None, &["/no/such/git/xyzzy", existing]),
            PathBuf::from(existing),
            "the first *existing* candidate wins, not merely the first"
        );
        assert_eq!(
            resolve_git_binary_from(None, &["/no/such/git/xyzzy"]),
            PathBuf::from("git"),
            "with nothing found, fall back to a bare PATH lookup"
        );
        assert_eq!(
            resolve_git_binary_from(Some(String::new().into()), &["/no/such/git/xyzzy"]),
            PathBuf::from("git"),
            "an empty override is ignored rather than spawning \"\""
        );
    }

    #[test]
    fn resolve_git_binary_reads_the_real_environment() {
        // Smoke: the public wrapper must not panic on whatever this machine has.
        let _ = resolve_git_binary();
    }
}
