//! Git remote operations.

use anyhow::{Context, Result};
use git2::{BranchType, Repository};
use serde::{Deserialize, Serialize};

/// Remote repository information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    /// Name of the remote (e.g., "origin", "upstream").
    pub name: String,
    /// URI of the remote repository.
    pub uri: String,
    /// Detected main branch name for this remote.
    pub main_branch: String,
}

impl RemoteInfo {
    /// Returns all remotes for a repository.
    pub fn get_all_remotes(repo: &Repository) -> Result<Vec<Self>> {
        let mut remotes = Vec::new();
        let remote_names = repo.remotes().context("Failed to get remote names")?;

        // Anchor the `gh` subprocess fallback to the repository's working
        // directory (RULE-1: git2 ops keep `&Repository`; the subprocess takes
        // `repo_root`). Derived from the git2 repo itself, so no external caller
        // signature changes.
        let repo_root = repo.workdir().unwrap_or_else(|| repo.path());

        for name in remote_names.iter().flatten().flatten() {
            if let Ok(remote) = repo.find_remote(name) {
                let uri = remote.url().unwrap_or("").to_string();
                let main_branch = Self::detect_main_branch(repo, name, repo_root)?;

                remotes.push(Self {
                    name: name.to_string(),
                    uri,
                    main_branch,
                });
            }
        }

        Ok(remotes)
    }

    /// Detects the main branch for a remote.
    ///
    /// `repo_root` anchors the `gh` subprocess fallback to the repository's
    /// working directory rather than the process current working directory.
    fn detect_main_branch(
        repo: &Repository,
        remote_name: &str,
        repo_root: &std::path::Path,
    ) -> Result<String> {
        // First try to get the remote HEAD reference
        if let Some(branch_name) = Self::main_branch_from_remote_head(repo, remote_name) {
            return Ok(branch_name);
        }

        // Try using GitHub CLI for GitHub repositories
        if let Ok(remote) = repo.find_remote(remote_name) {
            if let Ok(uri) = remote.url() {
                if uri.contains("github.com") {
                    if let Ok(main_branch) = Self::get_github_default_branch(uri, repo_root) {
                        return Ok(main_branch);
                    }
                }
            }
        }

        // Fallback to checking common branch names, preferring origin remote
        if let Some(branch_name) = Self::main_branch_from_common_names(repo, remote_name) {
            return Ok(branch_name);
        }

        // If no common branch found, try to find any branch
        let branch_iter = repo.branches(Some(BranchType::Remote))?;
        for branch_result in branch_iter {
            let (branch, _) = branch_result?;
            if let Some(name) = branch.name()? {
                if name.starts_with(&format!("{remote_name}/")) {
                    let branch_name = name
                        .strip_prefix(&format!("{remote_name}/"))
                        .unwrap_or(name);
                    return Ok(branch_name.to_string());
                }
            }
        }

        // If still no branch found, return "unknown"
        Ok("unknown".to_string())
    }

    /// Detects the main branch for a remote using only local refs — no network,
    /// no `gh` subprocess.
    ///
    /// Returns `None` when neither the remote's symbolic HEAD nor a common
    /// branch name (`main`/`master`/`develop`) resolves. Callers that treat the
    /// result as a safety signal must stay conservative on `None`; unlike
    /// [`Self::detect_main_branch`], this never falls back to an arbitrary
    /// remote branch.
    pub(crate) fn detect_main_branch_local(repo: &Repository, remote_name: &str) -> Option<String> {
        Self::main_branch_from_remote_head(repo, remote_name)
            .or_else(|| Self::main_branch_from_common_names(repo, remote_name))
    }

    /// Returns the branch the remote's symbolic `HEAD` reference points at.
    fn main_branch_from_remote_head(repo: &Repository, remote_name: &str) -> Option<String> {
        let head_ref_name = format!("refs/remotes/{remote_name}/HEAD");
        let head_ref = repo.find_reference(&head_ref_name).ok()?;
        let target = head_ref.symbolic_target().ok()??;
        // Extract branch name from refs/remotes/origin/main
        target
            .strip_prefix(&format!("refs/remotes/{remote_name}/"))
            .map(ToString::to_string)
    }

    /// Returns the first common branch name (`main`/`master`/`develop`) that
    /// exists as a remote-tracking ref, preferring the origin remote.
    fn main_branch_from_common_names(repo: &Repository, remote_name: &str) -> Option<String> {
        let common_branches = ["main", "master", "develop"];

        // First, check if this is the origin remote or if origin remote branches exist
        if remote_name == "origin" {
            for branch_name in &common_branches {
                let reference_name = format!("refs/remotes/origin/{branch_name}");
                if repo.find_reference(&reference_name).is_ok() {
                    return Some((*branch_name).to_string());
                }
            }
        } else {
            // For non-origin remotes, first check if origin has these branches
            for branch_name in &common_branches {
                let origin_reference = format!("refs/remotes/origin/{branch_name}");
                if repo.find_reference(&origin_reference).is_ok() {
                    return Some((*branch_name).to_string());
                }
            }

            // Then check the actual remote
            for branch_name in &common_branches {
                let reference_name = format!("refs/remotes/{remote_name}/{branch_name}");
                if repo.find_reference(&reference_name).is_ok() {
                    return Some((*branch_name).to_string());
                }
            }
        }

        None
    }

    /// Returns the default branch from GitHub using gh CLI.
    ///
    /// `repo_root` anchors the `gh` subprocess to the repository's working
    /// directory. The `gh repo view <repo_name>` invocation already passes an
    /// explicit repo argument (so it is CWD-inert), but the directory is pinned
    /// for uniformity with the rest of the repo-anchored subprocess seams.
    fn get_github_default_branch(uri: &str, repo_root: &std::path::Path) -> Result<String> {
        // Extract repository name from URI
        let repo_name = Self::extract_github_repo_name(uri)?;

        // Use gh CLI to get default branch, via the metrics choke point (#1387).
        let output = crate::github_metrics::run_gh(
            &crate::pr_status::resolve_gh_binary(),
            [
                "repo",
                "view",
                repo_name.as_str(),
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name",
            ],
            "repo view",
            Some(repo_root),
        );

        match output {
            Ok(output) if output.status.success() => {
                let branch_name = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !branch_name.is_empty() && branch_name != "null" {
                    Ok(branch_name)
                } else {
                    anyhow::bail!("GitHub CLI returned empty or null branch name")
                }
            }
            _ => anyhow::bail!("Failed to get default branch from GitHub CLI"),
        }
    }

    /// Extracts GitHub repository name from URI.
    fn extract_github_repo_name(uri: &str) -> Result<String> {
        // Handle both SSH and HTTPS GitHub URIs
        let repo_name = if uri.starts_with("git@github.com:") {
            // SSH format: git@github.com:owner/repo.git
            uri.strip_prefix("git@github.com:")
                .and_then(|s| s.strip_suffix(".git"))
                .unwrap_or(uri.strip_prefix("git@github.com:").unwrap_or(uri))
        } else if uri.contains("github.com") {
            // HTTPS format: https://github.com/owner/repo.git
            uri.split("github.com/")
                .nth(1)
                .and_then(|s| s.strip_suffix(".git"))
                .unwrap_or(uri.split("github.com/").nth(1).unwrap_or(uri))
        } else {
            anyhow::bail!("Not a GitHub URI: {uri}");
        };

        if repo_name.split('/').count() != 2 {
            anyhow::bail!("Invalid GitHub repository format: {repo_name}");
        }

        Ok(repo_name.to_string())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── extract_github_repo_name ─────────────────────────────────────

    #[test]
    fn ssh_url() {
        let result = RemoteInfo::extract_github_repo_name("git@github.com:owner/repo.git");
        assert_eq!(result.unwrap(), "owner/repo");
    }

    #[test]
    fn https_url() {
        let result = RemoteInfo::extract_github_repo_name("https://github.com/owner/repo.git");
        assert_eq!(result.unwrap(), "owner/repo");
    }

    #[test]
    fn https_url_no_git_suffix() {
        let result = RemoteInfo::extract_github_repo_name("https://github.com/owner/repo");
        assert_eq!(result.unwrap(), "owner/repo");
    }

    #[test]
    fn ssh_url_no_git_suffix() {
        let result = RemoteInfo::extract_github_repo_name("git@github.com:owner/repo");
        assert_eq!(result.unwrap(), "owner/repo");
    }

    #[test]
    fn non_github_url_fails() {
        let result = RemoteInfo::extract_github_repo_name("git@gitlab.com:owner/repo.git");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Not a GitHub URI"));
    }

    #[test]
    fn invalid_format_fails() {
        let result = RemoteInfo::extract_github_repo_name("git@github.com:invalid");
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid GitHub repository format"));
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn ssh_url_extracts_repo(
                owner in "[a-z]{3,10}",
                repo in "[a-z]{3,10}",
            ) {
                let url = format!("git@github.com:{owner}/{repo}.git");
                let result = RemoteInfo::extract_github_repo_name(&url).unwrap();
                prop_assert_eq!(result, format!("{owner}/{repo}"));
            }

            #[test]
            fn https_url_extracts_repo(
                owner in "[a-z]{3,10}",
                repo in "[a-z]{3,10}",
            ) {
                let url = format!("https://github.com/{owner}/{repo}.git");
                let result = RemoteInfo::extract_github_repo_name(&url).unwrap();
                prop_assert_eq!(result, format!("{owner}/{repo}"));
            }

            #[test]
            fn non_github_url_errors(
                host in "(gitlab|bitbucket|codeberg)",
                path in "[a-z]{3,10}/[a-z]{3,10}",
            ) {
                let url = format!("git@{host}.com:{path}.git");
                prop_assert!(RemoteInfo::extract_github_repo_name(&url).is_err());
            }
        }
    }

    // ── detect_main_branch_local ─────────────────────────────────────

    /// Creates a repo with one commit, anchored at `$CARGO_MANIFEST_DIR/tmp`,
    /// and returns the commit id for wiring up remote-tracking refs.
    fn repo_with_commit() -> (tempfile::TempDir, Repository, git2::Oid) {
        let tmp_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tmp");
        std::fs::create_dir_all(&tmp_root).unwrap();
        let temp_dir = tempfile::tempdir_in(&tmp_root).unwrap();
        let repo = Repository::init(temp_dir.path()).unwrap();
        let oid = {
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = repo.index().unwrap().write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
                .unwrap()
        };
        (temp_dir, repo, oid)
    }

    #[test]
    fn local_detection_prefers_symbolic_head() {
        let (_dir, repo, oid) = repo_with_commit();
        repo.reference("refs/remotes/origin/trunk", oid, false, "test")
            .unwrap();
        repo.reference_symbolic(
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/trunk",
            false,
            "test",
        )
        .unwrap();
        let result = RemoteInfo::detect_main_branch_local(&repo, "origin");
        assert_eq!(result.as_deref(), Some("trunk"));
    }

    #[test]
    fn local_detection_falls_back_to_common_names() {
        let (_dir, repo, oid) = repo_with_commit();
        repo.reference("refs/remotes/origin/master", oid, false, "test")
            .unwrap();
        let result = RemoteInfo::detect_main_branch_local(&repo, "origin");
        assert_eq!(result.as_deref(), Some("master"));
    }

    #[test]
    fn local_detection_returns_none_for_uncommon_branches() {
        // Unlike `detect_main_branch`, the local variant must not fall back to
        // an arbitrary remote branch: `None` keeps safety consumers conservative.
        let (_dir, repo, oid) = repo_with_commit();
        repo.reference("refs/remotes/origin/exotic", oid, false, "test")
            .unwrap();
        assert_eq!(RemoteInfo::detect_main_branch_local(&repo, "origin"), None);
    }
}
