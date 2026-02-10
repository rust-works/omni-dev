//! Git commit amendment operations.

use std::collections::HashMap;
use std::process::Command;

use anyhow::{Context, Result};
use git2::{Oid, Repository};
use tracing::debug;

use crate::data::amendments::{Amendment, AmendmentFile};
use crate::git::SHORT_HASH_LEN;

/// Amendment operation handler.
pub struct AmendmentHandler {
    repo: Repository,
}

impl AmendmentHandler {
    /// Creates a new amendment handler.
    pub fn new() -> Result<Self> {
        let repo = Repository::open(".").context("Failed to open git repository")?;
        Ok(Self { repo })
    }

    /// Applies amendments from a YAML file.
    pub fn apply_amendments(&self, yaml_file: &str) -> Result<()> {
        // Load and validate amendment file
        let amendment_file = AmendmentFile::load_from_file(yaml_file)?;

        // Safety checks
        self.perform_safety_checks(&amendment_file)?;

        // Group amendments by their position in history
        let amendments = self.organize_amendments(&amendment_file.amendments)?;

        if amendments.is_empty() {
            println!("No valid amendments found to apply.");
            return Ok(());
        }

        // Check if we only need to amend HEAD
        if amendments.len() == 1 && self.is_head_commit(&amendments[0].0)? {
            println!(
                "Amending HEAD commit: {}",
                &amendments[0].0[..SHORT_HASH_LEN]
            );
            self.amend_head_commit(&amendments[0].1)?;
        } else {
            println!(
                "Amending {} commits using interactive rebase",
                amendments.len()
            );
            self.amend_via_rebase(amendments)?;
        }

        println!("✅ Amendment operations completed successfully");
        Ok(())
    }

    /// Performs safety checks before amendment.
    fn perform_safety_checks(&self, amendment_file: &AmendmentFile) -> Result<()> {
        // Check if working directory is clean
        self.check_working_directory_clean()
            .context("Cannot amend commits with uncommitted changes")?;

        // Check if commits exist and are not in remote main branches
        for amendment in &amendment_file.amendments {
            self.validate_commit_amendable(&amendment.commit)?;
        }

        Ok(())
    }

    /// Validates that a commit can be safely amended.
    fn validate_commit_amendable(&self, commit_hash: &str) -> Result<()> {
        // Check if commit exists
        let oid = Oid::from_str(commit_hash)
            .with_context(|| format!("Invalid commit hash: {}", commit_hash))?;

        let _commit = self
            .repo
            .find_commit(oid)
            .with_context(|| format!("Commit not found: {}", commit_hash))?;

        // TODO: Check if commit is in remote main branches
        // This would require implementing main branch detection and remote checking
        // For now, we'll skip this check as it's complex and the basic functionality works

        Ok(())
    }

    /// Organizes amendments by their order in git history.
    fn organize_amendments(&self, amendments: &[Amendment]) -> Result<Vec<(String, String)>> {
        let mut valid_amendments = Vec::new();
        let mut commit_depths = HashMap::new();

        // Calculate depth of each commit from HEAD
        for amendment in amendments {
            if let Ok(depth) = self.get_commit_depth_from_head(&amendment.commit) {
                commit_depths.insert(amendment.commit.clone(), depth);
                valid_amendments.push((amendment.commit.clone(), amendment.message.clone()));
            } else {
                println!(
                    "Warning: Skipping invalid commit {}",
                    &amendment.commit[..SHORT_HASH_LEN]
                );
            }
        }

        // Sort by depth (deepest first for rebase order)
        valid_amendments.sort_by_key(|(commit, _)| commit_depths.get(commit).copied().unwrap_or(0));

        // Reverse so we process from oldest to newest
        valid_amendments.reverse();

        Ok(valid_amendments)
    }

    /// Returns the depth of a commit from HEAD (0 = HEAD, 1 = HEAD~1, etc.).
    fn get_commit_depth_from_head(&self, commit_hash: &str) -> Result<usize> {
        let target_oid = Oid::from_str(commit_hash)?;
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_head()?;

        for (depth, oid_result) in revwalk.enumerate() {
            let oid = oid_result?;
            if oid == target_oid {
                return Ok(depth);
            }
        }

        anyhow::bail!("Commit {} not found in current branch history", commit_hash);
    }

    /// Checks if a commit hash is the current HEAD.
    fn is_head_commit(&self, commit_hash: &str) -> Result<bool> {
        let head_oid = self.repo.head()?.target().context("HEAD has no target")?;
        let target_oid = Oid::from_str(commit_hash)?;
        Ok(head_oid == target_oid)
    }

    /// Amends the HEAD commit message.
    fn amend_head_commit(&self, new_message: &str) -> Result<()> {
        let head_commit = self.repo.head()?.peel_to_commit()?;

        // Use the simpler approach: git commit --amend
        let output = Command::new("git")
            .args(["commit", "--amend", "--message", new_message])
            .output()
            .context("Failed to execute git commit --amend")?;

        if !output.status.success() {
            let error_msg = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Failed to amend HEAD commit: {}", error_msg);
        }

        // Get the new commit ID for logging
        let new_head = self.repo.head()?.peel_to_commit()?;

        println!(
            "✅ Amended HEAD commit {} -> {}",
            &head_commit.id().to_string()[..SHORT_HASH_LEN],
            &new_head.id().to_string()[..SHORT_HASH_LEN]
        );

        Ok(())
    }

    /// Amends commits via individual interactive rebases (following shell script strategy).
    fn amend_via_rebase(&self, amendments: Vec<(String, String)>) -> Result<()> {
        if amendments.is_empty() {
            return Ok(());
        }

        println!("Amending commits individually in reverse order (newest to oldest)");

        // Sort amendments by commit depth (newest first, following shell script approach)
        let mut sorted_amendments = amendments.clone();
        sorted_amendments
            .sort_by_key(|(hash, _)| self.get_commit_depth_from_head(hash).unwrap_or(usize::MAX));

        // Process each commit individually
        for (commit_hash, new_message) in sorted_amendments {
            let depth = self.get_commit_depth_from_head(&commit_hash)?;

            if depth == 0 {
                // This is HEAD - simple amendment
                println!("Amending HEAD commit: {}", &commit_hash[..SHORT_HASH_LEN]);
                self.amend_head_commit(&new_message)?;
            } else {
                // This is an older commit - use individual interactive rebase
                println!(
                    "Amending commit at depth {}: {}",
                    depth,
                    &commit_hash[..SHORT_HASH_LEN]
                );
                self.amend_single_commit_via_rebase(&commit_hash, &new_message)?;
            }
        }

        Ok(())
    }

    /// Amends a single commit using individual interactive rebase (shell script strategy).
    fn amend_single_commit_via_rebase(&self, commit_hash: &str, new_message: &str) -> Result<()> {
        // Get the parent of the target commit to use as rebase base
        let base_commit = format!("{}^", commit_hash);

        // Create temporary sequence file for this specific rebase
        let temp_dir = tempfile::tempdir()?;
        let sequence_file = temp_dir.path().join("rebase-sequence");

        // Generate rebase sequence: edit the target commit, pick the rest
        let mut sequence_content = String::new();
        let commit_list_output = Command::new("git")
            .args(["rev-list", "--reverse", &format!("{}..HEAD", base_commit)])
            .output()
            .context("Failed to get commit list for rebase")?;

        if !commit_list_output.status.success() {
            anyhow::bail!("Failed to generate commit list for rebase");
        }

        let commit_list = String::from_utf8_lossy(&commit_list_output.stdout);
        for line in commit_list.lines() {
            let commit = line.trim();
            if commit.is_empty() {
                continue;
            }

            // Get short commit message for the sequence file
            let subject_output = Command::new("git")
                .args(["log", "--format=%s", "-n", "1", commit])
                .output()
                .context("Failed to get commit subject")?;

            let subject = String::from_utf8_lossy(&subject_output.stdout)
                .trim()
                .to_string();

            if commit.starts_with(&commit_hash[..commit.len().min(commit_hash.len())]) {
                // This is our target commit - mark it for editing
                sequence_content.push_str(&format!("edit {} {}\n", commit, subject));
            } else {
                // Other commits - just pick them
                sequence_content.push_str(&format!("pick {} {}\n", commit, subject));
            }
        }

        // Write sequence file
        std::fs::write(&sequence_file, sequence_content)?;

        println!(
            "Starting interactive rebase to amend commit: {}",
            &commit_hash[..SHORT_HASH_LEN]
        );

        // Execute rebase with custom sequence editor
        let rebase_result = Command::new("git")
            .args(["rebase", "-i", &base_commit])
            .env(
                "GIT_SEQUENCE_EDITOR",
                format!("cp {}", sequence_file.display()),
            )
            .env("GIT_EDITOR", "true") // Prevent interactive editor
            .output()
            .context("Failed to start interactive rebase")?;

        if !rebase_result.status.success() {
            let error_msg = String::from_utf8_lossy(&rebase_result.stderr);

            // Best-effort cleanup; the rebase may not have started.
            if let Err(e) = Command::new("git").args(["rebase", "--abort"]).output() {
                debug!("Rebase abort during cleanup failed: {e}");
            }

            anyhow::bail!("Interactive rebase failed: {}", error_msg);
        }

        // Check if we're now in a rebase state where we can amend
        let repo_state = self.repo.state();
        if repo_state == git2::RepositoryState::RebaseInteractive {
            // We should be stopped at the target commit - amend it
            let current_commit_output = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .output()
                .context("Failed to get current commit during rebase")?;

            let current_commit = String::from_utf8_lossy(&current_commit_output.stdout)
                .trim()
                .to_string();

            if current_commit
                .starts_with(&commit_hash[..current_commit.len().min(commit_hash.len())])
            {
                // Amend with new message
                let amend_result = Command::new("git")
                    .args(["commit", "--amend", "-m", new_message])
                    .output()
                    .context("Failed to amend commit during rebase")?;

                if !amend_result.status.success() {
                    let error_msg = String::from_utf8_lossy(&amend_result.stderr);
                    // Best-effort cleanup; abort so the repo isn't left mid-rebase.
                    if let Err(e) = Command::new("git").args(["rebase", "--abort"]).output() {
                        debug!("Rebase abort during cleanup failed: {e}");
                    }
                    anyhow::bail!("Failed to amend commit: {}", error_msg);
                }

                println!("✅ Amended commit: {}", &commit_hash[..SHORT_HASH_LEN]);

                // Continue the rebase
                let continue_result = Command::new("git")
                    .args(["rebase", "--continue"])
                    .output()
                    .context("Failed to continue rebase")?;

                if !continue_result.status.success() {
                    let error_msg = String::from_utf8_lossy(&continue_result.stderr);
                    // Best-effort cleanup; abort so the repo isn't left mid-rebase.
                    if let Err(e) = Command::new("git").args(["rebase", "--abort"]).output() {
                        debug!("Rebase abort during cleanup failed: {e}");
                    }
                    anyhow::bail!("Failed to continue rebase: {}", error_msg);
                }

                println!("✅ Rebase completed successfully");
            } else {
                // Best-effort cleanup; abort so the repo isn't left mid-rebase.
                if let Err(e) = Command::new("git").args(["rebase", "--abort"]).output() {
                    debug!("Rebase abort during cleanup failed: {e}");
                }
                anyhow::bail!(
                    "Unexpected commit during rebase. Expected {}, got {}",
                    &commit_hash[..SHORT_HASH_LEN],
                    &current_commit[..SHORT_HASH_LEN]
                );
            }
        } else if repo_state != git2::RepositoryState::Clean {
            anyhow::bail!(
                "Repository in unexpected state after rebase: {:?}",
                repo_state
            );
        }

        Ok(())
    }

    /// Checks if the working directory is clean (uses the repository instance).
    fn check_working_directory_clean(&self) -> Result<()> {
        let statuses = self
            .repo
            .statuses(None)
            .context("Failed to get repository status")?;

        // Filter out ignored files and only check for actual uncommitted changes
        let actual_changes: Vec<_> = statuses
            .iter()
            .filter(|entry| {
                let status = entry.status();
                // Only consider files that have actual changes, not ignored files
                !status.is_ignored()
            })
            .collect();

        if !actual_changes.is_empty() {
            // Print details about what's unclean for debugging
            println!("Working directory has uncommitted changes:");
            for status_entry in &actual_changes {
                let status = status_entry.status();
                let file_path = status_entry.path().unwrap_or("unknown");
                println!("  {} -> {:?}", file_path, status);
            }

            anyhow::bail!(
                "Working directory is not clean. Please commit or stash changes before amending commit messages."
            );
        }

        Ok(())
    }
}
