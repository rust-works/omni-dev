//! Git remote operations

use anyhow::{Context, Result};
use git2::{BranchType, Repository};
use serde::{Deserialize, Serialize};

/// Remote repository information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    /// Name of the remote (e.g., "origin", "upstream")
    pub name: String,
    /// URI of the remote repository
    pub uri: String,
    /// Detected main branch name for this remote
    pub main_branch: String,
}

impl RemoteInfo {
    /// Get all remotes for a repository
    pub fn get_all_remotes(repo: &Repository) -> Result<Vec<Self>> {
        let mut remotes = Vec::new();
        let remote_names = repo.remotes().context("Failed to get remote names")?;

        for name in remote_names.iter().flatten() {
            if let Ok(remote) = repo.find_remote(name) {
                let uri = remote.url().unwrap_or("").to_string();
                let main_branch = Self::detect_main_branch(repo, name)?;

                remotes.push(RemoteInfo {
                    name: name.to_string(),
                    uri,
                    main_branch,
                });
            }
        }

        Ok(remotes)
    }

    /// Detect the main branch for a remote
    fn detect_main_branch(repo: &Repository, remote_name: &str) -> Result<String> {
        // First try to get the remote HEAD reference
        let head_ref_name = format!("refs/remotes/{}/HEAD", remote_name);
        if let Ok(head_ref) = repo.find_reference(&head_ref_name) {
            if let Some(target) = head_ref.symbolic_target() {
                // Extract branch name from refs/remotes/origin/main
                if let Some(branch_name) =
                    target.strip_prefix(&format!("refs/remotes/{}/", remote_name))
                {
                    return Ok(branch_name.to_string());
                }
            }
        }

        // Try using GitHub CLI for GitHub repositories
        if let Ok(remote) = repo.find_remote(remote_name) {
            if let Some(uri) = remote.url() {
                if uri.contains("github.com") {
                    if let Ok(main_branch) = Self::get_github_default_branch(uri) {
                        return Ok(main_branch);
                    }
                }
            }
        }

        // Fallback to checking common branch names, preferring origin remote
        let common_branches = ["main", "master", "develop"];

        // First, check if this is the origin remote or if origin remote branches exist
        if remote_name == "origin" {
            for branch_name in &common_branches {
                let reference_name = format!("refs/remotes/origin/{}", branch_name);
                if repo.find_reference(&reference_name).is_ok() {
                    return Ok(branch_name.to_string());
                }
            }
        } else {
            // For non-origin remotes, first check if origin has these branches
            for branch_name in &common_branches {
                let origin_reference = format!("refs/remotes/origin/{}", branch_name);
                if repo.find_reference(&origin_reference).is_ok() {
                    return Ok(branch_name.to_string());
                }
            }

            // Then check the actual remote
            for branch_name in &common_branches {
                let reference_name = format!("refs/remotes/{}/{}", remote_name, branch_name);
                if repo.find_reference(&reference_name).is_ok() {
                    return Ok(branch_name.to_string());
                }
            }
        }

        // If no common branch found, try to find any branch
        let branch_iter = repo.branches(Some(BranchType::Remote))?;
        for branch_result in branch_iter {
            let (branch, _) = branch_result?;
            if let Some(name) = branch.name()? {
                if name.starts_with(&format!("{}/", remote_name)) {
                    let branch_name = name
                        .strip_prefix(&format!("{}/", remote_name))
                        .unwrap_or(name);
                    return Ok(branch_name.to_string());
                }
            }
        }

        // If still no branch found, return "unknown"
        Ok("unknown".to_string())
    }

    /// Get the default branch from GitHub using gh CLI
    fn get_github_default_branch(uri: &str) -> Result<String> {
        use std::process::Command;

        // Extract repository name from URI
        let repo_name = Self::extract_github_repo_name(uri)?;

        // Use gh CLI to get default branch
        let output = Command::new("gh")
            .args([
                "repo",
                "view",
                &repo_name,
                "--json",
                "defaultBranchRef",
                "--jq",
                ".defaultBranchRef.name",
            ])
            .output();

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

    /// Extract GitHub repository name from URI
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
            anyhow::bail!("Not a GitHub URI: {}", uri);
        };

        if repo_name.split('/').count() != 2 {
            anyhow::bail!("Invalid GitHub repository format: {}", repo_name);
        }

        Ok(repo_name.to_string())
    }
}
