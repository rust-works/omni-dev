//! Git remote operations

use anyhow::{Context, Result};
use git2::{BranchType, Repository};
use serde::{Deserialize, Serialize};

/// Remote repository information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteInfo {
    /// Name of the remote (e.g., "origin", "upstream")
    pub name: String,
    /// URL of the remote repository
    pub url: String,
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
                let url = remote.url().unwrap_or("").to_string();
                let main_branch = Self::detect_main_branch(repo, name)?;

                remotes.push(RemoteInfo {
                    name: name.to_string(),
                    url,
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
}
